use crate::VoiceWindow;
use crate::testing::loop_frames;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tutti_units::{Beat, Bpm, SamplePosition};

use crate::streamer::StreamerPort;

use super::*;
use crate::StreamPhase;
use crate::testing::{MockTransport, play, prepared, wait_for_open};
use crate::{DiskStreamer, DiskVoice};

const RATE: f64 = 48_000.0;

/// A stereo float WAV of `frames` frames, every sample `value`. A dyadic value
/// is read back exactly, so a voice that plays it plays `value` to the bit.
fn write_constant(path: &Path, value: f32, frames: usize) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("a writable temp dir");
    for _ in 0..frames {
        w.write_sample(value).expect("writes");
        w.write_sample(value).expect("writes");
    }
    w.finalize().expect("writes");
}

fn manual() -> DiskStreamer<crate::Manual<f32>, f32> {
    DiskStreamer::manual(SampleRate::of(RATE), Default::default()).expect("builds")
}

/// A file of `value` in a temp dir.
fn constant_file(value: f32) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("constant.wav");
    write_constant(&path, value, 48_000);
    (dir, path)
}

/// One block of the voice playing `reader` from beat 0, planar.
fn first_block(reader: StreamReader<f32>) -> Vec<Vec<f32>> {
    let mut voice = prepared(
        DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0))),
        SampleRate::of(RATE),
        64,
    );
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    play(&mut voice, &clock, SampleRate::of(RATE), 64, 64)
}

/// `StreamStart` keeps the range-checked contract: `new`, `TryFrom` and serde
/// agree (serde as the bare `f64`), and a negative, NaN or infinite position
/// is refused, never clamped.
#[test]
fn placement_keeps_the_range_contract() {
    use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
    let c = RangeContract {
        new: |raw| StreamStart::new(SamplePosition::new(raw)),
        get: |placement| placement.get().get(),
        of: None,
        valid: &[0.0, 0.5, 4_000.0, 1e12],
        invalid: &[-0.5, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY],
    };
    check_range_contract(&c);
    check_range_serde(&c);
}

/// A fractional start is floored to the frame it falls in, and the stream's
/// window opens there: the reader plays from frame 4 000 for 4 000.7.
#[test]
fn a_fractional_start_opens_at_its_frame() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let start = StreamStart::new(SamplePosition::new(4_000.7)).expect("a position");
    assert_eq!(start.frame(), 4_000);
    let opening = streamer.open_stream(&path, start);
    let (_stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    assert_eq!(reader.ring.play(), 4_000);
}

/// An adopted stream is the streamer's from its next step: the handle and the
/// reader name one stream, the table holds none until the streamer runs, and
/// one after.
#[test]
fn an_adopted_stream_is_served_from_the_streamers_next_step() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let opened = streamer
        .open_stream(&path, StreamStart::HEAD)
        .run()
        .expect("the file reads");
    let (stream, reader) = streamer.adopt(opened).expect("adopts");
    assert_eq!(
        streamer.driver.open_streams(),
        0,
        "the streamer has not run"
    );
    assert_eq!(stream.status().phase, StreamPhase::Opening);
    let _ = streamer.step();
    assert_eq!(streamer.driver.open_streams(), 1);
    assert_eq!(
        reader.id(),
        stream.id(),
        "the handle steers the stream it opened"
    );
}

/// An adopted reader has registered its control reader already, on the control
/// side, and the voice built from it registers nothing more: one registration
/// after the open, and still one with the voice built and playing, so nothing
/// on the audio thread registers (which locks and may allocate).
#[test]
fn an_adopted_reader_has_registered_its_control_before_it_is_returned() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (_stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    assert_eq!(
        reader.ring.registered_readers(),
        1,
        "the reader registered by the open"
    );
    let ring = Arc::clone(&reader.ring);
    let mut voice = prepared(
        DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0))),
        SampleRate::of(RATE),
        64,
    );
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let _ = play(&mut voice, &clock, SampleRate::of(RATE), 64, 64);
    assert_eq!(ring.registered_readers(), 1, "the voice registers none");
}

