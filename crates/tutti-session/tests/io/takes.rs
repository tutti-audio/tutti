//! Takes (design 030 section 5), rendered with no device: a session on a
//! `ManualStreamDriver`, its input fed by the test's own capture in lockstep
//! with the blocks it renders, so the sample the graph plays at frame `F` is
//! the one the test pushed at index `F`.

use std::any::Any;
use std::error::Error as _;
use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;
use tutti_rt::Mutex;

use tutti_buffers::ChannelLayout;
use tutti_buffers::{Interleaved, Sink, SinkFailed};
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_cpal::MicConfig;
use tutti_engine::{ManualStream, ManualStreamDriver, OutputSpec};
use tutti_graph::{InletRing, TapWriter};
use tutti_nodes::MetronomeMode;
use tutti_session::{
    Armed, Input, InputConfig, InputOpener, LiveSession, MainOutput, OpenedInput, OutputPort,
    Punch, Record, Recorded, Session, SessionEvent, Start, Take, TakeEnd, TakeError, TakeEvent,
    TakeFile, TakeTarget, Trim,
};
use tutti_units::{Amplitude, At, Beat, Frame, SampleRate, Samples};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;

/// The capture's writer, shared with the opener that made it.
#[derive(Default)]
struct Capture {
    writer: Option<TapWriter>,
    /// What the opener declares as the ring's lead.
    lead: Samples,
    /// The cell the capture callback would report its delay into.
    latency: Option<Arc<tutti_engine::StreamLatency>>,
}

#[derive(Debug)]
struct Opener(Arc<Mutex<Capture>>);

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Capture")
    }
}

impl InputOpener for Opener {
    fn open(&mut self, config: &MicConfig, rate: SampleRate) -> tutti_cpal::Result<OpenedInput> {
        let mut capture = self.0.lock();
        let (ring, writer) = InletRing::pair(ChannelLayout::STEREO, Samples::new(1 << 16), rate);
        capture.writer = Some(writer);
        capture.latency = config.latency.clone();
        Ok(OpenedInput {
            ring,
            keep: Box::new(()) as Box<dyn Any + Send>,
            lead: capture.lead,
        })
    }
}

/// The test's opener captures at one width and refuses the other, as a capture device
/// refuses a format it cannot give.
impl InputOpener<f64> for Opener {
    fn open(&mut self, _: &MicConfig, _: SampleRate) -> tutti_cpal::Result<OpenedInput<f64>> {
        Err(tutti_cpal::DeviceError::InvalidConfig(
            "this capture serves one sample width only".into(),
        ))
    }
}

struct Rig {
    session: LiveSession,
    stream: ManualStream,
    capture: Arc<Mutex<Capture>>,
    input: Input,
    /// Frames pushed so far: the next sample's index, and, in lockstep, the
    /// graph frame it is played at.
    pushed: usize,
    events: Vec<SessionEvent>,
    /// The playback delay each rendered block reports; `None`: none.
    delay: Option<Duration>,
    /// The value of frame `i` on both channels, to push instead of the ramp.
    signal: Option<fn(usize) -> f32>,
}

fn rig(config: InputConfig, lead: Samples) -> Rig {
    let capture = Arc::new(Mutex::new(Capture {
        writer: None,
        lead,
        latency: None,
    }));
    let (driver, stream) = ManualStreamDriver::new();
    let spec = OutputSpec::new(
        SampleRate::of(RATE),
        ChannelLayout::STEREO,
        tutti_buffers::SampleFormat::F32,
    );
    let live = Session::builder()
        .input_opener(Opener(Arc::clone(&capture)))
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec, driver)
        .expect("builds with no device");
    let mut session = live;
    let input = session.add_input(config).expect("opens");
    Rig {
        session,
        stream,
        capture,
        input,
        pushed: 0,
        events: Vec::new(),
        delay: None,
        signal: None,
    }
}

impl Rig {
    /// Pushes `frames` frames of the ramp (the sample at index `i` is `i`,
    /// the right channel its negative), renders them as one block and
    /// updates, collecting the events. Returns the block the session rendered.
    fn step(&mut self, frames: usize) -> Vec<f32> {
        let ramp: Vec<f32> = (self.pushed..self.pushed + frames)
            .flat_map(|i| match self.signal {
                Some(signal) => [signal(i); 2],
                None => [i as f32, -(i as f32)],
            })
            .collect();
        self.capture
            .lock()
            .writer
            .as_mut()
            .expect("opened")
            .push_interleaved(Interleaved::new(&ramp, ChannelLayout::STEREO).unwrap());
        self.pushed += frames;
        self.events.extend(self.session.update());
        // A callback's timestamp reports the playback delay each block.
        let rendered = match self.delay {
            Some(delay) => self
                .stream
                .render_block_delayed(Samples::new(frames), delay),
            None => self.stream.render_block(Samples::new(frames)),
        }
        .expect("the stream is open");
        self.events.extend(self.session.update());
        rendered
    }

