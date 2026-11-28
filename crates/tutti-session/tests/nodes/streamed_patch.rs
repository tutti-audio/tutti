//! A streamed voice declared through `Session::patch`: its config builds a node
//! that reads the stream's ring however many builds the patches make.

use crate::offline::offline_48k;
use tutti_buffers::{ChannelLayout, InterleavedMut};
use tutti_platform::Instant;
use tutti_sampler::{DiskVoice, StreamPhase, StreamStart, VoiceNodeConfig, VoiceWindow};
use tutti_session::{ConnectError, MainOutput, Mention, NodeDecl, PatchError, Session, WireStart};
use tutti_units::{Amplitude, Beat, Cents, StretchFactor};

/// Every frame of the file the voice streams.
const LEVEL: f32 = 0.25;

/// The streamed voice's config over a file of constant `LEVEL`, with its stream
/// primed.
fn voice(s: &mut Session, dir: &std::path::Path) -> VoiceNodeConfig<f32, tutti_sampler::Streamed> {
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

/// The loudest sample of `frames` stereo frames the session renders.
fn peak(s: &mut Session, frames: usize) -> f32 {
    let mut buf = vec![0.0f32; frames * 2];
    s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
    buf.iter().fold(0.0, |a, &x| a.max(x.abs()))
}

/// **A patch the check refuses leaves the stream's voice for the patch that
/// follows.** The first patch declares the streamed voice and a wire to a port it
/// has not; the patch builds the node before the check refuses it, and the built
/// node drops with the refusal. The patch with the wire fixed builds again, and
/// the voice plays the file's level from beat 0, not silence.
#[test]
fn a_refused_patch_leaves_the_streamed_voice_playing_in_the_next() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut s = offline_48k();
    let config = voice(&mut s, dir.path());

    let bad = NodeDecl::new("voice", config.clone());
    let wire = bad.output(0).connect(bad.input(0));
    let refused = s.patch((bad, wire));
    let PatchError::Wire { cause, .. } = refused.unwrap_err() else {
        panic!("a wire refusal");
    };
    assert!(matches!(cause, ConnectError::NoSuchInPort(_)));

    s.patch((
        NodeDecl::new("voice", config),
        Mention::new("voice").connect(MainOutput).unfaded(),
    ))
    .expect("the fixed patch applies");
    let _ = s.update();
    s.play().expect("room");
    assert_eq!(peak(&mut s, 1_024), LEVEL, "the file's own level, exactly");
}
