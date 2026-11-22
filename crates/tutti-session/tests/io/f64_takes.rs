//! A take recorded on an `f64` session: the capture, the inlet, the pump, the
//! file and the clip it reads back as all carry the graph's own format, so a
//! figure `f32` cannot hold survives the whole way.

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_platform::Instant;

use tutti_buffers::ChannelLayout;
use tutti_buffers::{Interleaved, SampleFormat};
use tutti_codec::{Format, Pcm};
use tutti_cpal::MicConfig;
use tutti_engine::{ManualStream, ManualStreamDriver, OutputSpec};
use tutti_graph::{InletRing, TapWriter};
use tutti_session::{
    InputConfig, InputOpener, OpenedInput, Punch, Record, Root, Session, SessionEvent, TakeEvent,
    TakeFile,
};
use tutti_units::{Frame, SampleRate, Samples};

use super::f64_device::f64_nodes::FINE;

const BLOCK: usize = 256;

/// The capture's writer, shared with the opener that made it.
type Writer = Arc<Mutex<Option<TapWriter<f64>>>>;

#[derive(Debug)]
struct Opener(Writer);

impl InputOpener<f64> for Opener {
    fn open(&mut self, _: &MicConfig, rate: SampleRate) -> tutti_cpal::Result<OpenedInput<f64>> {
        let (ring, writer) = InletRing::pair(ChannelLayout::STEREO, Samples::new(1 << 16), rate);
        *self.0.lock().unwrap() = Some(writer);
        Ok(OpenedInput {
            ring,
            keep: Box::new(()) as Box<dyn Any + Send>,
            lead: Samples::new(0),
        })
    }
}

/// The test's opener captures at one width and refuses the other, as a capture device
/// refuses a format it cannot give.
impl InputOpener<f32> for Opener {
    fn open(&mut self, _: &MicConfig, _: SampleRate) -> tutti_cpal::Result<OpenedInput<f32>> {
        Err(tutti_cpal::DeviceError::InvalidConfig(
            "this capture serves one sample width only".into(),
        ))
    }
}

/// **A take of `f64` frames reads back as a `Clip<f64>` holding `FINE`.** The
/// inlet is fed `[FINE, -FINE]` every frame; the take is written as 64-bit
/// float and read back unrounded, and the clip, added to the graph and played,
/// renders `FINE` to the stream.
#[test]
fn an_f64_take_reads_back_as_a_clip_holding_a_figure_f32_rounds_away() {
    assert_ne!(f64::from(FINE as f32), FINE, "setup: f32 rounds FINE away");
    let writer: Writer = Writer::default();
    let (driver, stream): (_, ManualStream<f64>) = ManualStreamDriver::<f64>::new();
    let spec = OutputSpec::new(SampleRate::SR_48K, ChannelLayout::STEREO, SampleFormat::F64);
    let mut session = Session::builder()
        .float::<f64>()
        .input_opener(Opener(Arc::clone(&writer)))
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec, driver)
        .expect("an f64 device session builds with no device");
    let input = session.add_input(InputConfig::default()).expect("opens");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    let take = session
        .record(
            Record::new(&input, TakeFile::new(&path, Format::Wav(Pcm::Float64))).punch(
                Punch::new(Frame::new(1_000))
                    .until(Frame::new(1_200))
                    .unwrap(),
            ),
        )
        .expect("armed");

    let frame = [FINE, -FINE].repeat(BLOCK);
    let mut events: Vec<SessionEvent> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let finished = |events: &[SessionEvent]| {
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Take(TakeEvent::Finished { .. })))
    };
    while !finished(&events) {
        assert!(Instant::now() < deadline, "never finished: {events:?}");
        let mut guard = writer.lock().unwrap();
        guard
            .as_mut()
            .expect("opened")
            .push_interleaved(Interleaved::new(&frame, ChannelLayout::STEREO).unwrap());
        drop(guard);
        events.extend(session.update());
        stream.render_block(Samples::new(BLOCK)).expect("open");
        events.extend(session.update());
        std::thread::yield_now();
    }
    drop(take);
    let done = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::Take(TakeEvent::Finished { recorded, .. }) => Some(recorded),
            _ => None,
        })
        .expect("finished");
    assert_eq!(done.frames(), Samples::new(200));

    let wave = tutti_codec::read_wave::<f64>(done.path().expect("a file")).expect("reads");
    assert_eq!(wave.len(), Samples::new(200));
    assert!((0..200).all(|i| wave.at(0, i) == FINE && wave.at(1, i) == -FINE));

    let clip = done.clip::<f64>().expect("a clip of the file");
    let node = session.graph().add(clip);
    session
        .graph()
        .connect(node.out(0), Root::output(0))
        .unwrap()
        .unfaded();
    session
        .graph()
        .connect(node.out(1), Root::output(1))
        .unwrap()
        .unfaded();
    let _ = session.update();
    session.play().expect("queued");
    let mut heard = Vec::new();
    for _ in 0..8 {
        let _ = session.update();
        heard.extend(stream.render_block(Samples::new(BLOCK)).expect("open"));
    }
    assert!(heard.contains(&FINE), "the clip plays FINE unrounded");
    assert!(heard.iter().all(|&x| x.abs() <= FINE));
}
