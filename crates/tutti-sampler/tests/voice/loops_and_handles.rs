//! A loop is one value on every tier, and a pool's and a node's handles steer
//! the voice they hold.
//!
//! The material is a ramp whose value *is* its frame index, played at the file's
//! own rate on a transport that moves one file frame per output frame, so
//! output frame `n` is the file frame the voice read: every figure below is an
//! analytic position (a loop repeats `[start, end)`; a speed of 2 reads every
//! second frame), never one render compared with another.

use std::path::Path;
use std::sync::Arc;
use tutti_buffers::Interleaved;

use tutti_buffers::Wave;
use tutti_sampler::testing::{MockTransport, loop_frames, play, wait_for_open};
use tutti_sampler::{
    DiskStreamer, DiskStreamerConfig, DiskVoice, Loop, LoopError, Manual, MemorySource,
    PlayDirection, Playback, SlotId, StreamHandle, StreamStart, VoiceNode, VoiceNodeHandle,
    VoicePool, VoiceWindow,
};
use tutti_units::{Beat, Bpm, PlaybackRate, SampleRate, Samples};

const SR: f64 = 48_000.0;
const RATE: SampleRate = SampleRate::of(SR);
/// Frames in the file: more than every figure below reads.
const LEN: usize = 4_096;
/// Frames each case renders.
const FRAMES: usize = 2_000;
const BLOCK: usize = 64;

/// The value of file frame `i`: its index, scaled into `[0, 1)`, exact in `f32`.
fn value(i: usize) -> f32 {
    i as f32 / LEN as f32
}

/// The file frames a hard loop over `[start, end)` plays, from frame 0: the
/// file up to `end`, then `[start, end)` over and over.
fn looped_frame(n: usize, start: usize, end: usize) -> usize {
    if n < end {
        n
    } else {
        start + (n - end) % (end - start)
    }
}

fn ramp() -> Arc<Wave> {
    let mut w = Wave::new(1, SampleRate::of(SR));
    for i in 0..LEN {
        w.append(Interleaved::new(&[value(i)], w.layout()).unwrap())
            .unwrap();
    }
    Arc::new(w)
}

fn write_ramp(path: &Path) {
    let samples: Vec<f32> = (0..LEN).map(value).collect();
    tutti_fixtures::signal::write_wav_f32(path, RATE, 1, &samples);
}

/// 120 bpm at `SR`: one beat is `SR / 2` frames, so the file's own rate reads
/// one frame per output frame.
fn rolling() -> Arc<MockTransport> {
    MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0))
}

/// A hand-driven streamer, with the ramp's stream open and primed.
fn streaming(
    dir: &Path,
) -> (
    DiskStreamer<Manual<f32>, f32>,
    StreamHandle<f32>,
    tutti_sampler::StreamReader<f32>,
) {
    let path = dir.join("ramp.wav");
    write_ramp(&path);
    let mut streamer = DiskStreamer::manual(SampleRate::of(SR), DiskStreamerConfig::default())
        .expect("streamer builds");
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    (streamer, stream, reader)
}

fn window() -> VoiceWindow {
    VoiceWindow::from(Beat::new(0.0))
}

/// A memory voice node over the ramp, and its handle.
fn memory_node() -> (VoiceNode<f32>, VoiceNodeHandle<f32>) {
    let source = MemorySource::placed(ramp(), window()).expect("a placeable wave");
    VoiceNode::with_channels(
        source,
        Playback::default(),
        tutti_buffers::ChannelLayout::MONO,
    )
    .expect("a finite gain and a nonempty width")
    .with_handle()
}

fn assert_frames(what: &str, got: &[f32], want: impl Fn(usize) -> usize) {
    for (n, &g) in got.iter().enumerate() {
        assert!(
            (g - value(want(n))).abs() < 1e-6,
            "{what}: output frame {n} is {} (file frame {}), want file frame {}",
            g,
            (g * LEN as f32).round(),
            want(n)
        );
    }
}

