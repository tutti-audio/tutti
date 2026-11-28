//! A loop is one value on every tier, a node's controls steer the voice it
//! holds, and a gain that is not finite is refused where it is made.
//!
//! The material is a ramp whose value *is* its frame index, played at the file's
//! own rate on a transport that moves one file frame per output frame, so
//! output frame `n` is the file frame the voice read: every figure below is an
//! analytic position (a loop repeats `[start, end)`; a speed of 2 reads every
//! second frame), never one render compared with another.

use std::path::Path;
use std::sync::Arc;
use tutti_buffers::{ChannelLayout, Interleaved};

use tutti_buffers::Wave;
use tutti_sampler::testing::{
    MockTransport, controlled, controlled_pool, free_config, loop_frames, placed_config,
    placed_source, play, source_config, voice_node, wait_for_open,
};
use tutti_sampler::{
    DiskStreamer, DiskStreamerConfig, DiskVoice, Loop, LoopError, Manual, MemorySourceConfig,
    PlayDirection, Playback, SlotId, StreamHandle, StreamStart, VoiceNode, VoiceNodeConfig,
    VoiceNodeControls, VoicePool, VoicePoolConfig, VoiceWindow,
};
use tutti_units::{Amplitude, Beat, Bpm, Cents, PlaybackRate, SampleRate, Samples, StretchFactor};

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
fn memory_node() -> (VoiceNode<f32>, VoiceNodeControls<f32>) {
    let source = source_config(ramp(), ChannelLayout::MONO, Some(window()));
    controlled(voice_node(source, Playback::default()))
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
    let (mut node, handle) = controlled(VoiceNode::from(
        VoiceNodeConfig::streaming(
            voice,
            stream,
            tutti_units::Amplitude::new(1.0),
            tutti_units::StretchFactor::UNITY,
            tutti_units::Cents::new(0.0),
        )
        .expect("a finite gain and a nonempty width"),
    ));
    handle
        .looping_cell()
        .publish(Some(loop_frames(300, 700, 0)));
    let _ = streamer.step_until_idle(1_000);
    let got = render_with(&mut node, FRAMES);
    assert_frames("disk", &got, |n| looped_frame(n, 300, 700));
}

/// **A streamed config builds a live voice every time**: a node built from it
/// takes the stream's one reader and hands it back when it drops, so the build
/// after plays the ramp from the stream, not the silent template.
#[test]
fn a_streamed_config_builds_a_live_voice_after_its_node_drops() {
    use tutti_graph::{HostValues, NodeConfig};
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, stream, reader) = streaming(dir.path());
    let config = VoiceNodeConfig::streaming(
        DiskVoice::from_reader(reader, window()),
        stream,
        tutti_units::Amplitude::new(1.0),
        tutti_units::StretchFactor::UNITY,
        tutti_units::Cents::new(0.0),
    )
    .expect("a finite gain and a nonempty width");
    for build in ["first", "second"] {
        let mut node = VoiceNode::from(config.clone());
        let _ = streamer.step_until_idle(1_000);
        let got = render_with(&mut node, 400);
        assert_frames(build, &got, |n| n);
        drop(node);
    }
    let node = config.build(&HostValues::new());
    drop(node);
}

/// A streamed config over the ramp, and the streamer that feeds it.
fn streamed_config(
    dir: &Path,
) -> (
    DiskStreamer<Manual<f32>, f32>,
    VoiceNodeConfig<f32, tutti_sampler::Streamed>,
) {
    let (streamer, stream, reader) = streaming(dir);
    let config = VoiceNodeConfig::streaming(
        DiskVoice::from_reader(reader, window()),
        stream,
        tutti_units::Amplitude::new(1.0),
        tutti_units::StretchFactor::UNITY,
        tutti_units::Cents::new(0.0),
    )
    .expect("a finite gain and a nonempty width");
    (streamer, config)
}