    /// Steps blocks until `done` holds of the events so far, giving the pump
    /// thread its time.
    fn run_until(&mut self, done: impl Fn(&[SessionEvent]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(&self.events) {
            assert!(
                Instant::now() < deadline,
                "never happened: {:?}",
                self.events
            );
            self.step(BLOCK);
            std::thread::yield_now();
        }
    }

    /// Steps blocks until `done` holds of the events so far, for at most a count of
    /// blocks that only guards a hang: the pump's task is woken by the bells' tick
    /// thread, which no count of updates orders against. The pump runs inside the
    /// updates, on this thread.
    fn steps_until(&mut self, done: impl Fn(&[SessionEvent]) -> bool) {
        for _ in 0..1_000_000 {
            if done(&self.events) {
                return;
            }
            self.step(BLOCK);
            std::thread::yield_now();
        }
        panic!("never happened: {:?}", self.events);
    }

    fn takes(&self) -> Vec<&TakeEvent> {
        self.events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Take(t) => Some(t),
                _ => None,
            })
            .collect()
    }
}

fn started(events: &[SessionEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, SessionEvent::Take(TakeEvent::Started { .. })))
}

fn finished(events: &[SessionEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            SessionEvent::Take(TakeEvent::Finished { .. } | TakeEvent::Failed { .. })
        )
    })
}

fn wav(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(name)
}

fn read(path: &std::path::Path) -> Vec<[f32; 2]> {
    let mut reader = hound::WavReader::open(path).expect("a readable wav");
    assert_eq!(reader.spec().channels, 2);
    let header_frames = reader.duration() as usize;
    let samples: Vec<f32> = reader.samples::<f32>().map(Result::unwrap).collect();
    assert_eq!(
        samples.len() / 2,
        header_frames,
        "the header's data length matches the frames in the file"
    );
    samples.chunks(2).map(|f| [f[0], f[1]]).collect()
}

fn recorded(events: &[SessionEvent]) -> &Recorded {
    events
        .iter()
        .find_map(|e| match e {
            SessionEvent::Take(TakeEvent::Finished { recorded, .. }) => Some(recorded),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Finished in {events:?}"))
}

/// Rolls the transport and renders a couple of blocks, so the playhead is
/// rolling.
fn roll(rig: &mut Rig) {
    rig.session.play().expect("queued");
    rig.step(64);
    rig.step(64);
    assert!(rig.session.playhead().playing());
}

/// **A punch from beat a to beat b records exactly the frames of
/// [F(a), F(b)), and the file's first sample is the graph's sample at F(a).**
/// At 120 bpm and 48 kHz half a beat is 12 000 frames: beats 0.5 to 1.0 are
/// the span [12 000, 24 000) of the transport's frames. The sample the graph
/// plays at frame F is the one the test pushed at index F, so the file starts
/// at the ramp's value for the punch-in frame and counts up by one.
#[test]
fn a_beat_punch_records_exactly_the_frames_between_its_beats() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    roll(&mut rig);
    let punch = Punch::new(Beat::new(0.5)).until(Beat::new(1.0)).unwrap();
    let take = rig
        .session
        .record(Record::new(&rig.input, TakeFile::wav(wav(&dir, "a.wav"))).punch(punch))
        .expect("armed");
    let window = take.window();
    let end = window.end.expect("a punch-out");
    assert_eq!(end.since(window.start), Some(Samples::new(12_000)));

    rig.run_until(finished);
    let done = recorded(&rig.events);
    let first = done.placement().first_frame();
    assert_eq!(first, window.start, "the punch-in was in the future");
    assert_eq!(done.frames(), Samples::new(12_000));
    assert_eq!(done.lost(), Samples::new(0));
    let samples = read(done.path().expect("a file"));
    assert_eq!(samples.len(), 12_000, "exactly the frames of [F(a), F(b))");
    assert_eq!(
        samples[0][0],
        first.get() as f32,
        "the graph's sample at F(a)"
    );
    assert_eq!(samples[0][1], -(first.get() as f32));
    assert_eq!(
        samples[11_999][0],
        (end.get() - 1) as f32,
        "up to F(b), not past"
    );
    drop(take);
}

/// **A recording-only click sounds from the take's punch-in to its punch-out
/// and is silent outside.** At 120 bpm and 48 kHz a beat is 24 000 frames: a
/// punch over beats 1 to 3 is the frames [24 000, 72 000). The click, routed to
/// the main output, clicks on the beats the transport recording flag covers:
/// beats 1 and 2, each heard from the frame after its beat (its first sample is
/// `sin(0)`). Beat 0 is before the punch-in and beat 3 is the punch-out's own
/// frame, so the output is exactly zero outside [24 001, 72 000).
#[test]
fn a_recording_only_click_sounds_from_punch_in_to_punch_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.session
        .set((
            rig.session.click().mode().to(MetronomeMode::RecordingOnly),
            rig.session.click().volume().to(Amplitude::new(1.0)),
        ))
        .unwrap();
    let click = rig
        .session
        .click_node()
        .expect("the builder inserts the click");
    for port in 0..2 {
        rig.session
            .graph()
            .set_sources(MainOutput::channel(port), [OutputPort::new(click, port)])
            .unwrap()
            .unfaded();
    }
    rig.session.play().expect("queued");
    let mut left: Vec<f32> = Vec::new();
    let collect = |rig: &mut Rig, left: &mut Vec<f32>, frames: usize| {
        left.extend(rig.step(frames).chunks(2).map(|frame| frame[0]));
    };
    collect(&mut rig, &mut left, 64);
    collect(&mut rig, &mut left, 64);
    let punch = Punch::new(Beat::new(1.0)).until(Beat::new(3.0)).unwrap();
    let take = rig
        .session
        .record(Record::new(&rig.input, TakeFile::wav(wav(&dir, "click.wav"))).punch(punch))
        .expect("armed");
    let window = take.window();
    assert_eq!(window.start, Frame::new(24_000));
    assert_eq!(window.end, Some(Frame::new(72_000)));
    while left.len() < 5 * 24_000 {
        collect(&mut rig, &mut left, BLOCK);
    }

    let sounding: Vec<usize> = (0..left.len()).filter(|&f| left[f] != 0.0).collect();
    assert!(
        sounding.iter().all(|f| (24_001..72_000).contains(f)),
        "silent outside the punch: first {:?}, last {:?}",
        sounding.first(),
        sounding.last()
    );
    for beat in [1usize, 2] {
        let from = beat * 24_000;
        assert!(
            left[from..from + 1_440].iter().any(|&x| x != 0.0),
            "beat {beat} clicks inside the punch"
        );
    }
    for beat in [0usize, 3, 4] {
        let from = beat * 24_000;
        assert!(
            left[from..from + 1_440].iter().all(|&x| x == 0.0),
            "beat {beat} is outside the punch"
        );
    }
    drop(take);
}

