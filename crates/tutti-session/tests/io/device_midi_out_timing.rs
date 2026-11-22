//! A sink is sent at the time each event's frame is heard, not when
//! the host happens to drain it: a session on `ManualStreamDriver`s, whose
//! callbacks report the audio output's latency as they are told to.
//!
//! Every figure is arithmetic on the MIDI spec's tick (24 per quarter note,
//! 1/48 s at 120 BPM, so 1 000 frames at 48 kHz) and on instants the test
//! constructs: each callback is dated by the test (`ManualStream::render_block_at`),
//! block `k` at `base + k * BLOCK / RATE`, so no figure depends on how long a
//! call took. The session places events against `Instant::now()` at each
//! `update`, so `base` sits where every send time falls after that `now` and
//! within `MAX_OUTPUT_AHEAD` of it, with a margin of half a second or more
//! either way.

use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_platform::{Instant, instant_of_monotonic_ns, monotonic_ns};

use tutti_buffers::ChannelLayout;
use tutti_engine::{ManualStream, ManualStreamDriver, OutputSpec};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_midi_runtime::{MidiOut, Outgoing, OutputLatency, OutputOffset, SendAt, Taken};
use tutti_session::{EventInputPort, LiveSession, Session};
use tutti_ump::{Message, MidiGroup, System, Ump};
use tutti_ump::{MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_units::{Beat, SampleRate, Samples, Seconds};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 512;
/// The most a figure here may differ from its arithmetic: the nanosecond
/// roundings of a block's date and of a frame count turned into a `Duration`.
const ROUNDING: Duration = Duration::from_nanos(5);

/// Every event the output was told to send, with when.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<Outgoing>>>);

impl MidiOut for Wire {
    fn queue_timed(&self, events: &[Outgoing]) -> Taken {
        self.0.lock().unwrap().extend_from_slice(events);
        Taken::within(events, events.len())
    }
}

impl Wire {
    fn take(&self) -> Vec<Outgoing> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    /// The send times of the ticks among `events`, in order; panics on a tick
    /// sent `Now`.
    fn tick_times(events: &[Outgoing]) -> Vec<Instant> {
        events
            .iter()
            .filter(|e| e.ump == Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock)))
            .map(|e| match e.send {
                SendAt::At(at) => at,
                SendAt::Now => panic!("a tick was sent now"),
            })
            .collect()
    }
}

fn spec() -> OutputSpec {
    OutputSpec::new(
        SampleRate::of(RATE),
        ChannelLayout::STEREO,
        tutti_buffers::SampleFormat::F32,
    )
}

/// A session on a manual stream with its clock running into `wire`.
fn rig(wire: &Wire) -> (LiveSession, ManualStream, tutti_session::MidiSink) {
    let (driver, stream) = ManualStreamDriver::new();
    let mut s = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec(), driver)
        .expect("builds with no device");
    let _ = s.update();
    let sink = s.open_midi_sink(Arc::new(wire.clone())).open();
    let _ = s.update();
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages {
            clock: true,
            ..tutti_midi_runtime::SyncMessages::OFF
        });
    s.play().expect("room");
    (s, stream, sink)
}

/// The OS clock now moved by `seconds`, in nanoseconds: the date of block 0.
fn base_ns(seconds: f64) -> i64 {
    monotonic_ns() + (seconds * 1e9) as i64
}

/// The instant of `ns` on the OS clock.
fn instant(ns: i64) -> Instant {
    instant_of_monotonic_ns(ns).expect("an instant near now")
}

/// The instant frame `frame` is rendered, for blocks dated from `base`: its
/// frames over the rate after block 0's date.
fn rendered(base: i64, frame: u64) -> Instant {
    instant(base) + Duration::from_secs_f64(frame as f64 / RATE)
}

/// Renders blocks `from..to`, block `k` dated `base + k * BLOCK / RATE`
/// (rounded to the nanosecond) and reporting `delay` as the audio output's
/// latency.
fn render(stream: &ManualStream, base: i64, blocks: Range<usize>, delay: Option<Duration>) {
    for k in blocks {
        let frames = (k * BLOCK) as u128;
        let ns = (frames * 1_000_000_000 + RATE as u128 / 2) / RATE as u128;
        stream
            .render_block_at(Samples::new(BLOCK), base + ns as i64, delay)
            .expect("the stream is open");
    }
}

/// Asserts `at` is `expected` to within [`ROUNDING`].
#[track_caller]
fn assert_at(at: Instant, expected: Instant, what: &str) {
    let off = if at >= expected {
        at.duration_since(expected)
    } else {
        expected.duration_since(at)
    };
    assert!(
        off <= ROUNDING,
        "{what}: {at:?} is {off:?} from {expected:?}"
    );
}