/// **A node built while another holds the stream's reader waits for it**: it
/// renders silence and reads `Waiting`, takes the reader between blocks once the
/// holder has dropped, and then plays the ramp and reads `Holding`.
#[test]
fn a_node_built_while_another_holds_the_reader_takes_it_when_that_one_drops() {
    use tutti_sampler::StreamHold;
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, config) = streamed_config(dir.path());
    let (holder, holder_controls) = controlled(VoiceNode::from(config.clone()));
    let (mut waiter, waiter_controls) = controlled(VoiceNode::from(config));
    assert_eq!(holder_controls.stream_hold(), Some(StreamHold::Holding));
    assert_eq!(waiter_controls.stream_hold(), Some(StreamHold::Waiting));
    assert!(
        render_with(&mut waiter, 128).iter().all(|&x| x == 0.0),
        "the waiter is silent while the holder has the reader"
    );
    assert_eq!(waiter_controls.stream_hold(), Some(StreamHold::Waiting));

    drop(holder);
    let _ = streamer.step_until_idle(1_000);
    assert_frames(
        "the waiter once it holds",
        &render_with(&mut waiter, 400),
        |n| n,
    );
    assert_eq!(waiter_controls.stream_hold(), Some(StreamHold::Holding));
}

/// **A waiter that drops before it is served leaves the reader to the next
/// build**: the holder's drop finds no waiter, so the reader goes home, and a
/// third build plays the ramp.
#[test]
fn a_waiter_dropped_first_leaves_the_reader_to_the_next_build() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, config) = streamed_config(dir.path());
    let holder = VoiceNode::from(config.clone());
    let waiter = VoiceNode::from(config.clone());
    drop(waiter);
    drop(holder);
    let mut third = VoiceNode::from(config);
    let _ = streamer.step_until_idle(1_000);
    assert_frames("the third build", &render_with(&mut third, 400), |n| n);
}

/// **A waiter that drops after it was lent the reader, before it took it, passes
/// the reader on**: the holder's drop lends it to the waiter, the waiter's drop
/// recalls it, and a third build plays the ramp.
#[test]
fn a_waiter_dropped_after_it_was_lent_the_reader_passes_it_on() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (mut streamer, config) = streamed_config(dir.path());
    let holder = VoiceNode::from(config.clone());
    let waiter = VoiceNode::from(config.clone());
    drop(holder);
    drop(waiter);
    let mut third = VoiceNode::from(config);
    let _ = streamer.step_until_idle(1_000);
    assert_frames("the third build", &render_with(&mut third, 400), |n| n);
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

/// **A disk voice in a pool's row keeps its loop and direction.** The row's
/// `looped` and `direction` are written to its stream's cells: the loop it
/// carries plays the span, and a reversed one plays the file from its end.
#[test]
fn a_disk_voice_added_to_a_pool_keeps_its_loop_and_direction() {
    for direction in [PlayDirection::Forward, PlayDirection::Reverse] {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (mut streamer, stream, reader) = streaming(dir.path());
        let voice = DiskVoice::from_reader(reader, window());
        let row = VoiceNodeConfig::streaming(
            voice,
            stream,
            Amplitude::new(1.0),
            StretchFactor::UNITY,
            Cents::new(0.0),
        )
        .expect("a finite gain and a nonempty width")
        .looped(loop_frames(300, 700, 0))
        .direction(direction);
        let config = VoicePoolConfig::<f32>::new(ChannelLayout::MONO)
            .expect("mono is a width")
            .row(SlotId(1), row)
            .expect("a free key");
        let (pool, controls) = controlled_pool(VoicePool::from(config));
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
        drop(controls);
    }
}

/// `FRAMES` of `pool` from beat 0, the streamer stepped between blocks as a
/// running one is, so a stream that turns or loops has its ring refilled.
fn play_pool(pool: &mut VoicePool<f32>, streamer: &mut DiskStreamer<Manual<f32>, f32>) -> Vec<f32> {
    let t = rolling();
    let mut out = Vec::new();
    while out.len() < FRAMES {
        out.extend(tutti_sampler::testing::block(pool, &t, RATE, BLOCK).swap_remove(0));
        t.advance(BLOCK as i64, RATE);
        let _ = streamer.step_until_idle(1_000);
    }
    out
}