/// **A plugin is told the host records from a take's punch-in to its
/// punch-out, and not outside.** The shared plugin logs each transport the host
/// sends it, before it reads anything from it, with the beat it holds at its
/// frame: the host sends one with every block and another at a frame where the
/// recording flag changes. A take punched over beats 0.25 to 0.5 (frames 6 000
/// to 12 000 at 120 bpm and 48 kHz) is recording exactly at the transports whose
/// beat is in [0.25, 0.5), and the log (its first 16 transports) holds the
/// punch-in and the punch-out.
#[cfg(feature = "clap")]
#[test]
fn a_plugin_is_told_the_host_records_during_a_take() {
    use crate::loading::{LoadPlugin, Perform, Spec};
    use tutti_shared_plugin::transcript::{Transcript, find_blob};

    tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    let plugin = tutti_plugin_types::PluginRef::file(tutti_fixtures::publish_with_extension(
        &tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin"),
        "clap",
    ));
    let notes = {
        let core = tutti_shared_plugin::core::Core::new();
        core.set_param(tutti_shared_plugin::core::MODE, 3.0 / 4.0);
        core.save()
    };
    let node = rig
        .session
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(plugin.clone()).restoring(tutti_session::PeerState::of_plugin(plugin, notes)),
        )
        .expect("the plugin loads");
    let handle = rig
        .session
        .instrument(node)
        .expect("the plugin declares itself an instrument");
    rig.session
        .graph()
        .connect(handle.output(), MainOutput)
        .unwrap();

    rig.session.play().expect("queued");
    rig.step(64);
    rig.step(64);
    let punch = Punch::new(Beat::new(0.25)).until(Beat::new(0.5)).unwrap();
    let take = rig
        .session
        .record(Record::new(&rig.input, TakeFile::wav(wav(&dir, "told.wav"))).punch(punch))
        .expect("armed");
    while rig.pushed < 24_000 {
        rig.step(BLOCK);
    }
    let state = rig
        .session
        .save_state(&node, tutti_plugin_types::StateUse::Project)
        .expect("the plugin is hosted")
        .perform(tutti_fixtures::main_thread())
        .expect("the plugin saves its state");
    let told = Transcript::parse(&find_blob(state.bytes())).transports;
    assert!(
        told.iter()
            .all(|t| t.recording == (0.25..0.5).contains(&t.beat)),
        "recording exactly while the beat is in [0.25, 0.5): {told:?}"
    );
    let mut flags: Vec<bool> = told.iter().map(|t| t.recording).collect();
    flags.dedup();
    assert_eq!(flags, [false, true, false], "{told:?}");
    assert!(told.iter().all(|t| t.playing), "{told:?}");
    drop(take);
}

/// **A punch in the past starts at once and reports the real F.** The frame
/// punched in at went by before the take was armed, so the first frame the
/// take holds is the first the tap saw, which `placement` states and the file
/// begins with.
#[test]
fn a_punch_in_the_past_starts_at_once_and_reports_the_real_frame() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(1024);
    let take = rig
        .session
        .record(
            Record::new(&rig.input, TakeFile::wav(wav(&dir, "past.wav")))
                .punch(Punch::new(Frame::new(10)).until(Frame::new(5_000)).unwrap()),
        )
        .expect("armed");
    rig.run_until(finished);
    let done = recorded(&rig.events);
    let first = done.placement().first_frame();
    assert!(first.get() >= 1024, "not frame 10: {first}");
    let samples = read(done.path().unwrap());
    assert_eq!(samples[0][0], first.get() as f32);
    assert_eq!(
        done.frames().get(),
        5_000 - first.get() as usize,
        "up to the punch-out"
    );
    drop(take);
}