/// Asserts `ticks` are the clock's, one per 1 000 frames from frame 0 on the
/// first, each sent `after` its frame is rendered.
#[track_caller]
fn assert_ticks(ticks: &[Instant], base: i64, after: Duration) {
    for (j, at) in ticks.iter().enumerate() {
        assert_at(
            *at,
            rendered(base, 1_000 * j as u64) + after,
            &format!("tick {j}"),
        );
    }
}

/// **Events reach the output at the instants their frames are heard, spaced by
/// the frames.** With the stream reporting 1.5 s of audio latency and 60 blocks
/// of clock rendered, every tick is in the future, and ticks 1 000 frames apart
/// are exactly 1 000 / 48 000 s apart in `SendAt::At`, not all `Now`, the
/// newest at its frame's instant plus the latency.
#[test]
fn the_clock_is_sent_at_the_time_of_its_frames() {
    let wire = Wire::default();
    let (mut s, stream, _sink) = rig(&wire);
    let latency = Duration::from_millis(1500);
    let base = base_ns(-1.0);
    render(&stream, base, 0..60, Some(latency));
    let _ = s.update();
    let sent = wire.take();
    let ticks = Wire::tick_times(&sent);
    // Frames 0 to 30 719 hold the ticks at 0, 1 000, ... 30 000.
    assert_eq!(ticks.len(), 31);
    for pair in ticks.windows(2) {
        assert_at(
            pair[1],
            pair[0] + Duration::from_secs_f64(1_000.0 / RATE),
            "tick spacing",
        );
    }
    assert_at(
        *ticks.last().unwrap(),
        rendered(base, 30_000) + latency,
        "the newest tick is its frame's instant plus the latency",
    );
    assert_ticks(&ticks, base, latency);
}

/// **An output's offset shifts every event by it.** The same clock in two
/// sessions, one with a +0.25 s offset: the newest tick leaves exactly 250 ms
/// later relative to the instant block 0 was dated, which is the same for both.
#[test]
fn an_output_offset_shifts_the_send_times() {
    let latency = Some(Duration::from_millis(1500));
    let newest_after_base = |offset: f64| {
        let wire = Wire::default();
        let (mut s, stream, _sink) = rig(&wire);
        let target = s.midi().output_latency();
        s.set(
            target.to(OutputLatency::new(
                Seconds::new(0.0),
                OutputOffset::new(Seconds::new(offset)).unwrap(),
            )
            .unwrap()),
        )
        .unwrap();
        let base = base_ns(-1.0);
        render(&stream, base, 0..60, latency);
        let _ = s.update();
        let ticks = Wire::tick_times(&wire.take());
        assert_ticks(
            &ticks,
            base,
            Duration::from_millis(1500) + Duration::from_secs_f64(offset),
        );
        ticks.last().unwrap().duration_since(instant(base))
    };
    let shift = newest_after_base(0.25) - newest_after_base(0.0);
    assert!(
        shift.abs_diff(Duration::from_millis(250)) <= 2 * ROUNDING,
        "shifted by {shift:?}"
    );
}

/// **A written output latency is the terms' own on a device clock.** A device
/// session's terms start at the default (no latency, no offset); after a write
/// of 12 ms and a +3 ms offset and one update, `output_terms` reports exactly
/// that latency, beside the audio output's.
#[test]
fn a_written_output_latency_is_the_terms_own_on_a_device_clock() {
    let wire = Wire::default();
    let (mut s, stream, _sink) = rig(&wire);
    let before = s.midi().output_terms().expect("a device session has terms");
    assert_eq!(before.latency, OutputLatency::default());

    let written = OutputLatency::new(
        Seconds::new(0.012),
        OutputOffset::new(Seconds::new(0.003)).unwrap(),
    )
    .unwrap();
    let target = s.midi().output_latency();
    s.set(target.to(written)).unwrap();
    render(&stream, base_ns(0.0), 0..2, Some(Duration::from_millis(40)));
    let _ = s.update();
    let terms = s.midi().output_terms().unwrap();
    assert_eq!(
        terms.latency, written,
        "the write is the terms' own latency"
    );
    assert_eq!(terms.audio_output, Some(Seconds::new(0.04)));
    assert!(terms.is_complete());
}

