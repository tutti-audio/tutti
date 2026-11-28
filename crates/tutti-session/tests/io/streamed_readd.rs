//! A streamed voice removed and declared again on a device session, before the
//! block that hands its old node back: the new node waits for the stream's reader,
//! renders silence, and takes it between blocks once the old node has dropped.

use tutti_buffers::{ChannelLayout, SampleFormat};
use tutti_engine::{ManualStream, ManualStreamDriver, OutputSpec};
use tutti_platform::Instant;
use tutti_sampler::{
    DiskVoice, StreamHold, StreamPhase, StreamStart, Streamed, VoiceNodeConfig, VoiceWindow,
};
use tutti_session::{LiveSession, MainOutput, Mention, NodeDecl, Session, WireStart};
use tutti_units::{Amplitude, Beat, Cents, SampleRate, Samples, StretchFactor};

/// Every frame of the file the voice streams.
const LEVEL: f32 = 0.25;

type Config = VoiceNodeConfig<f32, Streamed>;
type Live = LiveSession<&'static str, f32>;

fn device() -> (Live, ManualStream<f32>) {
    let (driver, stream) = ManualStreamDriver::<f32>::new();
    let spec = OutputSpec::new(SampleRate::SR_48K, ChannelLayout::STEREO, SampleFormat::F32);
    let session = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec, driver)
        .expect("a device session builds with no device");
    (session, stream)
}

/// The streamed voice's config over a file of constant `LEVEL`, its stream primed.
fn voice(s: &mut Live, dir: &std::path::Path) -> Config {
    let path = dir.join("level.wav");
    tutti_fixtures::signal::write_wav_f32(&path, s.sample_rate(), 2, &vec![LEVEL; 96_000]);
    let opened =
        tutti_fixtures::block_on(s.open_stream(path, StreamStart::HEAD).expect("a streamer"))
            .expect("the file reads");
    let (stream, reader) = s.apply(opened).expect("the session serves it");
    // A hang guard by deadline: the streamer's own thread primes the ring.
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while stream.status().phase == StreamPhase::Opening && Instant::now() < deadline {
        std::thread::park_timeout(std::time::Duration::from_millis(1));
    }
    assert_ne!(
        stream.status().phase,
        StreamPhase::Opening,
        "the ring primes"
    );
    VoiceNodeConfig::streaming(
        DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0))),
        stream,
        Amplitude::new(1.0),
        StretchFactor::UNITY,
        Cents::new(0.0),
    )
    .expect("a finite gain and a width the sampler runs")
}

fn declare(s: &mut Live, config: &Config) {
    s.patch((
        NodeDecl::new("voice", config.clone()),
        Mention::new("voice").connect(MainOutput).unfaded(),
    ))
    .expect("the patch applies");
}

fn hold(s: &Live) -> Option<StreamHold> {
    let key = s.key::<Config>(&"voice").expect("declared");
    s.graph_ref()
        .handle(&key)
        .expect("in the graph")
        .stream_hold()
}

/// The loudest sample of a rendered block.
fn peak(stream: &ManualStream<f32>) -> f32 {
    let block = stream.render_block(Samples::new(512)).expect("open");
    block.iter().fold(0.0, |a, &x| a.max(x.abs()))
}

/// **A streamed voice declared again before its old node is handed back waits,
/// then plays.** The first node plays the file's level. It is removed and the
/// same config declared again before the block that retires the old node: the new
/// node reads `Waiting` and the output is silent, since the old node still has the
/// reader. The block that applies the removal hands the old node back; the update
/// that collects it drops it, which lends the reader to the new node, and the
/// block after reads `Holding` and plays the file's level exactly.
#[test]
fn a_streamed_voice_declared_again_before_its_old_node_returns_waits_then_plays() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut s, stream) = device();
    let config = voice(&mut s, dir.path());
    declare(&mut s, &config);
    let _ = s.update();
    s.play().expect("room");
    assert_eq!(hold(&s), Some(StreamHold::Holding));
    assert_eq!(peak(&stream), LEVEL, "the first node plays the file");

    let old = s.key::<Config>(&"voice").expect("declared").erase();
    let _ = s.remove(old).expect("the node is there").unfaded();
    declare(&mut s, &config);
    let _ = s.update();
    assert_eq!(
        hold(&s),
        Some(StreamHold::Waiting),
        "the old node has the reader"
    );

    assert_eq!(
        peak(&stream),
        0.0,
        "the block applies the swap: nothing plays"
    );
    assert_eq!(
        hold(&s),
        Some(StreamHold::Waiting),
        "the old node is not dropped yet"
    );

    let _ = s.update();
    let _ = peak(&stream);
    assert_eq!(
        hold(&s),
        Some(StreamHold::Holding),
        "the new node took the reader"
    );
    assert_eq!(peak(&stream), LEVEL, "and plays the file's level exactly");
}