/// **Placement is the formula, term by term.** With the capture reporting 5 ms
/// (240 frames), the playback 10 ms (480), the ring a lead of 7 and a trim of 3
/// frames earlier, a take whose first frame is F = 10 000 starts at
/// F - (240 + 480 + 7) - 3 = 9 270, and says each term.
#[test]
fn placement_is_the_formula_with_each_term_stated() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(
        InputConfig::default().offset(Trim::Earlier(Samples::new(3))),
        Samples::new(7),
    );
    // The streams report: the playback through a callback, the capture
    // through the cell its callback records into.
    rig.delay = Some(Duration::from_millis(10));
    rig.capture
        .lock()
        .latency
        .as_ref()
        .expect("handed a cell")
        .record(Some(Duration::from_millis(5)));
    rig.step(256);
    let take = rig
        .session
        .record(
            Record::new(&rig.input, TakeFile::wav(wav(&dir, "p.wav"))).punch(
                Punch::new(Frame::new(10_000))
                    .until(Frame::new(10_100))
                    .unwrap(),
            ),
        )
        .expect("armed");
    rig.run_until(finished);
    let done = recorded(&rig.events);
    let latency = *done.latency();
    assert_eq!(latency.input, Some(Samples::new(240)));
    assert_eq!(latency.output, Some(Samples::new(480)));
    assert_eq!(latency.lead, Samples::new(7));
    assert_eq!(latency.offset, Trim::Earlier(Samples::new(3)));
    assert!(latency.is_complete());
    assert_eq!(done.placement().first_frame(), Frame::new(10_000));
    assert_eq!(done.start(), Start::At(Frame::new(10_000 - 727 - 3)));
    drop(take);
}

/// **A latency the device has not reported is unknown in the take, not zero.**
/// No callback has reported either direction: both terms are `None`, the take
/// says it is incomplete, and the start is placed by the terms that are known.
#[test]
fn an_unreported_latency_is_stated_unknown_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(7));
    rig.step(256);
    let take = rig
        .session
        .record(
            Record::new(&rig.input, TakeFile::wav(wav(&dir, "u.wav"))).punch(
                Punch::new(Frame::new(2_000))
                    .until(Frame::new(2_050))
                    .unwrap(),
            ),
        )
        .expect("armed");
    rig.run_until(finished);
    let done = recorded(&rig.events);
    let latency = *done.latency();
    assert_eq!((latency.input, latency.output), (None, None));
    assert!(!latency.is_complete());
    assert_eq!(
        done.start(),
        Start::At(Frame::new(2_000 - 7)),
        "only the lead is known"
    );
    drop(take);
}

/// **Started lands in the update after the punch-in, then Finished once.** A
/// take that ends by its punch-out reports exactly one Started and one
/// Finished, in that order, carrying the same id the handle has.
#[test]
fn started_then_finished_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let take = rig
        .session
        .record(
            Record::new(&rig.input, TakeFile::wav(wav(&dir, "e.wav"))).punch(
                Punch::new(Frame::new(3_000))
                    .until(Frame::new(3_500))
                    .unwrap(),
            ),
        )
        .expect("armed");
    let id = take.id();
    rig.run_until(finished);
    // `Finished` is the job's end: the pump has returned, and what it sent
    // before landed first. More updates see nothing more of it.
    for _ in 0..8 {
        rig.step(BLOCK);
    }
    let takes = rig.takes();
    assert_eq!(takes.len(), 2, "{takes:?}");
    assert!(matches!(takes[0], TakeEvent::Started { id: got, .. } if *got == id));
    assert!(matches!(takes[1], TakeEvent::Finished { id: got, .. } if *got == id));
    drop(take);
}

/// An open-ended take is finished by its handle: it stops at once and
/// returns without waiting, and an update reports its end with what the take holds.
#[test]
fn an_open_ended_take_is_finished_by_its_handle() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let take = rig
        .session
        .record(Record::new(&rig.input, TakeFile::wav(wav(&dir, "o.wav"))))
        .expect("armed");
    let mut armed: Option<Take<Armed>> = Some(take);
    let mut rolling = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while rolling.is_none() {
        assert!(Instant::now() < deadline);
        rig.step(BLOCK);
        std::thread::yield_now();
        match armed.take().unwrap().rolling() {
            Ok(r) => rolling = Some(r),
            Err(still) => armed = Some(still),
        }
    }
    let rolling = rolling.unwrap();
    let first = rolling.placement().first_frame();
    // `Started` lands once the pump has written the take's first frame.
    rig.run_until(started);
    rolling.finish();
    // The end lands in an update, with what the take holds.
    rig.run_until(finished);
    let done = recorded(&rig.events);
    assert_eq!(done.placement().first_frame(), first);
    assert!(done.frames().get() > 0);
    assert!(
        rig.takes()
            .iter()
            .any(|e| matches!(e, TakeEvent::Finished { .. }))
    );
}