/// Variant `Io`: a path with no file fails the open with the kind the
/// filesystem reported.
#[test]
fn a_missing_file_fails_with_io() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut streamer = manual();
    let opening = streamer.open_stream(dir.path().join("nothing.wav"), StreamStart::HEAD);
    assert_eq!(
        wait_for_open(&mut streamer, opening).err(),
        Some(OpenError::Io(std::io::ErrorKind::NotFound))
    );
}

/// Variant `Format`: a file that opens but is not audio fails the open as
/// such.
#[test]
fn a_file_that_is_not_audio_fails_with_format() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("notes.wav");
    std::fs::write(&path, b"this is not a RIFF file, whatever its name says").expect("writes");
    let mut streamer = manual();
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    assert_eq!(
        wait_for_open(&mut streamer, opening).err(),
        Some(OpenError::Format)
    );
}

/// Variant `StreamerGone`: a read file adopted by a streamer whose thread is gone
/// is refused at the adopt, rather than served by nothing.
#[test]
fn an_adopt_on_a_gone_streamer_fails_with_streamer_gone() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    streamer.driver.gone();
    let opened = streamer
        .open_stream(&path, StreamStart::HEAD)
        .run()
        .expect("reading does not need the streamer");
    assert!(matches!(
        streamer.adopt(opened).err(),
        Some(AdoptError::StreamerGone)
    ));
}

/// Variant `Busy`: an adopt never waits. With the streamer's queue full (nothing steps
/// it) an adopt gives the open back unchanged; once the streamer has run, the same open
/// adopts, and the streamer holds exactly the streams that were served.
#[test]
fn an_adopt_on_a_full_queue_gives_the_open_back_and_applies_after_a_step() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let mut served = Vec::new();
    let busy = loop {
        let opened = streamer
            .open_stream(&path, StreamStart::HEAD)
            .run()
            .expect("the file reads");
        match streamer.adopt(opened) {
            Ok(pair) => served.push(pair),
            Err(AdoptError::Busy(opened)) => break *opened,
            Err(other) => panic!("expected a full queue, got {other:?}"),
        }
        assert!(served.len() < 10_000, "the queue never filled");
    };
    let _ = streamer.step();
    assert_eq!(
        streamer.driver.open_streams(),
        served.len(),
        "the refused open is not a stream"
    );
    let (stream, reader) = streamer.adopt(busy).expect("room after the streamer ran");
    assert_eq!(stream.id(), reader.id());
    let _ = streamer.step();
    assert_eq!(streamer.driver.open_streams(), served.len() + 1);
    // The streams close into a queue that no step drains: end the streamer first, so
    // each close is refused at once rather than waiting for room.
    drop(streamer);
}