/// **`Loop` refuses what is not a loop where it is made.** An empty span, a
/// reversed one, and a crossfade longer than the span each name their reason;
/// a crossfade as long as the loop is the longest that fits.
#[test]
fn a_loop_refuses_an_empty_or_reversed_span_and_a_crossfade_that_does_not_fit() {
    let span = |a: usize, b: usize| Samples::new(a)..Samples::new(b);
    assert_eq!(
        Loop::new(span(10, 10)),
        Err(LoopError::EmptySpan {
            start: Samples::new(10),
            end: Samples::new(10)
        })
    );
    assert!(matches!(
        Loop::new(span(20, 10)),
        Err(LoopError::EmptySpan { .. })
    ));
    let looped = Loop::new(span(100, 164)).expect("a loop of 64 frames");
    assert_eq!(looped.len(), Samples::new(64));
    assert_eq!(
        looped.with_crossfade(Samples::new(65)),
        Err(LoopError::CrossfadeTooLong {
            crossfade: Samples::new(65),
            len: Samples::new(64)
        })
    );
    let longest = looped
        .with_crossfade(Samples::new(64))
        .expect("a crossfade as long as the loop fits");
    assert_eq!(longest.crossfade(), Samples::new(64));
}

/// **A looped memory voice repeats the span's frames.** Hard loop `[300, 700)`
/// of the ramp, set through the node's handle: the output is the file to frame
/// 700, then frames 300 to 699 over and over.
#[test]
fn a_looped_memory_voice_repeats_the_span() {
    let (mut node, handle) = memory_node();
    handle
        .looping_cell()
        .publish(Some(loop_frames(300, 700, 0)));
    let got = render_with(&mut node, FRAMES);
    assert_frames("memory", &got, |n| looped_frame(n, 300, 700));
}

/// **A looped disk voice repeats the same span's frames**, the loop set on the
/// stream its handle owns: the same `Loop` as the memory voice, and the same
/// analytic positions.
#[test]
fn a_looped_disk_voice_repeats_the_span() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, stream, reader) = streaming(dir.path());
    let voice = DiskVoice::from_reader(reader, window());
    let (mut node, handle) = VoiceNode::streaming(voice, stream, Playback::default())
        .expect("a finite gain and a nonempty width")
        .with_handle();
    handle
        .looping_cell()
        .publish(Some(loop_frames(300, 700, 0)));
    let _ = streamer.step_until_idle(1_000);
    let got = render_with(&mut node, FRAMES);
    assert_frames("disk", &got, |n| looped_frame(n, 300, 700));
}

/// `frames` of `node`, prepared, from beat 0.
fn render_with(node: &mut VoiceNode<f32>, frames: usize) -> Vec<f32> {
    use tutti_graph::Node;
    node.prepare(&tutti_graph::Prepare::new(
        RATE,
        tutti_buffers::MaxBlock::of(Samples::new(BLOCK)),
    ));
    play(node, &rolling(), RATE, frames, BLOCK).swap_remove(0)
}

/// **A disk voice added through the pool handle keeps its loop and direction.**
/// `add_streaming` takes the voice's `Playback` with the stream: the loop it
/// carries plays the span, and a reversed one plays the file from its end.
/// (A disk voice cannot be added without its stream, so the add that once
/// dropped both silently is not writable: see `VoicePoolHandle::add`.)
#[test]
fn a_disk_voice_added_to_a_pool_keeps_its_loop_and_direction() {
    for direction in [PlayDirection::Forward, PlayDirection::Reverse] {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (mut streamer, stream, reader) = streaming(dir.path());
        let voice = DiskVoice::from_reader(reader, window());
        let (pool, handle) = VoicePool::with_channels(tutti_buffers::ChannelLayout::MONO)
            .expect("mono is a width")
            .with_handle();
        let play = Playback {
            loop_: Some(loop_frames(300, 700, 0)),
            direction,
            ..Playback::default()
        };
        handle
            .add_streaming(SlotId(1), voice, stream, play)
            .expect("the queue has room");
        let _ = streamer.step_until_idle(1_000);
        let mut pool = tutti_sampler::testing::prepared(pool, RATE, BLOCK);
        let got = play_pool(&mut pool, &mut streamer);
        match direction {
            // Reverse mirrors the file and ignores the loop. A live stream turns
            // by a crossfade from what it was playing (about 256 frames past
            // the block it is in, over 512 more), so the mirror is judged past
            // it.
            PlayDirection::Reverse => {
                const TURNED: usize = 1_024;
                assert_frames("reversed disk voice in a pool", &got[TURNED..], |n| {
                    LEN - 1 - (TURNED + n)
                });
            }
            PlayDirection::Forward => {
                assert_frames("looped disk voice in a pool", &got, |n| {
                    looped_frame(n, 300, 700)
                });
            }
        }
        drop(handle);
    }
}