/// **A take ends when its input is removed**, instead of recording silence to
/// the end: it reports `Finished` once with what the input played before the
/// removal and nothing after (the file holds no frame of the silence a removed
/// node's tap reads).
#[test]
fn a_take_ends_when_its_input_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let path = wav(&dir, "r.wav");
    let _take = rig
        .session
        .record(Record::new(&rig.input, TakeFile::wav(path.clone())))
        .expect("armed");
    rig.run_until(started);
    rig.step(BLOCK);
    rig.session.remove(rig.input.node()).unwrap();
    rig.run_until(finished);
    // `Finished` is the job's end: more updates see nothing more of it.
    for _ in 0..8 {
        rig.step(BLOCK);
    }
    let takes = rig.takes();
    assert_eq!(takes.len(), 2, "started, then finished once: {takes:?}");
    let done = recorded(&rig.events);
    assert_eq!(done.end(), TakeEnd::InputRemoved);
    let frames = read(&path);
    assert_eq!(frames.len(), done.frames().get());
    assert!(!frames.is_empty());
    // The ramp's frame i is [i, -i]: silence would be [0, 0] past frame 0.
    assert!(
        frames.iter().all(|f| f[0] == -f[1]) && frames.iter().skip(1).all(|f| f[0] != 0.0),
        "no silence recorded after the removal"
    );
}

/// A sink that takes `ok` writes and then refuses, closing with a disk-full
/// error.
struct Failing {
    ok: usize,
}

impl Sink for Failing {
    type Sample = f32;
    type Error = std::io::Error;

    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    fn write(&mut self, _: Interleaved<'_>) -> Result<(), SinkFailed> {
        if self.ok == 0 {
            return Err(SinkFailed);
        }
        self.ok -= 1;
        Ok(())
    }

    fn finalize(self) -> Result<(), Self::Error> {
        Err(std::io::Error::other("disk full"))
    }
}

/// **A sink that fails mid-take gives Failed with the sink's error as the
/// source**, and no Finished.
#[test]
fn a_sink_that_fails_mid_take_reports_failed_with_its_error() {
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let take = rig
        .session
        .record(Record::new(&rig.input, TakeTarget::sink(Failing { ok: 2 })))
        .expect("armed");
    rig.run_until(finished);
    let takes = rig.takes();
    let TakeEvent::Failed { error, .. } = takes.last().unwrap() else {
        panic!("not Failed: {takes:?}");
    };
    assert!(
        matches!(
            error,
            TakeError::Sink {
                unfinished: None,
                ..
            }
        ),
        "{error:?}"
    );
    // `TakeError` is `Clone`, so the sink's error is shared behind an `Arc`,
    // whose `Error` impl hides the concrete type from a downcast.
    assert_eq!(
        error.source().expect("the sink's error").to_string(),
        "disk full"
    );
    assert_eq!(error.to_string(), "the take's sink failed");
    assert!(
        !takes
            .iter()
            .any(|e| matches!(e, TakeEvent::Finished { .. }))
    );
    drop(take);
}

/// Refusals are where they are made: a punch ending before it starts, a beat
/// with the transport stopped, a sink of another width, a node that is not an
/// input.
#[test]
fn what_cannot_be_armed_is_refused_where_it_is_made() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Punch::new(Beat::new(4.0)).until(Beat::new(4.0)).is_err(),
        "a punch of no length"
    );
    assert!(Punch::new(Frame::new(9)).until(Frame::new(3)).is_err());
    assert!(Punch::new(Beat::new(4.0)).until(At::NextBlock).is_err());

    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(64);
    let stopped = rig
        .session
        .record(
            Record::new(&rig.input, TakeFile::wav(wav(&dir, "s.wav")))
                .punch(Punch::new(Beat::new(2.0))),
        )
        .unwrap_err();
    assert!(
        matches!(stopped, TakeError::TransportStopped(b) if b == Beat::new(2.0)),
        "{stopped:?}"
    );
    // A beat against a frame, compared once the beat is a frame.
    roll(&mut rig);
    let backwards = rig
        .session
        .record(
            Record::new(&rig.input, TakeFile::wav(wav(&dir, "b.wav"))).punch(
                Punch::new(Frame::new(500_000))
                    .until(Beat::new(1.0))
                    .unwrap(),
            ),
        )
        .unwrap_err();
    assert!(matches!(backwards, TakeError::Punch(_)), "{backwards:?}");

    struct Mono;
    impl Sink for Mono {
        type Sample = f32;
        type Error = core::convert::Infallible;

        fn layout(&self) -> ChannelLayout {
            ChannelLayout::MONO
        }
        fn write(&mut self, _: Interleaved<'_>) -> Result<(), SinkFailed> {
            Ok(())
        }
        fn finalize(self) -> Result<(), Self::Error> {
            Ok(())
        }
    }
    let wide = rig
        .session
        .record(Record::new(&rig.input, TakeTarget::sink(Mono)))
        .unwrap_err();
    assert!(matches!(wide, TakeError::Width { .. }), "{wide:?}");
}