/// **While the audio latency is unreported, events are timed with the terms
/// known, and the terms say so.** Before the stream's first callback and after
/// one that reports nothing, `output_terms` is incomplete and ticks leave at
/// the instant their frames are rendered, with no latency term; once a callback
/// reports 40 ms the term is in, exactly.
#[test]
fn an_unreported_audio_latency_is_left_out_and_reported() {
    let wire = Wire::default();
    let (mut s, stream, _sink) = rig(&wire);
    let terms = s.midi().output_terms().expect("a device session has terms");
    assert_eq!(terms.audio_output, None);
    assert!(!terms.is_complete());

    let base = base_ns(0.5);
    render(&stream, base, 0..60, None);
    let _ = s.update();
    let sent = wire.take();
    assert!(!s.midi().output_terms().unwrap().is_complete());
    assert!(
        sent.iter().all(|e| matches!(e.send, SendAt::At(_))),
        "every event is ahead of the update"
    );
    let ticks = Wire::tick_times(&sent);
    assert_eq!(ticks.len(), 31, "the ticks of frames 0 to 30 719");
    assert_ticks(&ticks, base, Duration::ZERO);

    render(&stream, base, 60..62, Some(Duration::from_millis(40)));
    let terms = s.midi().output_terms().unwrap();
    assert_eq!(terms.audio_output, Some(Seconds::new(0.04)));
    assert!(terms.is_complete());
    let _ = s.update();
    let sent = wire.take();
    assert!(
        sent.iter().all(|e| matches!(e.send, SendAt::At(_))),
        "every event is ahead of the update"
    );
    // Frames 30 720 to 31 743 hold the tick at 31 000.
    let ticks = Wire::tick_times(&sent);
    assert_eq!(ticks.len(), 1);
    assert_at(
        ticks[0],
        rendered(base, 31_000) + Duration::from_millis(40),
        "the reported 40 ms is in the send time",
    );
}

/// **A clip's notes reach a sink at the instants their frames are heard.** A
/// note from beat 1 for one beat at 120 BPM and 48 kHz is a note-on at frame
/// 24 000 and a note-off at 48 000, so the two leave exactly 0.5 s apart in
/// `SendAt::At`, and the note-on leaves at the time its frame is heard: block
/// 0's date plus 0.5 s of frames and the 1.5 s the stream reports, less the
/// sink's own 5 ms latency.
#[test]
fn a_clips_notes_are_sent_when_their_frames_are_heard() {
    let wire = Wire::default();
    let (mut s, stream, sink) = rig(&wire);
    s.midi_nodes()
        .unwrap()
        .sync_controls
        .messages()
        .store(tutti_midi_runtime::SyncMessages::OFF);
    let target = s.midi().output_latency();
    s.set(
        target.to(OutputLatency::new(
            Seconds::new(0.005),
            OutputOffset::new(Seconds::new(0.0)).unwrap(),
        )
        .unwrap()),
    )
    .unwrap();
    let note = |on: bool, at: Beat| {
        let voice = if on {
            Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0xC800))
        } else {
            Voice2::note_off(NoteNumber::of(60), NoteVelocity::MIN)
        };
        TimedMidiEvent::new(at, Message::midi2(MidiChannel::FIRST, voice)).unwrap()
    };
    let clip = s.graph().add(MidiClipNode::new([
        note(true, Beat::new(1.0)),
        note(false, Beat::new(2.0)),
    ]));
    s.graph()
        .set_sources(EventInputPort::new(sink.node(), 0), [clip.event_output(0)])
        .expect("the sink's node takes events");
    let _ = s.update();
    let latency = Duration::from_millis(1500);
    let base = base_ns(-1.2);
    render(&stream, base, 0..100, Some(latency));
    let _ = s.update();
    let sends: Vec<(bool, Instant)> = wire
        .take()
        .iter()
        .filter_map(|e| match (e.ump.note_key_event(), e.send) {
            (Some(tutti_ump::NoteKeyEvent::On(_)), SendAt::At(at)) => Some((true, at)),
            (Some(tutti_ump::NoteKeyEvent::Off(_)), SendAt::At(at)) => Some((false, at)),
            _ => None,
        })
        .collect();
    let [(true, on), (false, off)] = sends[..] else {
        panic!("a note-on then a note-off, scheduled: {sends:?}")
    };
    assert_at(
        off,
        on + Duration::from_millis(500),
        "the note-off is half a second after the note-on",
    );
    // The note-on is frame 24 000 and the sink's own latency is sent early.
    assert_at(
        on,
        rendered(base, 24_000) + latency - Duration::from_millis(5),
        "the note-on is its frame's instant plus the audio latency, less the sink's",
    );
}
