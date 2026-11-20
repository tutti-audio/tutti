//! End to end: what a stream reports ([`StreamStatus`]), a
//! panic contained to its stream, a fallible construction, and the decode
//! lane. The status cell's own table is `status.rs`'s and the step's is
//! `record/tests.rs`'s; these drive a streamer, hand-stepped and threaded.

use crate::VoiceWindow;

use crate::testing::loop_frames;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tutti_platform::Instant;

use tutti_units::{Beat, Bpm, Samples};

use super::command::StreamId;
use super::decode::Seams;
use super::status::{StreamFault, StreamPhase};
use super::*;
use crate::testing::{MockTransport, play, prepared, wait_for_open, wait_for_open_threaded};
use crate::{DiskStreamerConfig, DiskVoice, OpenError};

/// The frames `a..e`, as a block's claimed range.
fn span(a: u64, e: u64) -> std::ops::Range<Samples> {
    Samples::new(a as usize)..Samples::new(e as usize)
}

const WAIT: Duration = Duration::from_secs(10);

/// A stereo float WAV of `seconds` at `rate`, every sample `value` (a dyadic
/// value is read back exactly).
pub(super) fn constant_file(
    dir: &Path,
    name: &str,
    rate: u32,
    seconds: u32,
    value: f32,
) -> PathBuf {
    let path = dir.join(name);
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&path, spec).expect("a writable temp dir");
    for _ in 0..rate * seconds * 2 {
        w.write_sample(value).expect("writes");
    }
    w.finalize().expect("writes");
    path
}

/// The repo's FLAC fixture cut to its first 90%: it opens, reports its whole
/// length, and fails to decode past the cut.
fn cut_flac(dir: &Path) -> PathBuf {
    let bytes = std::fs::read(tutti_fixtures::asset("audio/stereo.flac")).expect("fixture");
    let path = dir.join("cut.flac");
    std::fs::write(&path, &bytes[..bytes.len() * 9 / 10]).expect("write");
    path
}

/// A streamer future with what keeps it going: its bells, whose tick thread wakes the
/// task, and the executor whose thread runs it (a dropped executor ends the future).
struct Threaded {
    streamer: DiskStreamer<Running, f32>,
    _bells: tutti_rt::Bells,
    _executor: tutti_platform::Executor,
}

/// The two drivers, so one case runs on both.
enum Driver {
    Hand(Box<DiskStreamer<Manual<f32>, f32>>),
    /// Its bells, which wake the future's task, live as long as it does.
    Threaded(Box<Threaded>),
}

impl Driver {
    fn start(rate: f64, threaded: bool, seams: Seams) -> Self {
        let rate = tutti_units::SampleRate::of(rate);
        let config = DiskStreamerConfig::default();
        if threaded {
            let bells = tutti_rt::Bells::new().expect("a tick thread");
            let executor = tutti_platform::Executor::new();
            let streamer = DiskStreamer::build(rate, config, seams, &executor, &bells)
                .expect("the future starts");
            Self::Threaded(Box::new(Threaded {
                streamer,
                _bells: bells,
                _executor: executor,
            }))
        } else {
            Self::Hand(Box::new(
                DiskStreamer::<Manual<f32>, f32>::manual_with(rate, config, seams).expect("builds"),
            ))
        }
    }

    /// Opens `path` and waits for the stream to be primed.
    fn open(&mut self, path: &Path) -> (Stream<f32>, StreamReader<f32>) {
        let opened = match self {
            Self::Hand(s) => {
                let opening = s.open(path, Placement::HEAD);
                wait_for_open(s.as_mut(), opening).expect("the file opens")
            }
            Self::Threaded(t) => wait_for_open_threaded(t.streamer.open(path, Placement::HEAD))
                .expect("the file opens"),
        };
        self.until("the stream to be primed", || {
            opened.0.status().phase != StreamPhase::Opening
        });
        opened
    }