/// `FRAMES` of `pool` from beat 0, the streamer stepped between blocks as a
/// running one is, so a stream that turns or loops has its ring refilled.
fn play_pool(pool: &mut VoicePool<f32>, streamer: &mut DiskStreamer<Manual<f32>, f32>) -> Vec<f32> {
    play_pool_calling(pool, streamer, || {})
}

/// [`play_pool`], calling `mid` once, between the blocks, when half the frames
/// are out.
fn play_pool_calling(
    pool: &mut VoicePool<f32>,
    streamer: &mut DiskStreamer<Manual<f32>, f32>,
    mid: impl FnOnce(),
) -> Vec<f32> {
    let t = rolling();
    let mut out = Vec::new();
    let mut mid = Some(mid);
    while out.len() < FRAMES {
        if out.len() >= FRAMES / 2
            && let Some(mid) = mid.take()
        {
            mid();
        }
        let block = tutti_sampler::testing::block(pool, &t, RATE, BLOCK).swap_remove(0);
        out.extend(block);
        t.advance(BLOCK as i64, RATE);
        let _ = streamer.step_until_idle(1_000);
    }
    out
}

/// **The handle's speed cell sets a memory voice's speed, and stops and resumes a
/// free-running one.** At speed 2 the voice reads every second file frame; a
/// placed voice refuses `stop` and `play`; a free-running one, stopped, is
/// silent and reads nothing, and resumes where its cursor stopped.
#[test]
fn a_node_handle_sets_speed_and_stops_and_resumes_a_free_running_voice() {
    let (mut node, handle) = memory_node();
    handle.speed_cell().publish(PlaybackRate::of(2.0));
    let got = render_with(&mut node, 400);
    assert_frames("speed 2", &got, |n| 2 * n);
    assert_eq!(handle.stop(), Err(tutti_sampler::PlayError::Placed));
    assert_eq!(handle.play(), Err(tutti_sampler::PlayError::Placed));

    let source = MemorySource::new(ramp()).expect("a placeable wave");
    let (mut node, handle) = VoiceNode::with_channels(
        source,
        Playback::default(),
        tutti_buffers::ChannelLayout::MONO,
    )
    .expect("a finite gain and a nonempty width")
    .with_handle();
    node = tutti_sampler::testing::prepared(node, RATE, BLOCK);
    let t = rolling();
    let mut heard = Vec::new();
    for block in 0..6 {
        match block {
            2 => handle.stop().expect("a free-running voice"),
            4 => handle.play().expect("a free-running voice"),
            _ => {}
        }
        heard.extend(tutti_sampler::testing::block(&mut node, &t, RATE, BLOCK).swap_remove(0));
        t.advance(BLOCK as i64, RATE);
    }
    assert!(handle.is_playing());
    for (n, &g) in heard.iter().enumerate() {
        let stopped = (2 * BLOCK..4 * BLOCK).contains(&n);
        let cursor = if n >= 4 * BLOCK { n - 2 * BLOCK } else { n };
        let want = if stopped { 0.0 } else { value(cursor) };
        assert!(
            (g - want).abs() < 1e-6,
            "frame {n}: {g}, want {want} (stopped: {stopped})"
        );
    }
}