/// A take written to a file makes a clip from it; one written to a sink of
/// the caller's has no file to make one from, and says so by name.
#[cfg(feature = "sampler")]
#[test]
fn a_take_makes_a_clip_only_from_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let to_file = rig
        .session
        .record(
            Record::new(&rig.input, TakeFile::wav(wav(&dir, "c.wav"))).punch(
                Punch::new(Frame::new(1_000))
                    .until(Frame::new(1_200))
                    .unwrap(),
            ),
        )
        .expect("armed");
    rig.run_until(finished);
    let from_file = recorded(&rig.events).clip::<f32>();
    assert!(from_file.is_ok(), "{:?}", from_file.err());

    rig.events.clear();
    struct Clean;
    impl Sink for Clean {
        type Sample = f32;
        type Error = core::convert::Infallible;

        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }
        fn write(&mut self, _: Interleaved<'_>) -> Result<(), SinkFailed> {
            Ok(())
        }
        fn finalize(self) -> Result<(), Self::Error> {
            Ok(())
        }
    }
    let from = rig.pushed as u64;
    let to_sink = rig
        .session
        .record(
            Record::new(&rig.input, TakeTarget::sink(Clean)).punch(
                Punch::new(Frame::new(from + 1_000))
                    .until(Frame::new(from + 1_100))
                    .unwrap(),
            ),
        )
        .expect("armed");
    rig.run_until(finished);
    let from_sink = recorded(&rig.events).clip::<f32>();
    assert!(
        matches!(
            from_sink,
            Err(tutti_session::Error::Take(TakeError::NoFile))
        ),
        "{:?}",
        from_sink.err()
    );
    drop((to_file, to_sink));
}

/// **A take records and finishes, and the file holds the frames the input played.** The
/// pump runs on tutti's thread, woken by the tap's bell: the ramp the test pushed (frame `i` is `[i, -i]`) comes back frame for frame from the
/// first frame the take kept.
#[test]
fn a_take_records_and_its_file_holds_the_frames_written() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let path = wav(&dir, "ramp.wav");
    let take = rig
        .session
        .record(Record::new(&rig.input, TakeFile::wav(path.clone())))
        .expect("armed");
    rig.steps_until(started);
    for _ in 0..16 {
        rig.step(BLOCK);
    }
    take.finish();
    rig.steps_until(finished);
    let done = recorded(&rig.events);
    let first = done.placement().first_frame().get();
    let frames = read(&path);
    assert_eq!(frames.len(), done.frames().get());
    // A stop ends the pump at its next wake, so how much of the tail it drained first
    // depends on when the tick task woke it: the frames it kept are what is pinned.
    assert!(!frames.is_empty(), "the take kept no frame");
    for (k, frame) in frames.iter().enumerate() {
        let want = (first as usize + k) as f32;
        assert_eq!(*frame, [want, -want], "file frame {k}");
    }
}

/// **`finish` returns without waiting, and the completion lands in a later update.**
/// The pump closes the sink on its own thread and the end lands only in an `update`, so
/// `finish` returns at once, nothing has landed, and the updates that follow report the
/// end once.
#[test]
fn finish_returns_at_once_and_the_completion_lands_in_the_updates_after() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let take = rig
        .session
        .record(Record::new(&rig.input, TakeFile::wav(wav(&dir, "f.wav"))))
        .expect("armed");
    rig.steps_until(started);
    let before = rig.takes().len();
    take.finish();
    assert_eq!(
        rig.takes().len(),
        before,
        "finish landed nothing: it only stops the pump"
    );
    rig.steps_until(finished);
    for _ in 0..8 {
        rig.step(BLOCK);
    }
    let ends = rig
        .takes()
        .iter()
        .filter(|e| matches!(e, TakeEvent::Finished { .. } | TakeEvent::Failed { .. }))
        .count();
    assert_eq!(ends, 1, "the end is reported once: {:?}", rig.takes());
}

/// **A take that has nothing to record never stalls `update`.** Its
/// punch-in is far ahead, so the pump has no frame to keep: it awaits its bell instead
/// of waiting on the tap. A hundred updates return with it armed, and stopping it before
/// its punch-in lands as a failure within the updates that follow.
#[test]
fn an_idle_take_never_stalls_update() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let take = rig
        .session
        .record(
            Record::new(&rig.input, TakeFile::wav(wav(&dir, "idle.wav")))
                .punch(Punch::new(At::Frame(Frame::new(1 << 30)))),
        )
        .expect("armed");
    for _ in 0..100 {
        rig.step(BLOCK);
    }
    assert!(rig.takes().is_empty(), "nothing was punched in");
    take.finish();
    rig.steps_until(finished);
    assert!(
        matches!(
            rig.takes().as_slice(),
            [TakeEvent::Failed {
                error: TakeError::NeverStarted,
                ..
            }]
        ),
        "{:?}",
        rig.takes()
    );
}