    /// Runs the streamer (a step, or a wait for its thread) until `done`, or
    /// fails after [`WAIT`].
    fn until(&mut self, what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < WAIT, "gave up waiting for {what}");
            match self {
                Self::Hand(s) => {
                    let _ = s.step();
                }
                Self::Threaded(..) => {}
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

// ---------------------------------------------------------------------------
// Status.
// ---------------------------------------------------------------------------

/// **A decode fault is `Faulted(Decode)` and the voice reads silence from
/// it.** A FLAC cut at 90% opens and reports its length; the stream is `Live`
/// until the refill reaches the cut, then faulted, and a voice playing it
/// sounds the frames before the cut and nothing after.
#[test]
fn a_decode_fault_is_faulted_and_the_voice_reads_silence() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = cut_flac(dir.path());
    let meta = tutti_codec::read_wave_metadata(&path).expect("the cut file has a header");
    let rate = f64::from(meta.sample_rate);
    let total = meta.total_frames.expect("flac reports a length") as usize;

    let mut sampler =
        DiskStreamer::manual(SampleRate::of(rate), Default::default()).expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let (stream, reader) = wait_for_open(&mut sampler, opening).expect("a cut file opens");
    assert_eq!(
        stream.status().phase,
        StreamPhase::Faulted(StreamFault::Decode)
    );

    let mut voice = prepared(
        DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0))),
        SampleRate::of(rate),
        512,
    );
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let out = play(&mut voice, &clock, SampleRate::of(rate), total, 512);
    let kept = out[0].iter().rposition(|&s| s != 0.0).map_or(0, |i| i + 1);
    assert!(
        kept > 1_000 && kept < total,
        "kept {kept} of {total} frames"
    );
    assert!(
        out.iter().all(|c| c[kept..].iter().all(|&s| s == 0.0)),
        "the voice sounded past the fault"
    );
}

/// **The end of the file is `Ended`, and a looped stream never is.** A short
/// file is wholly in its ring once primed; a loop leaves the stream `Live`.
#[test]
fn the_end_of_the_file_is_ended_and_a_loop_has_none() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "short.wav", 8_000, 1, 0.25);
    let mut sampler =
        DiskStreamer::<_, f32>::manual(SampleRate::SR_8K, Default::default()).expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let (stream, mut reader) = wait_for_open(&mut sampler, opening).expect("opens");
    assert_eq!(
        stream.status().phase,
        StreamPhase::Live,
        "primed, with the file's end still ahead of the window"
    );

    // The reader plays near the end: the refill appends the last frames.
    drop(reader.block(Samples::new(7_900), [span(7_900, 7_904), span(0, 0)]));
    let _ = sampler.step_until_idle(1_000);
    assert_eq!(stream.status().phase, StreamPhase::Ended);

    stream
        .looping_cell()
        .publish(Some(loop_frames(1_000, 4_000, 0)));
    let _ = sampler.step_until_idle(1_000);
    assert_eq!(
        stream.status().phase,
        StreamPhase::Live,
        "a looped stream has no end"
    );
}

/// **Underruns are read from the status**, the first reader of what the voice
/// counts: the total grows with what the audio side reports, and reading it
/// does not lose a count.
#[test]
fn underruns_are_read_from_the_status() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "a.wav", 8_000, 1, 0.25);
    let mut sampler =
        DiskStreamer::<_, f32>::manual(SampleRate::SR_8K, Default::default()).expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let (stream, reader) = wait_for_open(&mut sampler, opening).expect("opens");
    assert_eq!(stream.status().underruns, 0);

    for _ in 0..3 {
        reader.cells.report_underrun();
    }
    assert_eq!(stream.status().underruns, 3);
    assert_eq!(stream.status().underruns, 3, "a read does not consume it");
    reader.cells.report_underrun();
    reader.cells.report_underrun();
    assert_eq!(stream.status().underruns, 5, "the total counts on");
}

/// **A dropped hand-stepped streamer publishes `Faulted(StreamerGone)` to a live
/// stream** at once: its cycle state drops with it.
#[test]
fn a_dropped_streamer_publishes_streamer_gone() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "a.wav", 8_000, 1, 0.25);
    let mut driver = Driver::start(8_000.0, false, Seams::default());
    let (stream, _reader) = driver.open(&path);
    assert!(
        matches!(
            stream.status().phase,
            StreamPhase::Live | StreamPhase::Ended
        ),
        "setup: a served stream"
    );
    drop(driver);
    assert_eq!(
        stream.status().phase,
        StreamPhase::Faulted(StreamFault::StreamerGone)
    );
}