/// **The controls' speed cell sets a memory voice's speed, and stop and resume a
/// free-running one.** At speed 2 the voice reads every second file frame; a
/// placed voice has no steering to stop or play; a free-running one, stopped,
/// is silent and reads nothing, and resumes where its cursor stopped.
#[test]
fn a_nodes_controls_set_speed_and_stop_and_resume_a_free_running_voice() {
    let (mut node, handle) = memory_node();
    handle.speed_cell().publish(PlaybackRate::of(2.0));
    let got = render_with(&mut node, 400);
    assert_frames("speed 2", &got, |n| 2 * n);
    assert!(
        handle.steering().is_none(),
        "a placed voice has no steering"
    );

    let source = source_config(ramp(), ChannelLayout::MONO, None);
    let (mut node, handle) = controlled(voice_node(source, Playback::default()));
    node = tutti_sampler::testing::prepared(node, RATE, BLOCK);
    let t = rolling();
    let steering = handle.steering().expect("a free-running voice");
    let mut heard = Vec::new();
    for block in 0..6 {
        match block {
            2 => steering.stop(),
            4 => steering.play(),
            _ => {}
        }
        heard.extend(tutti_sampler::testing::block(&mut node, &t, RATE, BLOCK).swap_remove(0));
        t.advance(BLOCK as i64, RATE);
    }
    assert!(steering.is_playing());
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
    voice_node(source_config(ramp(), ChannelLayout::MONO, None), play)
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

/// **`play` starts a free-running voice a reset left stopped**: after the
/// node is reset (which rewinds and stops its cursor) it is silent until
/// `play`, which sounds it from the ramp's first frame. The transport rolls on
/// across the blocks, so nothing else flushes the voice.
#[test]
fn play_restarts_a_free_running_voice_after_a_reset() {
    use tutti_graph::Node;
    let (node, handle) = controlled(free_node(Playback::default()));
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
    handle.steering().expect("a free-running voice").play();
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

/// **A gain that is not finite is refused at a node's controls, and the voice
/// keeps its gain.** NaN and both infinities each name `GainError::NotFinite`,
/// and the voice renders the ramp at unity across the refusals.
#[test]
fn a_gain_that_is_not_finite_is_refused_and_the_voice_keeps_its_gain() {
    use tutti_sampler::GainError;
    let bad = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY].map(Amplitude::new);
    let (mut node, node_handle) = memory_node();
    for gain in bad {
        assert_eq!(node_handle.set_gain(gain), Err(GainError::NotFinite));
    }
    assert_frames("node at unity", &render_with(&mut node, 400), |n| n);
}

/// **No public setter or constructor takes a gain that is not finite.** The
/// source's, the disk voice's and its controls' setters, a
/// node's constructor and a clip's builder each name `GainError::NotFinite`,
/// and the one that holds a gain keeps the one it had.
#[test]
fn every_gain_entry_refuses_a_gain_that_is_not_finite() {
    use tutti_graph::{IntoNode, NodeParts};
    use tutti_sampler::{Clip, GainError, VoiceNodeError};
    let bad = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY].map(Amplitude::new);
    let half = Amplitude::new(0.5);

    let source = placed_source(ramp(), window());
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

        assert_eq!(
            VoiceNodeConfig::placed(placed_config(ramp(), window()).gain(gain)).err(),
            Some(VoiceNodeError::Gain(GainError::NotFinite))
        );
        assert_eq!(
            VoiceNodeConfig::free(free_config(ramp()).gain(gain)).err(),
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

/// **A voice width of no channels is refused where a pool config or a node is built**,
/// not coerced to mono.
#[test]
fn a_pool_or_node_of_no_channels_is_refused() {
    use tutti_sampler::PlaceError;
    assert_eq!(
        VoicePoolConfig::<f32>::new(0u16).err(),
        Some(PlaceError::EmptyWidth)
    );
    assert_eq!(
        MemorySourceConfig::new(ramp(), 0u16).err(),
        Some(PlaceError::EmptyWidth)
    );
}
