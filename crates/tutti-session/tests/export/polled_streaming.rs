//! A session on a polled executor: its streamer and its jobs run inside `update`,
//! so a stream opens, primes and bounces with no thread but the bells' tick.

use std::path::Path;

use tutti_buffers::ChannelLayout;
use tutti_platform::Executor;
use tutti_sampler::{Clip, Placement, StreamPhase};
use tutti_session::{
    AudioEntry, Bounce, BounceLatency, BounceTail, PoolState, Range, Root, SamplerEvent,
    SamplerLoad, Session, SessionEvent, audio_file,
};
use tutti_units::{Beat, SampleRate, Seconds};

const RATE: f64 = 48_000.0;
/// File frames: a second of audio.
const LEN: usize = 48_000;
/// Updates a landing may take. A bound that only guards a hang: the streamer's task
/// is woken by the bells' tick thread, which no count of updates orders against, so
/// it is generous and the loop yields.
const UPDATES: usize = 1_000_000;

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
        .executor(Executor::polled())
        .build()
        .expect("a session builds on a polled executor")
}

/// Updates until a sampler event lands.
fn updates_until_sampler(s: &mut Session) -> Option<SamplerEvent> {
    for _ in 0..UPDATES {
        for event in s.update() {
            if let SessionEvent::Sampler(event) = event {
                return Some(event);
            }
        }
        std::thread::yield_now();
    }
    None
}

/// **An open stream is primed by updates.** Nothing runs between updates: the
/// streamer answers the open inside one, the open's job lands inside a later one,
/// and the ring is filled by then.
#[test]
fn updates_prime_an_open_streams_ring() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    let id = s.open_stream(&path, Placement::HEAD).expect("a streamer");
    let landed = updates_until_sampler(&mut s);
    assert!(
        matches!(landed, Some(SamplerEvent::Opened { id: opened }) if opened == id),
        "{landed:?}"
    );
    let (stream, _reader) = s.take_stream(id).expect("the landed stream");
    assert_ne!(
        stream.status().phase,
        StreamPhase::Opening,
        "the ring is primed"
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
    let id: SamplerLoad = s.open_stream(&path, Placement::HEAD).expect("a streamer");
    let landed = updates_until_sampler(&mut s);
    assert!(
        matches!(landed, Some(SamplerEvent::Opened { id: opened }) if opened == id),
        "{landed:?}"
    );
    let (stream, reader) = s.take_stream(id).expect("the landed stream");
    let (node, _handle) = s
        .graph()
        .add(Clip::streaming(reader, stream).at(Beat::new(0.0)));
    s.graph()
        .connect(node, Root)
        .expect("the clip reaches the root");
    let _ = s.update();

    let rendered = s
        .bounce(
            Bounce::project()
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

/// **A pool load runs inside `update`.** A resident `audio_file` entry lands as
/// `Loaded` with the ramp's frames, and nothing ran before the first update.
#[test]
fn a_pool_load_runs_inside_update() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    s.pool(audio_file("ramp", &path)).expect("a declaration");
    assert!(
        !matches!(s.pool_state(&"ramp"), Some(PoolState::Loaded)),
        "nothing runs outside update"
    );
    for _ in 0..UPDATES {
        let _ = s.update();
        if matches!(s.pool_state(&"ramp"), Some(PoolState::Loaded)) {
            break;
        }
    }
    let Ok(AudioEntry::Resident(wave)) = s.audio(&"ramp") else {
        panic!("the entry is resident once loaded");
    };
    assert_eq!(wave.len().get(), LEN);
    assert_eq!(wave.at(0, LEN - 1), value(LEN - 1));
}

/// **A dropped session ends its streamer: the open stream's source is released.** The
/// stream is opened from bytes the test also holds, so the source's reference count shows
/// whether the streamer still holds the stream. The streamer's task waits on its bell;
/// the session's drop wakes it, and the executor's next poll ends the task and drops the
/// stream with its source.
#[test]
fn a_dropped_session_ends_its_streamer_and_releases_the_streams_source() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let bytes: std::sync::Arc<[u8]> = std::fs::read(&path).expect("reads").into();
    let executor = Executor::polled();
    let mut s = Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .executor(executor.clone())
        .build()
        .expect("a session builds on a polled executor");
    let id = s
        .open_stream(std::sync::Arc::clone(&bytes), Placement::HEAD)
        .expect("a streamer");
    let landed = updates_until_sampler(&mut s);
    assert!(
        matches!(landed, Some(SamplerEvent::Opened { id: opened }) if opened == id),
        "{landed:?}"
    );
    assert!(
        std::sync::Arc::strong_count(&bytes) > 1,
        "the open stream holds its source"
    );
    drop(s);
    assert!(
        std::sync::Arc::strong_count(&bytes) > 1,
        "the streamer's task still holds the stream until it is polled"
    );
    executor.poll(usize::MAX);
    assert_eq!(
        std::sync::Arc::strong_count(&bytes),
        1,
        "the woken streamer ended and dropped the stream"
    );
}