/// Two opens, the second read before
/// the streamer has run for the first: each adopt yields the reader of its
/// own open, the first is not the second, each plays its own file, and the
/// first ring is not freed (it is fed) while its stream is open and is freed
/// when its stream closes.
///
/// How it passes: each adopt mints its own stream id and builds its own ring,
/// so a reader can only be its stream's.
#[test]
fn two_opens_each_yield_the_reader_of_their_own_stream() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (path_a, path_b) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    // Dyadic constants: voice A plays 0.25 and voice B 0.5, exactly.
    write_constant(&path_a, 0.25, 48_000);
    write_constant(&path_b, 0.5, 48_000);

    let mut streamer = manual();
    let first = streamer.open_stream(&path_a, StreamStart::HEAD).run();
    let second = streamer.open_stream(&path_b, StreamStart::HEAD).run();
    let (stream_a, a) = streamer.adopt(first.expect("a reads")).expect("adopts");
    let (stream_b, b) = streamer.adopt(second.expect("b reads")).expect("adopts");
    let (id_a, id_b) = (stream_a.id(), stream_b.id());
    assert_ne!(id_a, id_b);
    let _ = streamer.step_until_idle(1_000);
    assert_eq!(
        a.id(),
        id_a,
        "the first open's reader is the first stream's"
    );
    assert_eq!(
        b.id(),
        id_b,
        "the second open's reader is the second stream's"
    );
    assert!(!Arc::ptr_eq(&a.ring, &b.ring), "two streams, two rings");

    let ring_a = Arc::downgrade(&a.ring);
    let (block_a, block_b) = (first_block(a), first_block(b));
    assert!(
        block_a.iter().flatten().all(|&s| s == 0.25),
        "the first reader plays the first file: {:?}",
        &block_a[0][..4]
    );
    assert!(
        block_b.iter().flatten().all(|&s| s == 0.5),
        "the second reader plays the second file: {:?}",
        &block_b[0][..4]
    );

    // The first ring is still its stream's: the second open did not replace
    // it, and the streamer holds it until the stream closes.
    assert!(ring_a.upgrade().is_some(), "the first ring was freed early");
    drop(stream_a);
    let _ = streamer.step();
    assert!(
        ring_a.upgrade().is_none(),
        "the first ring outlived its stream"
    );
}

/// A command meant for one stream cannot reach another.
/// Two streams of one file: a loop set through A's handle is A's alone (B's
/// reader, and a fork of it, still see no loop); and then through B's, B's
/// alone.
#[test]
fn a_loop_set_through_one_stream_leaves_the_other_unchanged() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (a, reader_a) = wait_for_open(&mut streamer, opening).expect("the file opens");
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (b, reader_b) = wait_for_open(&mut streamer, opening).expect("the file opens");
    let loop_of = |reader: &StreamReader<f32>| reader.ring.applied().loop_;
    let looped = |start, end| Some(loop_frames(start, end, 0));

    // B is opened second, so a command that went to "the newest" or to "the
    // first" would be A's or B's by accident; both directions are checked.
    a.looping_cell().publish(looped(1_000, 3_000));
    let _ = streamer.step_until_idle(1_000);
    assert_eq!(loop_of(&reader_a), looped(1_000, 3_000));
    assert_eq!(loop_of(&reader_b), None, "B was not asked");

    b.looping_cell().publish(looped(500, 900));
    let _ = streamer.step_until_idle(1_000);
    assert_eq!(loop_of(&reader_b), looped(500, 900));
    assert_eq!(loop_of(&reader_a), looped(1_000, 3_000), "A is as it was");
}

/// A command for a stream the streamer no longer holds is ignored, and
/// reaches nobody: the handle of a stream that is gone commands nothing. A
/// stream closed by its handle's drop, then a stale handle (one a late
/// command could still be sent through) setting a loop beside a stream that
/// is open: the open one keeps the loop it had, and the streamer neither
/// panics nor keeps anything of the closed one.
#[test]
fn a_stopped_streams_handle_commands_nothing() {
    let (_dir, path) = constant_file(0.25);
    let mut streamer = manual();
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (stopped, reader_a) = wait_for_open(&mut streamer, opening).expect("the file opens");
    let id_a = stopped.id();
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (_b, reader_b) = wait_for_open(&mut streamer, opening).expect("the file opens");

    drop(stopped);
    drop(reader_a);
    let _ = streamer.step();
    assert_eq!(streamer.driver.open_streams(), 1, "A is closed, B is open");

    // The handle of the stream that is gone: what a late command still
    // reaching the queue looks like.
    let ghost = Stream::detached(id_a, StreamerPort::new(streamer.streamer_tx.clone()));
    ghost
        .looping_cell()
        .publish(Some(loop_frames(1_000, 3_000, 0)));
    let _ = streamer.step_until_idle(1_000);
    assert_eq!(
        reader_b.ring.applied().loop_,
        None,
        "a command for a closed stream reached another"
    );
    assert_eq!(streamer.driver.open_streams(), 1);
    drop(ghost);
    let _ = streamer.step();
    assert_eq!(
        streamer.driver.open_streams(),
        1,
        "the ghost's close found nothing to close"
    );
}