/// Polls `done` until it holds, for at most a count of polls that only guards a hang: the
/// streamer's task is woken by the bells' tick thread and runs on its own thread, which no
/// count of polls orders against.
fn polled_until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..1_000_000 {
        if done() {
            return true;
        }
        std::thread::yield_now();
    }
    done()
}

/// **A dropped streamer's future ends and publishes `Faulted(StreamerGone)`**: the drop
/// raises the stop flag and rings, never joins, and the future's own thread runs it to its
/// end, on an executor that nothing drives as on any other.
#[test]
fn a_dropped_streamer_future_publishes_streamer_gone() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "a.wav", 8_000, 1, 0.25);
    let bells = tutti_rt::Bells::new().expect("a tick thread");
    let executor = tutti_platform::Executor::new();
    let streamer = DiskStreamer::<Running, f32>::build(
        tutti_units::SampleRate::SR_8K,
        DiskStreamerConfig::default(),
        Seams::default(),
        &executor,
        &bells,
    )
    .expect("the future spawns");
    let mut opening = Some(streamer.open(&path, Placement::HEAD));
    let mut opened = None;
    assert!(
        polled_until(|| {
            match opening.take().map(PendingOpen::poll) {
                Some(Open::Ready(stream, reader)) => opened = Some((stream, reader)),
                Some(Open::Pending(again)) => opening = Some(again),
                Some(Open::Failed(error)) => panic!("{error}"),
                None => {}
            }
            opened.is_some()
        }),
        "the streamer never answered the open"
    );
    let (stream, _reader) = opened.expect("opened");
    assert_ne!(
        stream.status().phase,
        StreamPhase::Faulted(StreamFault::StreamerGone)
    );
    drop(streamer);
    assert!(
        polled_until(|| stream.status().phase == StreamPhase::Faulted(StreamFault::StreamerGone)),
        "the future never ended"
    );
}

// ---------------------------------------------------------------------------
// Containment.
// ---------------------------------------------------------------------------

/// **A panicking decode faults only its stream, and the others keep filling**
/// on the hand-stepped and the threaded driver, with two streams (the
/// serial refill) and three (the parallel one). The hook on `RegionOut` panics
/// where stream 1 reads its file once armed; every stream's reader then plays
/// near the end of a file longer than its ring, so each one's refill has
/// frames to read.
fn a_panicking_decode_faults_only_its_stream(threaded: bool) {
    const RATE: u32 = 4_000;
    // 40 s against a 30 s ring: a reader near the end needs reads.
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "long.wav", RATE, 40, 0.25);
    let near_end = u64::from(RATE) * 38;

    for streams in [2usize, 3] {
        let armed = Arc::new(AtomicBool::new(false));
        let hook_armed = Arc::clone(&armed);
        let seams = Seams {
            read_hook: Some(Arc::new(move |id, _at| {
                if id == StreamId::of(1) && hook_armed.load(Ordering::SeqCst) {
                    panic!("test hook: the decoder panicked");
                }
            })),
            ..Seams::default()
        };
        let mut driver = Driver::start(f64::from(RATE), threaded, seams);
        let mut open: Vec<_> = (0..streams).map(|_| driver.open(&path)).collect();
        assert_eq!(
            open[0].1.id(),
            StreamId::of(1),
            "setup: the first open is 1"
        );
        let before: Vec<u64> = open.iter().map(|(_, r)| r.ring.window().1).collect();

        armed.store(true, Ordering::SeqCst);
        for (_, reader) in &mut open {
            drop(reader.block(
                Samples::new(near_end as usize),
                [span(near_end, near_end + 4), span(0, 0)],
            ));
        }

        let status = |open: &[(Stream<f32>, StreamReader<f32>)], i: usize| open[i].0.status().phase;
        driver.until("the faulted stream and the others' refills", || {
            status(&open, 0) == StreamPhase::Faulted(StreamFault::Panicked)
                && (1..streams).all(|i| open[i].1.ring.window().1 > before[i])
        });
        for i in 1..streams {
            assert!(
                !matches!(status(&open, i), StreamPhase::Faulted(_)),
                "stream {} faulted with the one that panicked (threaded: {threaded}, \
                     {streams} streams)",
                i + 1
            );
        }
        // Nothing feeds it any more: its window stays where the failed
        // refill left it.
        let left = open[0].1.ring.window();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            open[0].1.ring.window(),
            left,
            "a faulted stream is not refilled"
        );
    }
}

