//! A session built with a thread start that always refuses: it builds, steps its
//! streamer and its jobs from `update`, and bounces a streamed sample.

use std::io;
use std::path::Path;

use tutti_buffers::ChannelLayout;
use tutti_platform::{PriorityRequest, StopToken, ThreadHandle};
use tutti_sampler::{Clip, Placement, StreamPhase};
use tutti_session::{
    AudioEntry, Bounce, BounceLatency, BounceTail, Fallback, PoolState, Range, Root, SamplerEvent,
    SamplerLoad, Session, SessionEvent, audio_file,
};
use tutti_units::{Beat, SampleRate, Seconds};

const RATE: f64 = 48_000.0;
/// File frames: a second of audio.
const LEN: usize = 48_000;
/// Updates a landing may take: a count, never a time.
const UPDATES: usize = 8;

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

/// The host's choice here: no thread starts, for any reason.
fn no_threads(
    _: &str,
    _: Option<PriorityRequest>,
    _: Box<dyn FnOnce(&StopToken) + Send>,
) -> io::Result<ThreadHandle<()>> {
    Err(io::Error::other("no threads here"))
}

fn session() -> Session {
    Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .thread_start(no_threads)
        .build()
        .expect("a session builds with no thread")
}

fn fallbacks(events: &[SessionEvent], which: Fallback) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, SessionEvent::SteppedFallback { what } if *what == which))
        .count()
}

/// The sampler events of up to [`UPDATES`] updates, until one is `want`.
fn updates_until_sampler(s: &mut Session) -> (Vec<SessionEvent>, Option<SamplerEvent>) {
    let mut all = Vec::new();
    for _ in 0..UPDATES {
        let mut events = s.update();
        let found = events
            .iter()
            .position(|e| matches!(e, SessionEvent::Sampler(_)));
        let landed = found.map(|at| match events.remove(at) {
            SessionEvent::Sampler(event) => event,
            _ => unreachable!("matched a sampler event"),
        });
        all.extend(events);
        if landed.is_some() {
            return (all, landed);
        }
    }
    (all, None)
}

/// **A refused streamer thread is reported once and counted once, at build.**
/// The first update carries one `SteppedFallback` for the streamer and none for
/// the jobs, which have started nothing yet.
#[test]
fn a_refused_streamer_thread_is_reported_once_and_counted_once() {
    let mut s = session();
    assert_eq!(s.fallbacks(Fallback::Streamer), 1);
    assert_eq!(s.fallbacks(Fallback::Jobs), 0);
    let first = s.update();
    assert_eq!(fallbacks(&first, Fallback::Streamer), 1);
    assert_eq!(fallbacks(&first, Fallback::Jobs), 0);
    assert_eq!(
        fallbacks(&s.update(), Fallback::Streamer),
        0,
        "reported once"
    );
}

/// **One `update` steps a stepped streamer until the stream is primed.** The
/// update that lands the open has already refilled the new stream's ring, so
/// the stream is no longer `Opening`.
#[test]
fn update_primes_an_open_streams_ring_in_one_call() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    let id = s.open_stream(&path, Placement::HEAD).expect("a streamer");
    let (events, landed) = updates_until_sampler(&mut s);
    assert!(
        matches!(landed, Some(SamplerEvent::Opened { id: opened }) if opened == id),
        "{landed:?}"
    );
    assert_eq!(
        fallbacks(&events, Fallback::Jobs),
        0,
        "a stepped open is polled, not a job"
    );
    assert_eq!(s.fallbacks(Fallback::Jobs), 0);
    let (stream, _reader) = s.take_stream(id).expect("the landed stream");
    assert_ne!(
        stream.status().phase,
        StreamPhase::Opening,
        "the ring is primed"
    );
}

/// **A bounce of a streamed sample at unity speed is the source file's frames.**
/// A stereo ramp streamed through a session with no threads, placed at beat 0
/// and bounced for a second: output frame `k` is the file's frame `k` on both
/// channels, exactly, as the file holds it.
#[test]
fn a_bounce_of_a_streamed_sample_at_unity_reproduces_the_files_frames() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    let id: SamplerLoad = s.open_stream(&path, Placement::HEAD).expect("a streamer");
    let (_, landed) = updates_until_sampler(&mut s);
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

/// **A pool load is a job on the inline spawner: one update runs it, and the
/// jobs fallback is reported once and counted once.** A resident `audio_file`
/// entry lands as `Loaded` within a few updates, and the count stays 1.
#[test]
fn a_pool_load_runs_inline_and_reports_the_jobs_fallback_once() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    s.pool(audio_file("ramp", &path)).expect("a declaration");
    assert_eq!(s.fallbacks(Fallback::Jobs), 1);
    let mut reported = 0;
    for _ in 0..UPDATES {
        reported += fallbacks(&s.update(), Fallback::Jobs);
        if matches!(s.pool_state(&"ramp"), Some(PoolState::Loaded)) {
            break;
        }
    }
    assert_eq!(reported, 1);
    assert!(matches!(s.audio(&"ramp"), Ok(AudioEntry::Resident(_))));
    assert_eq!(s.fallbacks(Fallback::Jobs), 1);
}