/// **Dropping a session commits every take before it returns.** The pump awaits its
/// tap's bell; the graph's drop drops the tap's writer, which rings that bell, and the
/// session's drop closes the landings, rings each pump's bell and joins the pump, which
/// closes its sink: the take's file is at its path, complete, holding the ramp's frames,
/// when the drop returns. Until then the path does not exist.
#[test]
fn dropping_a_session_commits_every_take_before_it_returns() {
    let dir = tempfile::tempdir().unwrap();
    let path = wav(&dir, "dropped.wav");
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let take = rig
        .session
        .record(Record::new(&rig.input, TakeFile::wav(path.clone())))
        .expect("armed");
    rig.steps_until(started);
    for _ in 0..16 {
        rig.step(BLOCK);
    }
    assert!(!path.exists(), "the file is committed when the pump ends");

    drop(rig);
    // The drop finishes the take's commit before it returns.
    assert!(
        path.exists(),
        "the drop returned before the take was committed"
    );

    let frames = read(&path);
    assert!(!frames.is_empty(), "the committed file holds the take");
    let first = frames[0][0];
    for (k, frame) in frames.iter().enumerate() {
        let want = first + k as f32;
        assert_eq!(*frame, [want, -want], "frame {k}");
    }
    drop(take);
}

/// **A session dropped on a thread that drives an executor returns, leaving its take's
/// pump detached**: a join there would stop the tasks the thread runs, so the drop does
/// not wait. The future that drops the session ends.
#[test]
fn dropping_a_session_on_a_driving_thread_returns_without_joining() {
    let dir = tempfile::tempdir().unwrap();
    let path = wav(&dir, "detached.wav");
    let executor = tutti_platform::Executor::new();
    // A session is not `Send`: the future builds it, arms the take and drops it, with no
    // await between, on the thread that drives it.
    let dropper = executor
        .spawn_dedicated("drops-a-session", None, {
            let path = path.clone();
            async move {
                let mut rig = rig(InputConfig::default(), Samples::new(0));
                rig.step(256);
                let take = rig
                    .session
                    .record(Record::new(&rig.input, TakeFile::wav(path)))
                    .expect("armed");
                rig.steps_until(started);
                drop(rig);
                drop(take);
            }
        })
        .expect("a dedicated thread");
    assert_eq!(
        tutti_platform::join(dropper),
        Some(()),
        "the future that dropped the session ended without panicking"
    );
}

/// Runs a take of the ramp into `target` to its end and returns the
/// session's events.
fn record_to_end(target: TakeFile) -> Vec<SessionEvent> {
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let take = rig
        .session
        .record(Record::new(&rig.input, target))
        .expect("armed");
    rig.steps_until(started);
    for _ in 0..16 {
        rig.step(BLOCK);
    }
    take.finish();
    rig.steps_until(finished);
    rig.events
}

/// The names in `dir`, sorted.
fn names(dir: &tempfile::TempDir) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

/// **A take onto an existing file is refused when it is armed, with `Exists`: the path keeps
/// its old bytes and nothing is left beside it.**
#[test]
fn a_take_onto_an_existing_file_is_refused_at_arming() {
    let dir = tempfile::tempdir().unwrap();
    let path = wav(&dir, "taken.wav");
    std::fs::write(&path, b"old bytes").unwrap();
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.step(256);
    let Err(error) = rig
        .session
        .record(Record::new(&rig.input, TakeFile::wav(path.clone())))
    else {
        panic!("the path exists");
    };
    assert!(
        matches!(&error, TakeError::Open(e)
            if matches!(**e, tutti_codec::EncodeError::Exists { .. })),
        "{error:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"old bytes");
    assert_eq!(names(&dir), [std::ffi::OsString::from("taken.wav")]);
}

/// **A take with `replace` replaces what is at its path and leaves no temporary file beside
/// it.**
#[test]
fn a_replacing_take_replaces_an_existing_file_and_leaves_nothing_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = wav(&dir, "taken.wav");
    std::fs::write(&path, b"old bytes").unwrap();
    let events = record_to_end(TakeFile::wav(path.clone()).replace());
    let done = recorded(&events);
    assert_eq!(read(&path).len(), done.frames().get());
    assert_eq!(names(&dir), [std::ffi::OsString::from("taken.wav")]);
}

/// 1000.25 steps of 16 bit, exact in `f32`: a quarter of a step off a code.
const LEVEL: f32 = 1_000.25 / 32_768.0;

fn constant(_: usize) -> f32 {
    LEVEL
}

/// A sine of 0.25 at 997 Hz.
fn tone(i: usize) -> f32 {
    (0.25 * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / RATE).sin()) as f32
}

/// The 16-bit samples of a take of `signal` into `target`, and the graph frame the
/// first of them is.
fn signal_take(
    signal: fn(usize) -> f32,
    target: impl FnOnce(std::path::PathBuf) -> TakeFile,
) -> (Vec<i32>, usize) {
    let dir = tempfile::tempdir().unwrap();
    let path = wav(&dir, "signal.wav");
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.signal = Some(signal);
    rig.step(256);
    let take = rig
        .session
        .record(Record::new(&rig.input, target(path.clone())))
        .expect("armed");
    rig.steps_until(started);
    for _ in 0..16 {
        rig.step(BLOCK);
    }
    take.finish();
    rig.steps_until(finished);
    let first = recorded(&rig.events).placement().first_frame().get() as usize;
    let samples = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<i32>()
        .map(Result::unwrap)
        .collect();
    (samples, first)
}