/// A free-running (unplaced) memory voice node over the ramp, built as `play`
/// says.
fn free_node(play: Playback) -> VoiceNode<f32> {
    let mut source = MemorySource::new(ramp()).expect("a placeable wave");
    source.set_loop(play.loop_);
    VoiceNode::with_channels(source, play, tutti_buffers::ChannelLayout::MONO)
        .expect("a finite gain and a nonempty width")
}

/// **A reversed free-running voice plays the ramp backwards from its last
/// frame**: output frame `n` is file frame `LEN - 1 - n`, and the voice is
/// silent past the file's first frame.
#[test]
fn a_reversed_free_running_voice_plays_the_ramp_backwards() {
    let mut node = free_node(Playback {
        direction: PlayDirection::Reverse,
        ..Playback::default()
    });
    let got = render_with(&mut node, LEN + 128);
    assert_frames("reversed", &got[..LEN], |n| LEN - 1 - n);
    assert!(
        got[LEN..].iter().all(|&g| g == 0.0),
        "silent past the start"
    );
}

/// **A looped free-running voice wraps its own cursor at the loop's end**:
/// output frame `n` is file frame `looped_frame(n, 300, 700)`.
#[test]
fn a_looped_free_running_voice_wraps_at_the_loops_end() {
    let mut node = free_node(Playback {
        loop_: Some(loop_frames(300, 700, 0)),
        ..Playback::default()
    });
    let got = render_with(&mut node, FRAMES);
    assert_frames("looped", &got, |n| looped_frame(n, 300, 700));
}

/// **A pool refuses a stretch for a free-running voice at the call**, on the
/// add and on a later stretch, and takes it once the voice is placed.
#[test]
fn a_free_running_voice_refuses_stretch_until_it_is_placed() {
    use tutti_units::{Cents, StretchFactor};
    let (_pool, handle) = VoicePool::new().with_handle();
    let id = tutti_sampler::SlotId(1);
    let stretched = Playback {
        stretch: StretchFactor::new(2.0),
        ..Playback::default()
    };
    let free = || MemorySource::new(ramp()).expect("a placeable wave");
    assert_eq!(
        handle.add(id, free(), stretched),
        Err(tutti_sampler::SendError::FreeRunning)
    );
    handle
        .add(id, free(), Playback::default())
        .expect("a plain free-running voice");
    assert_eq!(
        handle.set_stretch(id, StretchFactor::new(2.0), Cents::new(0.0)),
        Err(tutti_sampler::SendError::FreeRunning)
    );
    handle.set_window(id, window()).expect("room");
    handle
        .set_stretch(id, StretchFactor::new(2.0), Cents::new(0.0))
        .expect("a placed voice stretches");
}

/// **`play` starts a free-running voice a reset left stopped**: after the
/// node is reset (which rewinds and stops its cursor) it is silent until
/// `play`, which sounds it from the ramp's first frame. The transport rolls on
/// across the blocks, so nothing else flushes the voice.
#[test]
fn play_restarts_a_free_running_voice_after_a_reset() {
    use tutti_graph::Node;
    let (node, handle) = free_node(Playback::default()).with_handle();
    let mut node = tutti_sampler::testing::prepared(node, RATE, BLOCK);
    let t = rolling();
    let block = |node: &mut VoiceNode<f32>| {
        let out = tutti_sampler::testing::block(node, &t, RATE, BLOCK).swap_remove(0);
        t.advance(BLOCK as i64, RATE);
        out
    };
    assert_frames("before the reset", &block(&mut node), |n| n);
    node.reset();
    assert!(
        block(&mut node).iter().all(|&g| g == 0.0),
        "stopped by the reset"
    );
    handle.play().expect("a free-running voice");
    assert_frames("after play", &block(&mut node), |n| n);
}

