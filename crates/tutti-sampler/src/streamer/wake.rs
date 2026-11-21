//! The streamer awaits its bell until a source rings it, one case per source (the
//! table in [`bell`](super::bell)'s docs).
//!
//! The streamer future has no timer: nothing but a ring wakes it, so a case that
//! sees the streamer do what it waits for inside the bound was rung. The command
//! source and shutdown are `thread.rs`'s tests.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tutti_platform::{Executor, Instant};

use tutti_rt::Bells;
use tutti_units::{SampleRate, Samples};

use super::contained::constant_file;
use super::status::StreamPhase;
use super::*;
use crate::testing::wait_for_open_threaded;

/// A session and file rate low enough that a 40 s file is 160 000 frames and
/// its ring (30 s) is 120 000: smaller than the file, so the window can fall
/// below its threshold and be outrun.
const RATE: u32 = 4_000;
const BOUND: Duration = Duration::from_secs(10);

/// What keeps the streamer's future alive: its bells, and the executor whose thread
/// runs it (a dropped executor ends it).
type Rig = (Bells, Executor);

/// A streamer future on a threaded executor, and what keeps it going.
fn streamer() -> (DiskStreamer<Running, f32>, Rig) {
    let executor = Executor::new();
    let bells = Bells::new(&executor).expect("the bells' tick task");
    let streamer = DiskStreamer::start(
        SampleRate::of(f64::from(RATE)),
        DiskStreamerConfig::default(),
        &executor,
        &bells,
    )
    .expect("starts");
    (streamer, (bells, executor))
}

fn until(bound: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < bound {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    done()
}

/// A stream on the long file, primed, with the window it holds.
fn primed(
    path: &Path,
) -> (
    DiskStreamer<Running, f32>,
    Rig,
    StreamHandle<f32>,
    StreamReader<f32>,
    u64,
) {
    let (streamer, bells) = streamer();
    let (stream, reader) =
        wait_for_open_threaded(&streamer, streamer.open_stream(path, StreamStart::HEAD))
            .expect("opens");
    assert!(
        until(BOUND, || stream.status().phase != StreamPhase::Opening),
        "the stream never primed"
    );
    let (from, to) = reader.ring.window();
    assert_eq!(from, 0);
    assert!(
        to < 160_000,
        "setup: the window must not hold the whole file"
    );
    (streamer, bells, stream, reader, to)
}

fn long_file(dir: &Path) -> PathBuf {
    constant_file(dir, "long.wav", RATE, 40, 0.25)
}

fn claim(reader: &mut StreamReader<f32>, play: u64) {
    let none = Samples::new(0)..Samples::new(0);
    drop(reader.block(Samples::new(play as usize), [none.clone(), none]));
}

/// **An open wakes an idle streamer**: the open's command rings, the streamer
/// answers.
#[test]
fn an_open_wakes_an_idle_streamer() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = long_file(dir.path());
    let (streamer, _bells) = streamer();
    let _ = wait_for_open_threaded(&streamer, streamer.open_stream(&path, StreamStart::HEAD))
        .expect("opens");
}

/// **A ring below its refill threshold wakes an idle streamer**: the
/// reader plays on inside its window until fewer than three quarters of the
/// ring are ahead of it, and its block rings; the streamer extends the window.
#[test]
fn a_ring_below_threshold_wakes_an_idle_streamer() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (_streamer, _bells, _stream, mut reader, to) = primed(&long_file(dir.path()));
    claim(&mut reader, to / 2);
    assert!(
        until(BOUND, || reader.ring.window().1 > to),
        "the window did not grow: the streamer slept through a ring below threshold"
    );
}

/// **A jump outside the window wakes an idle streamer**: the reader seeks
/// far past the window's end, and its block rings; the streamer moves the
/// window to hold it.
#[test]
fn a_jump_outside_the_window_wakes_an_idle_streamer() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (_streamer, _bells, _stream, mut reader, to) = primed(&long_file(dir.path()));
    let play = to + 40_001;
    claim(&mut reader, play);
    assert!(
        until(BOUND, || {
            let (from, to) = reader.ring.window();
            from <= play && play < to
        }),
        "the window did not follow the jump: the streamer slept through it"
    );
}

/// **A voice's ring is allocation-free and wait-free on the audio thread**: a
/// reader that asks for the streamer on every block (it alternates between a
/// seek past the window and one back before it, so each block finds the ring
/// wanting a refill) claims two thousand blocks inside `assert_no_alloc`, with
/// the streamer idle for the first of them and running for the rest (so both the
/// ring that wakes it and the ring of an awake one are taken). The
/// allocator's trap covers frees too.
#[test]
fn a_voices_ring_is_allocation_free() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (_streamer, _bells, _stream, mut reader, to) = primed(&long_file(dir.path()));
    let far = to + 40_001;
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            claim(&mut reader, if i % 2 == 0 { far } else { 0 });
        }
    });
}
