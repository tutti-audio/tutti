//! A session's streamer runs on the thread of its own that `spawn_dedicated` gives it, so
//! a stream opens, primes and bounces with no thread but that and the bells' tick.

use crate::bounced::bounced;
use std::path::Path;
use std::time::Duration;

use tutti_buffers::ChannelLayout;
use tutti_platform::Instant;
use tutti_sampler::{Clip, StreamFailure, StreamPhase, StreamStart};
use tutti_session::{Bounce, BounceLatency, BounceTail, MainOutput, Range, Session};
use tutti_units::{Beat, SampleRate, Seconds};

const RATE: f64 = 48_000.0;
/// File frames: a second of audio.
const LEN: usize = 48_000;
/// Frame `i` of the ramp file: distinct per frame, exact in `f32`.
fn value(i: usize) -> f32 {
    (i as f32 + 1.0) * 1e-5
}

/// A stereo f32 WAV: `value(i)` left, `-value(i)` right.
fn write_ramp(path: &Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..LEN {
        w.write_sample(value(i)).expect("writes");
        w.write_sample(-value(i)).expect("writes");
    }
    w.finalize().expect("writes");
}

fn session() -> Session {
    Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("a session builds")
}

/// Reads `asset` on the calling thread and applies it to `s`.
fn open(
    s: &mut Session,
    asset: impl Into<tutti_platform::Asset>,
) -> (
    tutti_sampler::StreamHandle<f32>,
    tutti_sampler::StreamReader<f32>,
) {
    let opened =
        tutti_fixtures::block_on(s.open_stream(asset, StreamStart::HEAD).expect("a streamer"))
            .expect("the file reads");
    s.apply(opened).expect("the session serves it")
}

/// **An open stream is primed.** The app reads the file and the session applies it; the
/// streamer's own thread fills the ring.
#[test]
fn updates_prime_an_open_streams_ring() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    let (stream, _reader) = open(&mut s, &path);
    // A hang guard by deadline: the streamer's thread primes the ring on its own.
    let deadline = Instant::now() + Duration::from_secs(10);
    while stream.status().phase == StreamPhase::Opening && Instant::now() < deadline {
        std::thread::park_timeout(Duration::from_millis(1));
    }
    assert_ne!(
        stream.status().phase,
        StreamPhase::Opening,
        "the ring is never primed"
    );
}

/// **A bounce of a streamed stereo ramp at unity speed is the source file's
/// frames.** The ramp is placed at beat 0 and bounced for a second: output frame
/// `k` is the file's frame `k` on both channels, exactly, as the file holds it.
#[test]
fn a_bounce_of_a_streamed_ramp_at_unity_reproduces_the_files_frames() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    let (stream, reader) = open(&mut s, &path);
    let node = s
        .graph()
        .add(Clip::streaming(reader, stream).at(Beat::new(0.0)));
    s.graph()
        .connect(node, MainOutput)
        .expect("the clip reaches the main output");
    let _ = s.update();

    let rendered = bounced(
        &mut s,
        Bounce::new()
            .master_buffers()
            .channels(ChannelLayout::STEREO)
            .range(Range::Seconds(Seconds::new(0.0), Seconds::new(1.0)))
            .sample_rate(SampleRate::of(RATE))
            .latency(BounceLatency::Keep)
            .tail(BounceTail::None),
    )
    .expect("the bounce renders")
    .into_master()
    .into_planes();
    for k in 0..LEN {
        assert_eq!(rendered[0][k], value(k), "left, file frame {k}");
        assert_eq!(rendered[1][k], -value(k), "right, file frame {k}");
    }
}

/// **A dropped session ends its streamer: an open stream is told it is gone.** The
/// streamer's task waits on its bell; the session's drop wakes it, and its thread ends the
/// task and drops the stream's record, which publishes `Faulted(StreamerGone)` to the
/// handle the app still holds.
#[test]
fn a_dropped_session_ends_its_streamer_and_tells_the_stream() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    let (stream, _reader) = open(&mut s, &path);
    assert_ne!(
        stream.status().phase,
        StreamPhase::Faulted(StreamFailure::StreamerGone)
    );
    drop(s);
    // A hang guard by deadline: the streamer's thread is woken by the bell.
    let deadline = Instant::now() + Duration::from_secs(10);
    while stream.status().phase != StreamPhase::Faulted(StreamFailure::StreamerGone)
        && Instant::now() < deadline
    {
        std::thread::park_timeout(Duration::from_millis(1));
    }
    assert_eq!(
        stream.status().phase,
        StreamPhase::Faulted(StreamFailure::StreamerGone),
        "the woken streamer never ended and dropped the stream"
    );
}