#[test]
fn a_panicking_decode_faults_only_its_stream_hand_stepped() {
    a_panicking_decode_faults_only_its_stream(false);
}

#[test]
fn a_panicking_decode_faults_only_its_stream_threaded() {
    a_panicking_decode_faults_only_its_stream(true);
}

/// **A panic while a loop is captured faults that stream and no other**: the
/// capture reads the file (a crossfade's lead-in), under the same containment
/// as a refill. Hand-stepped.
#[test]
fn a_panic_while_a_loop_is_captured_faults_that_stream() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = constant_file(dir.path(), "a.wav", 8_000, 2, 0.25);
    let armed = Arc::new(AtomicBool::new(false));
    let hook_armed = Arc::clone(&armed);
    let seams = Seams {
        read_hook: Some(Arc::new(move |id, _at| {
            if id == StreamId::of(1) && hook_armed.load(Ordering::SeqCst) {
                panic!("test hook: the decoder panicked");
            }
        })),
        ..Seams::default()
    };
    let mut driver = Driver::start(8_000.0, false, seams);
    let (first, _r1) = driver.open(&path);
    let (second, _r2) = driver.open(&path);

    armed.store(true, Ordering::SeqCst);
    first
        .looping_cell()
        .publish(Some(loop_frames(2000, 6000, 500)));
    driver.until("the capture to fault its stream", || {
        first.status().phase == StreamPhase::Faulted(StreamFault::Panicked)
    });
    assert_ne!(
        second.status().phase,
        StreamPhase::Faulted(StreamFault::Panicked)
    );
    assert!(
        !matches!(second.status().phase, StreamPhase::Faulted(_)),
        "the other stream faulted too"
    );
}

// ---------------------------------------------------------------------------
// Fallible construction.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The decode lane.
// ---------------------------------------------------------------------------

/// The repo's mp3 with the marker of its length (the `Info` tag of its first
/// frame) overwritten, so the container reports no frame count: a file that
/// cannot seek, which a stream decodes whole. Pinned as such, or the lane
/// tests prove nothing.
pub(super) fn mp3(dir: &Path) -> PathBuf {
    let mut bytes = std::fs::read(tutti_fixtures::asset("audio/stereo.mp3")).expect("fixture");
    let at = bytes
        .windows(4)
        .position(|w| w == b"Info")
        .expect("the fixture carries an Info tag");
    bytes[at..at + 4].copy_from_slice(b"XXXX");
    let path = dir.join("no_length.mp3");
    std::fs::write(&path, &bytes).expect("write");
    assert!(
        !crate::probe(&path).expect("a probe").streamable,
        "setup: the mp3 must be a file the streamer decodes whole"
    );
    path
}

/// **A file that cannot seek opens through the decode lane and plays what
/// the file holds**, hand-stepped and threaded: the first frames of its ring
/// are the decoded file's.
#[test]
fn a_file_that_cannot_seek_opens_through_the_decode_lane() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = mp3(dir.path());
    let wave = tutti_codec::read_wave::<f32>(&path).expect("the fixture decodes");
    for threaded in [false, true] {
        let mut driver = Driver::start(wave.sample_rate().get(), threaded, Seams::default());
        let (stream, mut reader) = driver.open(&path);
        assert!(matches!(
            stream.status().phase,
            StreamPhase::Live | StreamPhase::Ended
        ));
        let claim = reader.block(Samples::new(0), [span(0, 8_000), span(0, 0)]);
        let mut sounded = false;
        for s in 0..8_000u64 {
            assert!(claim.contains(s), "frame {s} (threaded: {threaded})");
            for c in 0..2 {
                let want = wave.at(c, s as usize);
                assert_eq!(claim.frame(s).get(c), want, "frame {s} channel {c}");
                sounded |= want != 0.0;
            }
        }
        assert!(sounded, "setup: the fixture's first frames are not silence");
    }
}