/// A take into a WAV of the integer type `pcm`.
fn int_take(path: std::path::PathBuf, pcm: IntPcm) -> TakeFile {
    TakeFile::new(path, tutti_codec::Format::Wav(tutti_codec::Pcm::Int(pcm)))
}

fn int16(path: std::path::PathBuf) -> TakeFile {
    int_take(path, IntPcm::new(IntDepth::Int16))
}

/// **A take to an integer file is dithered by default, and the dither is triangular:** the
/// samples of a constant a quarter step off a code scatter, and their mean squared distance
/// from the level is 1/4 step^2 (1/6 for the dither's variance, 1/12 for the rounding,
/// whatever the level), which neither plain rounding (1/16 here) nor a constant has.
#[test]
fn a_take_to_an_integer_file_is_triangular_dithered_by_default() {
    let (samples, _) = signal_take(constant, int16);
    // How much of the tail the pump drained before the stop is not pinned: the figure's
    // bound follows the count. The squared error of a triangular dither has a standard
    // deviation under 0.3, and the bound is seven of the mean's.
    let n = samples.len() as f64;
    assert!(n > 200.0, "{n}");
    assert!(samples.iter().any(|&s| s != samples[0]), "a flat file");
    let exact = f64::from(LEVEL) * 32_768.0;
    let power = samples
        .iter()
        .map(|&s| (f64::from(s) - exact).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    assert!((power - 0.25).abs() < 2.1 / n.sqrt(), "{power} over {n}");
}

/// **Takes armed together draw their own dither noise.** They are summed later, and one noise
/// sequence would add coherently. Two takes of one constant, armed in one update, start at one
/// frame, so equal noise would give equal files.
#[test]
fn takes_armed_together_do_not_share_dither_noise() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (wav(&dir, "a.wav"), wav(&dir, "b.wav"));
    let mut rig = rig(InputConfig::default(), Samples::new(0));
    rig.signal = Some(constant);
    rig.step(256);
    let first = rig
        .session
        .record(Record::new(&rig.input, int16(a.clone())))
        .expect("armed");
    let second = rig
        .session
        .record(Record::new(&rig.input, int16(b.clone())))
        .expect("armed");
    rig.steps_until(started);
    for _ in 0..16 {
        rig.step(BLOCK);
    }
    first.finish();
    second.finish();
    rig.steps_until(|events| {
        let ended = |e: &&SessionEvent| matches!(e, SessionEvent::Take(TakeEvent::Finished { .. }));
        events.iter().filter(ended).count() == 2
    });
    let starts: Vec<_> = rig
        .events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Take(TakeEvent::Finished { recorded, .. }) => {
                Some(recorded.placement().first_frame())
            }
            _ => None,
        })
        .collect();
    assert_eq!(starts[0], starts[1], "the takes start together");
    let read16 = |path: &std::path::Path| -> Vec<i16> {
        hound::WavReader::open(path)
            .unwrap()
            .into_samples::<i16>()
            .map(Result::unwrap)
            .collect()
    };
    let (a, b) = (read16(&a), read16(&b));
    let n = a.len().min(b.len());
    assert!(n > 200, "{n}");
    assert_ne!(a[..n], b[..n], "two takes shared one noise sequence");
}

/// **`no_dither` writes the plain rounding, exactly:** every sample is the quantization of
/// the level.
#[test]
fn a_take_with_no_dither_writes_the_rounded_value_exactly() {
    let (samples, _) = signal_take(constant, |path| {
        int_take(path, IntPcm::new(IntDepth::Int16).no_dither())
    });
    let plain = tutti_codec::IntDepth::Int16.quantize(LEVEL);
    assert!(!samples.is_empty());
    assert!(samples.iter().all(|&s| s == plain), "{:?}", &samples[..8]);
}

/// **`noise_shaping` reaches the take's encoder:** with no dither a sine is written as
/// its plain rounding; fed back through the filter a good part of the samples round the
/// other way, and each stays within the feedback bound
/// `(1 + sum |c|) / 2` steps of the sine.
#[test]
fn a_take_with_noise_shaping_feeds_the_rounding_error_back() {
    let (shaped, first) = signal_take(tone, |path| {
        int_take(
            path,
            IntPcm::new(IntDepth::Int16).no_dither().noise_shaping(),
        )
    });
    let bound = 0.5
        * (1.0
            + tutti_dsp::NoiseShaping::new(SampleRate::of(RATE))
                .feedback_gain()
                .get());
    let mut moved = 0;
    for (n, &s) in shaped.iter().enumerate() {
        let exact = f64::from(tone(first + n / 2)) * 32_768.0;
        assert!((f64::from(s) - exact).abs() <= bound + 1e-9, "sample {n}");
        moved += usize::from(f64::from(s) != exact.round());
    }
    assert!(moved * 10 > shaped.len(), "{moved} of {}", shaped.len());
}