/// **A free-running voice built with a stretch plays dry, not silent**: a
/// node built directly with a stretch in its `Playback` has no way to refuse
/// it, and the stretch path reads the transport's clock, which the voice does
/// not have. It reads its own cursor, unstretched: output frame `n` is file
/// frame `n`, bit for bit.
#[test]
fn a_free_running_voice_built_with_a_stretch_plays_the_ramp_dry() {
    let mut node = free_node(Playback {
        stretch: tutti_units::StretchFactor::new(2.0),
        ..Playback::default()
    });
    let got = render_with(&mut node, 1_000);
    assert_frames("dry", &got, |n| n);
}

/// A mono pool holding the ramp as a placed memory voice at `SlotId(1)`.
fn pool_with_resident_ramp() -> (VoicePool<f32>, tutti_sampler::VoicePoolHandle<f32>) {
    let (pool, handle) = VoicePool::with_channels(tutti_buffers::ChannelLayout::MONO)
        .expect("mono is a width")
        .with_handle();
    let source = MemorySource::placed(ramp(), window()).expect("a placeable wave");
    handle
        .add(SlotId(1), source, Playback::default())
        .expect("room");
    (prepared_pool(pool), handle)
}

fn prepared_pool(pool: VoicePool<f32>) -> VoicePool<f32> {
    tutti_sampler::testing::prepared(pool, RATE, BLOCK)
}

/// `FRAMES` of the memory `pool` from beat 0, calling `mid` between the blocks
/// when half are out.
fn play_memory_pool(pool: &mut VoicePool<f32>, mid: impl FnOnce()) -> Vec<f32> {
    let t = rolling();
    let mut out = Vec::new();
    let mut mid = Some(mid);
    while out.len() < FRAMES {
        if out.len() >= FRAMES / 2
            && let Some(mid) = mid.take()
        {
            mid();
        }
        out.extend(tutti_sampler::testing::block(pool, &t, RATE, BLOCK).swap_remove(0));
        t.advance(BLOCK as i64, RATE);
    }
    out
}

/// **An add at a live id is refused, and the voice there plays on.** The
/// refused add carries a wave of another value; the render is the resident
/// ramp, frame for frame, across the refusal, and the slot is free again once
/// its voice is removed.
#[test]
fn an_add_at_a_live_id_is_refused_and_the_resident_voice_plays_on() {
    let (mut pool, handle) = pool_with_resident_ramp();
    let mut flat = Wave::new(1, SampleRate::of(SR));
    for _ in 0..LEN {
        flat.append(Interleaved::new(&[0.5f32], flat.layout()).unwrap())
            .unwrap();
    }
    let got = play_memory_pool(&mut pool, || {
        let source = MemorySource::placed(Arc::new(flat), window()).expect("a placeable wave");
        assert_eq!(
            handle.add(SlotId(1), source, Playback::default()),
            Err(tutti_sampler::SendError::SlotInUse(SlotId(1)))
        );
    });
    assert_frames("resident ramp", &got, |n| n);

    handle.remove(SlotId(1)).expect("room");
    let source = MemorySource::placed(ramp(), window()).expect("a placeable wave");
    handle
        .add(SlotId(1), source, Playback::default())
        .expect("a removed id is free again");
}

/// **An add at a live id is refused on the streaming path too**: the stream is
/// handed back in the error, and the resident disk voice plays on.
#[test]
fn a_streaming_add_at_a_live_id_is_refused_and_the_resident_voice_plays_on() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, stream, reader) = streaming(dir.path());
    let voice = DiskVoice::from_reader(reader, window());
    let (pool, handle) = VoicePool::with_channels(tutti_buffers::ChannelLayout::MONO)
        .expect("mono is a width")
        .with_handle();
    handle
        .add_streaming(SlotId(1), voice, stream, Playback::default())
        .expect("room");
    let _ = streamer.step_until_idle(1_000);
    let mut pool = prepared_pool(pool);
    let other = dir.path().join("other.wav");
    write_ramp(&other);
    let opening = streamer.open_stream(&other, StreamStart::HEAD);
    let (second, second_reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    let got = play_pool_calling(&mut pool, &mut streamer, || {
        let refused = handle
            .add_streaming(
                SlotId(1),
                DiskVoice::from_reader(second_reader, window()),
                second,
                Playback::default(),
            )
            .expect_err("the id is live");
        assert_eq!(
            refused.error,
            tutti_sampler::SendError::SlotInUse(SlotId(1))
        );
    });
    assert_frames("resident disk voice", &got, |n| n);
}