/// **A non-seekable file opens while another stream keeps refilling**: the decode
/// reads a chunk per cycle, so a stream on the same streamer is refilled in the
/// cycles the decode takes, and the open completes when the decode does.
#[test]
fn a_decode_in_flight_does_not_stop_another_stream() {
    const RATE: u32 = 4_000;
    let dir = tempfile::tempdir().expect("a temp dir");
    let long = constant_file(dir.path(), "long.wav", RATE, 40, 0.25);
    let near_end = u64::from(RATE) * 38;
    // A chunk of 256 frames: the fixture takes many cycles to decode.
    let seams = Seams {
        decode_chunk: Some(Samples::new(256)),
        ..Seams::default()
    };
    let mut sampler = DiskStreamer::<Manual<f32>, f32>::manual_with(
        tutti_units::SampleRate::of(f64::from(RATE)),
        Default::default(),
        seams,
    )
    .expect("builds");
    let busy = sampler.open(&long, Placement::HEAD);
    let (_busy, mut reader) = wait_for_open(&mut sampler, busy).expect("opens");
    let before = reader.ring.window().1;

    let mut opening = sampler.open(mp3(dir.path()), Placement::HEAD);
    // The first cycle starts the decode, which one chunk does not finish.
    let _ = sampler.step();
    opening = match opening.poll() {
        Open::Pending(again) => again,
        other => panic!("one chunk of the decode answered the open: {other:?}"),
    };

    drop(reader.block(
        Samples::new(near_end as usize),
        [span(near_end, near_end + 4), span(0, 0)],
    ));
    let _ = sampler.step();
    assert!(
        reader.ring.window().1 > before,
        "the other stream was not refilled while the decode ran"
    );
    let opening = match opening.poll() {
        Open::Pending(again) => again,
        other => panic!("the decode is still running, yet the open answered: {other:?}"),
    };
    let _ = sampler.step_until_idle(10_000);
    assert!(
        matches!(opening.poll(), Open::Ready(..)),
        "the open completes once the decode does"
    );
}

/// **A panic in the decode is that open's failure, not the lane's**: the
/// first open of a file whose decode panics fails with `Format`, and the next
/// open through the same lane succeeds.
#[test]
fn a_panicking_decode_fails_that_open_only() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = mp3(dir.path());
    let first = Arc::new(AtomicBool::new(true));
    let seams = Seams {
        decode_hook: Some(Arc::new(move |_path| {
            if first.swap(false, Ordering::SeqCst) {
                panic!("test hook: the decoder panicked");
            }
        })),
        ..Seams::default()
    };
    let mut sampler = DiskStreamer::<Manual<f32>, f32>::manual_with(
        tutti_units::SampleRate::SR_44K1,
        Default::default(),
        seams,
    )
    .expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let answer = wait_for_open(&mut sampler, opening);
    assert!(
        matches!(&answer, Err(OpenError::Format)),
        "{:?}",
        answer.map(|_| ())
    );
    let opening = sampler.open(&path, Placement::HEAD);
    wait_for_open(&mut sampler, opening).expect("the lane decodes the next file");
}

// ---------------------------------------------------------------------------
// Cut 6: what a fork reads.
// ---------------------------------------------------------------------------