/// Frames of the 32-bit integer file the `f64` tests stream.
const I32_FRAMES: usize = 3_000;

/// The integer one full-scale unit is, in the file's 32 bits.
const I32_FULL_SCALE: f64 = 2_147_483_648.0;

/// Left sample `k` of frame `i`: odd, past 24 significant bits.
fn i32_left(i: usize) -> i32 {
    2_000_000_011 + 3 * i as i32
}

/// Right sample of frame `i`, never the left's.
fn i32_right(i: usize) -> i32 {
    -1_500_000_007 - 5 * i as i32
}

/// The stereo 32-bit integer WAV of [`I32_FRAMES`] frames, in a temp dir.
fn i32_file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("i32.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).expect("a writable temp dir");
    for i in 0..I32_FRAMES {
        w.write_sample(i32_left(i)).expect("writes");
        w.write_sample(i32_right(i)).expect("writes");
    }
    w.finalize().expect("writes");
    (dir, path)
}

/// A hand-stepped `f64` streamer.
fn manual_f64() -> DiskStreamer<crate::Manual<f64>, f64> {
    DiskStreamer::<crate::Manual<f64>, f64>::manual(SampleRate::of(RATE), Default::default())
        .expect("builds")
}

/// **An `f64` stream holds the file's values in `f64`**: the file's integer
/// `k` is `k / 2^31` and its odd low bits survive in the ring, the figure an
/// `f32` ring (24 significant bits) cannot hold. The two channels differ at
/// every frame, so a swap or a narrowing is a wrong value.
#[test]
fn an_f64_stream_reads_back_the_files_values_exactly() {
    let (_dir, path) = i32_file();
    let mut streamer = manual_f64();
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (_stream, mut reader) = wait_for_open(&mut streamer, opening).expect("the file opens");

    let whole = Samples::new(0)..Samples::new(I32_FRAMES);
    let none = Samples::new(0)..Samples::new(0);
    let block = reader.block(Samples::new(0), [whole, none]);
    for i in 0..I32_FRAMES {
        assert!(block.contains(i as u64), "frame {i} is in the window");
        let frame = block.frame(i as u64);
        assert_eq!(
            frame.get(0),
            f64::from(i32_left(i)) / I32_FULL_SCALE,
            "left of frame {i}"
        );
        assert_eq!(
            frame.get(1),
            f64::from(i32_right(i)) / I32_FULL_SCALE,
            "right of frame {i}"
        );
    }
    let wide = f64::from(i32_left(1)) / I32_FULL_SCALE;
    assert!(
        wide != f64::from(wide as f32),
        "setup: the value needs more than 24 significant bits"
    );
}

/// **An `f64` ring writes a loop's repeats in `f64`**: with the loop
/// `[1000, 1500)` set, straight position `1500 + k` holds file frame `1000 + k`,
/// the file's exact value.
#[test]
fn an_f64_stream_writes_a_loop_by_position() {
    let (_dir, path) = i32_file();
    let mut streamer = manual_f64();
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (stream, mut reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    stream
        .looping_cell()
        .publish(Some(loop_frames(1_000, 1_500, 0)));
    let _ = streamer.step_until_idle(1_000);

    let play = 1_600usize;
    let reads = [
        Samples::new(play)..Samples::new(play + 100),
        Samples::new(0)..Samples::new(0),
    ];
    let block = reader.block(Samples::new(play), reads);
    for k in 0..100 {
        let at = (play + k) as u64;
        assert!(block.contains(at), "position {at} is in the window");
        let file_frame = 1_000 + (play + k - 1_000) % 500;
        assert_eq!(
            block.frame(at).get(0),
            f64::from(i32_left(file_frame)) / I32_FULL_SCALE,
            "position {at}"
        );
    }
}