/// **A command for an id holding no voice is refused, and nothing changes.**
/// Every command names `SlotId(2)`, which was never added; the voice at
/// `SlotId(1)` renders the ramp, frame for frame, across them. A removed id is
/// unknown again.
#[test]
fn a_command_for_an_id_with_no_voice_is_refused_and_changes_nothing() {
    use tutti_sampler::SendError::NoSuchSlot;
    use tutti_units::{Amplitude, Cents, StretchFactor};
    let (mut pool, handle) = pool_with_resident_ramp();
    let unknown = SlotId(2);
    let got = play_memory_pool(&mut pool, || {
        assert_eq!(handle.remove(unknown), Err(NoSuchSlot(unknown)));
        assert_eq!(
            handle.replace_wave(unknown, ramp()),
            Err(NoSuchSlot(unknown))
        );
        assert_eq!(
            handle.set_window(unknown, window()),
            Err(NoSuchSlot(unknown))
        );
        assert_eq!(
            handle.set_gain(unknown, Amplitude::new(0.0)),
            Err(NoSuchSlot(unknown))
        );
        assert_eq!(
            handle.set_speed(unknown, PlaybackRate::of(2.0)),
            Err(NoSuchSlot(unknown))
        );
        assert_eq!(
            handle.set_loop(unknown, Some(loop_frames(300, 700, 0))),
            Err(NoSuchSlot(unknown))
        );
        assert_eq!(
            handle.set_direction(unknown, PlayDirection::Reverse),
            Err(NoSuchSlot(unknown))
        );
        assert_eq!(
            handle.set_stretch(unknown, StretchFactor::new(2.0), Cents::new(0.0)),
            Err(NoSuchSlot(unknown))
        );
    });
    assert_frames("resident ramp", &got, |n| n);
    assert_eq!(handle.drops(), tutti_sampler::PoolDrops::default());

    handle.remove(SlotId(1)).expect("a live id");
    assert_eq!(
        handle.set_gain(SlotId(1), Amplitude::new(1.0)),
        Err(NoSuchSlot(SlotId(1)))
    );
    assert_eq!(handle.remove(SlotId(1)), Err(NoSuchSlot(SlotId(1))));
}

/// **A wave cannot replace a disk voice's**: the call names the slot and
/// queues nothing (the pool's count of ignored swaps stays zero), and the
/// voice renders the file's ramp across it.
#[test]
fn replace_wave_on_a_disk_voice_is_refused_and_queues_nothing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, stream, reader) = streaming(dir.path());
    let voice = DiskVoice::from_reader(reader, window());
    let (pool, handle) = VoicePool::with_channels(tutti_buffers::ChannelLayout::MONO)
        .expect("mono is a width")
        .with_handle();
    handle
        .add_streaming(SlotId(1), voice, stream, Playback::default())
        .expect("room");
    let _ = streamer.step_until_idle(1_000);
    let mut pool = prepared_pool(pool);
    let got = play_pool_calling(&mut pool, &mut streamer, || {
        assert_eq!(
            handle.replace_wave(SlotId(1), ramp()),
            Err(tutti_sampler::SendError::DiskVoice(SlotId(1)))
        );
    });
    assert_frames("disk voice", &got, |n| n);
    assert_eq!(handle.drops().wave_swaps_ignored, 0);
}