/// **A fork reads the loop as the streamer applied it, not as it was
/// asked.** A loop whose crossfade lead-in lies past a damaged file's cut
/// cannot be read, so the streamer plays it hard (no crossfade) and says so:
/// the stream's status and the ring's published control carry the downgrade,
/// the handle's own copy carries the request, and a fork of the voice plays the
/// downgrade.
#[test]
fn a_fork_plays_the_loop_as_applied_not_as_asked() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = cut_flac(dir.path());
    let meta = tutti_codec::read_wave_metadata(&path).expect("the cut file has a header");
    let total = meta.total_frames.expect("flac reports a length");
    let mut sampler = DiskStreamer::<_, f32>::manual(
        SampleRate::of(f64::from(meta.sample_rate)),
        Default::default(),
    )
    .expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let (stream, reader) = wait_for_open(&mut sampler, opening).expect("a cut file opens");
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));

    // The loop's fade leads in from just before its start, which is past the
    // cut: unreadable.
    let (start, end) = (total as f64 * 0.95, total as f64);
    let asked = Some(loop_frames(start.floor() as usize, end as usize, 100));
    stream.looping_cell().publish(asked);
    let _ = sampler.step_until_idle(1_000);

    let Some(applied) = stream.status().loop_ else {
        panic!("the loop was not applied: {:?}", stream.status().loop_)
    };
    assert_eq!(
        applied.crossfade().get(),
        0,
        "the unreadable lead-in plays hard"
    );
    assert_eq!(stream.loop_(), asked, "the handle's copy is the request");
    assert_eq!(
        voice.forked_loop(),
        None,
        "setup: a live voice is not a fork"
    );
    let fork = voice.fork_copy();
    assert_eq!(
        fork.forked_loop(),
        Some(stream.status().loop_),
        "the fork plays the loop as applied"
    );
    assert_ne!(fork.forked_loop(), Some(asked));
}

/// A stream names its file: its asset, its own rate, and the wave when the
/// streamer holds the file whole. A seekable file is read a piece at a time and
/// has none; a file that cannot seek is decoded whole to be streamed, and the
/// stream's file is that wave itself (not a second decode), the one its writer
/// reads. The reader names the same file as the handle.
#[test]
fn a_stream_names_its_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let wav = constant_file(dir.path(), "a.wav", 8_000, 1, 0.25);
    let mut sampler =
        DiskStreamer::<_, f32>::manual(SampleRate::SR_8K, Default::default()).expect("builds");
    let opening = sampler.open(&wav, Placement::HEAD);
    let (stream, reader) = wait_for_open(&mut sampler, opening).expect("opens");
    assert_eq!(stream.file().asset().name(), wav.display().to_string());
    assert_eq!(stream.file().file_rate(), SampleRate::SR_8K);
    assert!(stream.file().resident().is_none(), "read a piece at a time");
    assert_eq!(reader.file().asset().name(), stream.file().asset().name());
    assert_eq!(reader.file_rate(), SampleRate::SR_8K);

    {
        let mp3 = mp3(dir.path());
        let wave = tutti_codec::read_wave::<f32>(&mp3).expect("the fixture decodes");
        let mut sampler =
            DiskStreamer::<_, f32>::manual(wave.sample_rate(), Default::default()).expect("builds");
        let opening = sampler.open(&mp3, Placement::HEAD);
        let (stream, _reader) = wait_for_open(&mut sampler, opening).expect("opens");
        assert_eq!(stream.file().asset().name(), mp3.display().to_string());
        assert_eq!(stream.file().file_rate(), wave.sample_rate());
        let resident = stream
            .file()
            .resident()
            .expect("decoded whole to be streamed");
        assert_eq!(resident.len(), wave.len());
        assert_eq!(resident.at(0, 100), wave.at(0, 100));
    }
}

/// A fork of a stream whose record is gone says so even when the stream met
/// a decode fault first: the fault word is sticky (the first fault stays), so
/// "the stream is closed" is a fact of its own, which the voice reads.
#[test]
fn a_fork_of_a_closed_stream_fails_even_after_a_decode_fault() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = cut_flac(dir.path());
    let meta = tutti_codec::read_wave_metadata(&path).expect("the cut file has a header");
    let mut sampler = DiskStreamer::<_, f32>::manual(
        SampleRate::of(f64::from(meta.sample_rate)),
        Default::default(),
    )
    .expect("builds");
    let opening = sampler.open(&path, Placement::HEAD);
    let (stream, reader) = wait_for_open(&mut sampler, opening).expect("a cut file opens");
    assert_eq!(
        stream.status().phase,
        StreamPhase::Faulted(StreamFault::Decode),
        "setup: the stream met its fault first"
    );
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
    assert!(
        voice
            .fork_copy()
            .fault()
            .is_some_and(|l| l.fault().is_none()),
        "a fork of a stream still served is healthy"
    );

    drop(stream);
    let _ = sampler.step();
    let fork = voice.fork_copy();
    let fault = fork.fault().and_then(|latch| latch.fault());
    assert!(
        fault.is_some_and(|e| e.to_string().contains("ended")),
        "the closed stream's fork carries the failure"
    );
}

// ---------------------------------------------------------------------------
// An asset of bytes.
// ---------------------------------------------------------------------------

/// **A stream of a `Vec<u8>` asset plays the file's frames**: the repo's FLAC
/// fixture, read in by the test, streams a piece at a time through a decoder
/// opened on the bytes, and its first frames are the file's own.
#[test]
fn a_stream_of_bytes_plays_the_files_frames() {
    let path = tutti_fixtures::asset("audio/stereo.flac");
    let wave = tutti_codec::read_wave::<f32>(&path).expect("the fixture decodes");
    let bytes = std::fs::read(&path).expect("fixture");
    let mut sampler =
        DiskStreamer::<_, f32>::manual(wave.sample_rate(), Default::default()).expect("builds");
    let opening = sampler.open(Asset::from(bytes).hint("flac"), Placement::HEAD);
    let (stream, mut reader) = wait_for_open(&mut sampler, opening).expect("opens");
    assert!(
        stream.file().resident().is_none(),
        "setup: a seekable file is read a piece at a time"
    );
    let claim = reader.block(Samples::new(0), [span(0, 4_000), span(0, 0)]);
    let mut sounded = false;
    for s in 0..4_000u64 {
        assert!(claim.contains(s), "frame {s}");
        for c in 0..2 {
            let want = wave.at(c, s as usize);
            assert_eq!(claim.frame(s).get(c), want, "frame {s} channel {c}");
            sounded |= want != 0.0;
        }
    }
    assert!(sounded, "setup: the fixture's first frames are not silence");
}

/// A source of the mp3 fixture's bytes that gives no identity: what an app's
/// own [`Open`](tutti_platform::Open) is by default.
struct Anonymous(Arc<[u8]>);

impl tutti_platform::Open for Anonymous {
    fn open(&self) -> std::io::Result<Box<dyn tutti_platform::ReadSeek>> {
        Ok(Box::new(std::io::Cursor::new(Arc::clone(&self.0))))
    }

    fn name(&self) -> String {
        "anonymous bytes".into()
    }
}

/// A streamer whose decode thread counts the whole-file decodes it is asked
/// for.
fn counting_streamer() -> (
    DiskStreamer<Manual<f32>, f32>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let decodes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&decodes);
    let seams = Seams {
        decode_hook: Some(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        })),
        ..Seams::default()
    };
    let streamer = DiskStreamer::<Manual<f32>, f32>::manual_with(
        tutti_units::SampleRate::SR_44K1,
        Default::default(),
        seams,
    )
    .expect("builds");
    (streamer, decodes)
}

/// **One `Arc<[u8]>` asset opened twice decodes once**: the second open is
/// served from the cache, which the asset's minted identity keys.
#[test]
fn a_bytes_asset_opened_twice_decodes_once() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let bytes: Arc<[u8]> = std::fs::read(mp3(dir.path())).expect("read").into();
    let asset = Asset::from(bytes).hint("mp3");
    let (mut sampler, decodes) = counting_streamer();
    for _ in 0..2 {
        let opening = sampler.open(asset.clone(), Placement::HEAD);
        wait_for_open(&mut sampler, opening).expect("opens");
    }
    assert_eq!(decodes.load(Ordering::SeqCst), 1, "decoded again");
    assert_eq!(sampler.cache().len(), 1);
}

/// **An asset with no identity is never cached**: the same source opened twice
/// is decoded twice and leaves the cache empty.
#[test]
fn an_asset_without_identity_is_never_cached() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let bytes: Arc<[u8]> = std::fs::read(mp3(dir.path())).expect("read").into();
    let asset =
        Asset::from(Arc::new(Anonymous(bytes)) as Arc<dyn tutti_platform::Open>).hint("mp3");
    assert!(asset.identity().is_none(), "setup");
    let (mut sampler, decodes) = counting_streamer();
    for _ in 0..2 {
        let opening = sampler.open(asset.clone(), Placement::HEAD);
        wait_for_open(&mut sampler, opening).expect("opens");
    }
    assert_eq!(decodes.load(Ordering::SeqCst), 2, "cached");
    assert_eq!(sampler.cache().len(), 0);
}