/// **A gain that is not finite is refused at the handles, and the voice keeps
/// its gain.** NaN and both infinities, on the pool's `set_gain` and on an add's
/// `Playback`, and on a node handle: each names `GainError::NotFinite` and the
/// voice renders the ramp at unity across the refusal.
#[test]
fn a_gain_that_is_not_finite_is_refused_and_the_voice_keeps_its_gain() {
    use tutti_sampler::{GainError, SendError};
    use tutti_units::Amplitude;
    let (mut pool, handle) = pool_with_resident_ramp();
    let bad = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY].map(Amplitude::new);
    let got = play_memory_pool(&mut pool, || {
        for gain in bad {
            assert_eq!(
                handle.set_gain(SlotId(1), gain),
                Err(SendError::Gain(GainError::NotFinite))
            );
            let source = MemorySource::placed(ramp(), window()).expect("a placeable wave");
            let play = Playback {
                gain,
                ..Playback::default()
            };
            assert_eq!(
                handle.add(SlotId(7), source, play),
                Err(SendError::Gain(GainError::NotFinite))
            );
        }
    });
    assert_frames("unity gain kept", &got, |n| n);

    let (mut node, node_handle) = memory_node();
    for gain in bad {
        assert_eq!(node_handle.set_gain(gain), Err(GainError::NotFinite));
    }
    assert_frames("node at unity", &render_with(&mut node, 400), |n| n);
}

/// **No public setter or constructor takes a gain that is not finite.** The
/// source's, the disk voice's and its controls' setters, a pool's `insert`, a
/// node's constructor and a clip's builder each name `GainError::NotFinite`,
/// and the one that holds a gain keeps the one it had.
#[test]
fn every_gain_entry_refuses_a_gain_that_is_not_finite() {
    use tutti_graph::{IntoNode, NodeParts};
    use tutti_sampler::{Clip, GainError, VoiceNodeError};
    use tutti_units::Amplitude;
    let bad = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY].map(Amplitude::new);
    let half = Amplitude::new(0.5);

    let source = MemorySource::placed(ramp(), window()).expect("a placeable wave");
    source.set_gain(half).expect("finite");
    let dir = tempfile::tempdir().expect("a temp dir");
    let (_streamer, _stream, reader) = streaming(dir.path());
    let disk = DiskVoice::from_reader(reader, window());
    disk.set_gain(half).expect("finite");
    let NodeParts { controls, .. } = disk.clone().into_parts();
    for gain in bad {
        assert_eq!(source.set_gain(gain), Err(GainError::NotFinite));
        assert_eq!(disk.set_gain(gain), Err(GainError::NotFinite));
        assert_eq!(controls.set_gain(gain), Err(GainError::NotFinite));

        let (mut pool, _handle) = VoicePool::new().with_handle();
        let play = Playback {
            gain,
            ..Playback::default()
        };
        let again = MemorySource::placed(ramp(), window()).expect("a placeable wave");
        assert_eq!(
            pool.insert(SlotId(1), again, play),
            Err(GainError::NotFinite)
        );
        assert_eq!(pool.voice_count(), 0, "nothing was inserted");

        let again = MemorySource::placed(ramp(), window()).expect("a placeable wave");
        assert_eq!(
            VoiceNode::new(again, play).err(),
            Some(VoiceNodeError::Gain(GainError::NotFinite))
        );
        assert_eq!(
            Clip::new(ramp()).expect("a clip").gain(gain).err(),
            Some(GainError::NotFinite)
        );
    }
    assert_eq!(source.gain(), half, "the source kept its gain");
    assert_eq!(disk.gain(), half, "the disk voice kept its gain");
}

/// **A voice width of no channels is refused where a pool or a node is built**,
/// not coerced to mono.
#[test]
fn a_pool_or_node_of_no_channels_is_refused() {
    use tutti_sampler::{PlaceError, VoiceNodeError};
    assert_eq!(
        VoicePool::<f32>::with_channels(0u16).err(),
        Some(PlaceError::EmptyWidth)
    );
    let source = MemorySource::placed(ramp(), window()).expect("a placeable wave");
    assert_eq!(
        VoiceNode::with_channels(source, Playback::default(), 0u16).err(),
        Some(VoiceNodeError::Width(PlaceError::EmptyWidth))
    );
}
