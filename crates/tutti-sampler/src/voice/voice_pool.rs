//! Tests for the voice pool, kept in one module because they exercise the
//! pieces in combination — a command sent through the handle, drained by the
//! pool, rendered through a slot, summed — and because many of them reach
//! private state (`slot.stretch`, `voices`) that a sibling module could not see.
//!
//! The implementation is one file per duty: [`types`](super::types),
//! [`slot`](super::slot), [`command`](super::command), [`pool`](super::pool),
//! [`node`](super::node).

use crate::testing::loop_frames;
use tutti_buffers::Mono;

use super::command::*;
use super::memory_source::{MemorySource, VoiceWindow};
use super::node::*;
use super::pool::*;
use super::types::*;
use crate::PlaceError;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use tutti_buffers::ChannelLayout;
use tutti_buffers::MaxBlock;
use tutti_buffers::Wave;
use tutti_graph::{IntoNode, Node, NodeParts, Prepare};
use tutti_rt::Mutex;
use tutti_units::SampleRate;
use tutti_units::{Amplitude, Beat, Cents, Seconds, StretchFactor};

use crate::voice::memory_source::MemorySourceConfig;
use tutti_units::{BeatDuration, Bpm, Samples};

use crate::testing::MockTransport;

/// The rate the tests' blocks run at, unless a test says otherwise.
const SR: f64 = 44_100.0;

/// A command queued as given, the way a test drives the drain: the handle's
/// own queueing under its one lock, for a command no public method builds.
trait QueueRaw {
    fn send(&self, cmd: VoiceCommand<f32>) -> Result<(), SendError>;
}

impl QueueRaw for VoicePoolHandle<f32> {
    fn send(&self, cmd: VoiceCommand<f32>) -> Result<(), SendError> {
        let cmd = self.prepare(cmd);
        self.send_locked(&mut self.lock(), cmd)
    }
}

/// One one-frame block of `node` under `t` at `rate`, into `out` (as many
/// channels as it holds); the transport is not moved.
fn tick(node: &mut dyn Node, t: &MockTransport, rate: f64, out: &mut [f32]) {
    let got = crate::testing::block(node, t, SampleRate::of(rate), 1);
    for (o, c) in out.iter_mut().zip(got) {
        *o = c[0];
    }
}

/// One block of `n` frames of `node` under `t` at `rate`, planar; the
/// transport is not moved.
fn run(node: &mut dyn Node, t: &MockTransport, rate: f64, n: usize) -> Vec<Vec<f32>> {
    crate::testing::block(node, t, SampleRate::of(rate), n)
}

/// `node` as the graph takes it: the unit it renders, and the source a
/// fork of it comes from.
fn live_and_fork(node: impl IntoNode) -> (Box<dyn Node>, Box<dyn tutti_graph::ForkSource>) {
    let NodeParts { node, fork, .. } = node.into_parts();
    (node, fork.expect("a forkable node"))
}

/// A fork of the node `source` is the fork source of, as a live
/// duplicate takes it, prepared at `rate`.
fn fork_of(source: &dyn tutti_graph::ForkSource, rate: f64) -> Box<dyn Node> {
    let mut node = source
        .fork(tutti_graph::RenderMode::Realtime)
        .expect("a memory voice forks")
        .node;
    node.prepare(&Prepare::new(
        SampleRate::of(rate),
        MaxBlock::of(Samples::new(64)),
    ));
    node
}

fn make_wave(samples: usize) -> Arc<Wave> {
    let data: Vec<f32> = (0..samples)
        .map(|i| (i as f32 + 1.0) / samples as f32)
        .collect();
    Arc::new(Wave::from_mono(SampleRate::SR_44K1, Mono::new(&data)))
}

/// Send an in-memory voice through the one `AddVoice` path — the test-setup
/// mirror of the timeline's `promote_pending_clip_waves` emit.
fn add_ram_clip(handle: &VoicePoolHandle<f32>, id: SlotId, sampler: MemorySource<f32>) {
    handle
        .send(VoiceCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback::default(),
            }),
            stretch: None,
        })
        .expect("the command queue has room in a test");
}

/// Resetting a fork must not reach the live voice.
///
/// A fork is taken while the live voice plays and is reset and rendered
/// on another thread. Had it shared the live filter's state, its reset
/// would clear the FIFOs and phase accumulators of the *live* unit —
/// measurable as live output dropping to exactly 0.0 for one window. This
/// pins that whatever the render does to its fork, the live voice keeps
/// its state.
///
/// This pins, at the node, what a shared filter would break: a fork's node builds its
/// own filter (`VoiceNode::with_channels`), and a filter owns its vocoders by value, so
/// the types rule a shared one out (see the test below).
#[test]
fn resetting_a_fork_leaves_the_live_voice_playing() {
    // Matches `make_wave`, so the source needs no rate conversion.
    const SR: f64 = 44_100.0;
    // Long enough to outlast the vocoder's fill-up: at 2x stretch the unit
    // consumes source at 1/2 rate, so 8192 warm-up ticks eat 4096 samples
    // and the 2048-sample window needs 4096 ticks before anything is emitted.
    let wave = make_wave(48_000);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    let node = VoiceNode::with_channels(
        sampler,
        Playback {
            stretch: StretchFactor::new(2.0),
            ..Playback::default()
        },
        1u16,
    )
    .expect("a finite gain and a nonempty width");
    assert!(node.slot.stretch.is_some(), "vacuous without a filter");
    let (mut live, source) = live_and_fork(node);
    live.prepare(&Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(64)),
    ));

    // Build real vocoder history. The playhead must ADVANCE: a placed
    // source derives its position from the transport every tick, so a frozen
    // playhead feeds the vocoder a constant and it synthesises nothing.
    let mut out = [0.0f32; 1];
    let mut peak_before = 0.0f32;
    for _ in 0..8192 {
        tick(&mut *live, &transport, SR, &mut out);
        transport.advance(1, SampleRate::of(SR));
        peak_before = peak_before.max(out[0].abs());
    }
    assert!(
        peak_before > 1e-4,
        "the live voice must be audible before the render starts"
    );

    // What a fork's renderer does: fork, then reset (and render) it.
    let mut render = fork_of(&*source, SR);
    render.reset();
    run(&mut *render, &transport, SR, 64);

    // The live voice must be unaffected — it keeps emitting immediately,
    // with no re-fill gap.
    let mut peak_after = 0.0f32;
    for _ in 0..512 {
        tick(&mut *live, &transport, SR, &mut out);
        transport.advance(1, SampleRate::of(SR));
        peak_after = peak_after.max(out[0].abs());
    }
    assert!(
        peak_after > 1e-4,
        "a region render silenced the live voice \
             (peak {peak_before:.6} -> {peak_after:.6})"
    );
}

/// Every node type the offline render can carry must share no running
/// state with the live node.
///
/// A fork renders on another thread while the audio thread plays the
/// original. `VoicePool`'s fork is an empty pool; `VoiceNode`'s holds a
/// copy of its voice, so its stretch vocoder must be the fork's own.
///
/// Asserted on audio: a live voice whose fork was taken mid-play, reset
/// and rendered for 4096 frames plays, sample for sample, what an
/// identical voice that was never forked plays.
///
/// This pins, at the voice, what a reintroduced shared vocoder would break.
#[test]
fn a_standalone_voices_fork_shares_no_stretch_state() {
    const SR: f64 = 44_100.0;
    let wave = make_wave(48_000);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let node = || {
        let sampler = MemorySource::placed(Arc::clone(&wave), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave");
        VoiceNode::with_channels(
            sampler,
            Playback {
                stretch: StretchFactor::new(2.0),
                ..Playback::default()
            },
            1u16,
        )
        .expect("a finite gain and a nonempty width")
    };
    let (live, mut twin) = (node(), node());
    assert!(
        live.slot.stretch.is_some(),
        "test is vacuous unless a filter is resident"
    );
    let (mut live, source) = live_and_fork(live);
    live.prepare(&Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(64)),
    ));
    twin.prepare(&Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(64)),
    ));
    let mut out = [0.0f32; 1];
    let mut out_twin = [0.0f32; 1];
    for _ in 0..4096 {
        tick(&mut *live, &transport, SR, &mut out);
        tick(&mut twin, &transport, SR, &mut out_twin);
        transport.advance(1, SampleRate::of(SR));
    }

    let mut render = fork_of(&*source, SR);
    render.reset();
    let mut scratch = [0.0f32; 1];
    for _ in 0..4096 {
        tick(&mut *render, &transport, SR, &mut scratch);
    }

    let mut heard = false;
    for i in 0..8192 {
        tick(&mut *live, &transport, SR, &mut out);
        tick(&mut twin, &transport, SR, &mut out_twin);
        transport.advance(1, SampleRate::of(SR));
        assert_eq!(
            out[0].to_bits(),
            out_twin[0].to_bits(),
            "frame {i}: the render's clone reached the live voice's filter"
        );
        heard |= out[0].abs() > 1e-4;
    }
    assert!(heard, "silence proves nothing");
}

/// A pool's fork holds copies of the voices the pool was built with and
/// shares nothing with the live pool: a voice the live handle adds later is
/// not in it (one voice's ramp, not two), and a
/// pool whose handle has queued a command is not forked at all (its voices
/// are on the audio thread).
#[test]
fn a_pools_fork_shares_nothing_with_the_live_pool() {
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut pool = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(
                MemorySource::placed(make_wave(4096), VoiceWindow::from(Beat::new(0.0)))
                    .expect("a placeable wave"),
            ),
            play: Playback::default(),
        },
    );
    let NodeParts {
        node: mut live,
        controls: handle,
        fork,
        ..
    } = pool.into_parts();
    let source = fork.expect("a pool forks");
    live.prepare(&Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(64)),
    ));
    let mut fork = source
        .fork(tutti_graph::RenderMode::Realtime)
        .expect("a pool forks")
        .node;
    fork.prepare(&Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(64)),
    ));
    add_ram_clip(
        &handle,
        SlotId(2),
        MemorySource::placed(make_wave(4096), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave"),
    );
    let live_block = run(&mut *live, &transport, SR, 64);
    let fork_block = run(&mut *fork, &transport, SR, 64);
    // The wave is a ramp, frame `k` is `(k + 1) / 4096`, which a placed voice
    // at the render's rate reads back exactly, on each channel a mono wave
    // is placed on: the fork has the one voice it was built with.
    for k in 8..56 {
        let ramp = (k as f32 + 1.0) / 4096.0;
        assert!(
            (fork_block[0][k] - ramp).abs() < 1e-6,
            "frame {k}: the fork reads {}, one voice's ramp is {ramp}",
            fork_block[0][k]
        );
        assert!(
            (live_block[0][k] - 2.0 * ramp).abs() < 1e-6,
            "frame {k}: the live pool plays both its voices"
        );
    }
    assert!(
        source.fork(tutti_graph::RenderMode::Realtime).is_err(),
        "a pool whose handle queued a command has no copy of its voices"
    );
}

/// A transport seek must flush the vocoder's buffered audio.
///
/// A seek reaches a node only as its block's `Env` (a transport change,
/// or a block that does not continue the last), and nothing on any
/// transport-driven path calls `Node::reset`. So when the playhead
/// jumps, the placement gate re-derives the new position correctly and
/// immediately, while a vocoder would keep draining input taken in from
/// *before* the jump — up to two windows of frames per channel, plus
/// per-bin phase accumulators still tracking the old material — unless
/// the node flushes it on the jump's frame.
///
/// The wave is loud in its first half and exactly silent in its second,
/// which is what makes the assertion about the leak rather than about
/// liveness. Every other stretch test on this path asserts only `!= 0.0` or
/// `> 1e-6` (`clips_sum_together`, `six_channel_clip_with_stretch_...`), and
/// a leak at signal level passes all of them — the same blind spot that let
/// a 60 dB gain error live in the vocoder. Here, parking the playhead in the
/// silent half means any output above the floor is provably material the
/// filter should not still be holding.
#[test]
fn a_transport_seek_flushes_stretch_state() {
    const SR: f64 = 44_100.0;
    // Ten seconds, so the playhead has room to run for thousands of blocks
    // inside one half without leaving it.
    const LEN: usize = 441_000;
    // 120 BPM = 2 beats/s, and a placed read stretched 2x takes half a
    // source frame per output frame, so the wave spans 40 beats and the
    // halves split at beat 20.
    const LOUD_BEAT: f64 = 2.0;
    const SILENT_BEAT: f64 = 30.0;

    // Loud first half at 3 kHz (a real signal — the vocoder needs a changing
    // input to synthesise from), exactly silent second half.
    let data: Vec<f32> = (0..LEN)
        .map(|i| {
            if i < LEN / 2 {
                0.5 * (((std::f64::consts::TAU * 3000.0 * i as f64 / SR).sin()) as f32)
            } else {
                0.0
            }
        })
        .collect();
    let wave = Arc::new(Wave::from_mono(SampleRate::of(SR), Mono::new(&data)));

    let transport = MockTransport::rolling(Beat::new(SILENT_BEAT), Bpm::of(120.0));
    let (mut unit, handle) = VoicePool::with_channels(1u16)
        .expect("a width the sampler reads")
        .with_handle();

    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    handle
        .send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
            }),
            // The sender materialises the filter on the control thread, as
            // `VoicePoolHandle::send` does for a voice that arrives
            // already needing one.
            stretch: None,
        })
        .expect("the command queue has room in a test");

    let mut out = [0.0f32; 1];
    tick(&mut unit, &transport, SR, &mut out);
    assert!(
        unit.voices[0].stretch.is_some(),
        "test is vacuous unless the stretch path is live"
    );

    // Advance the playhead one sample per tick, the way a real transport
    // moves — a frozen playhead would make every frame re-derive the same
    // position, so the source would emit a constant and the vocoder would
    // have nothing to synthesise from.
    let drive = |unit: &mut VoicePool<f32>, n: usize| {
        let mut peak = 0.0f32;
        let mut out = [0.0f32; 1];
        for _ in 0..n {
            tick(unit, &transport, SR, &mut out);
            transport.advance(1, SampleRate::of(SR));
            peak = peak.max(out[0].abs());
        }
        peak
    };

    // Settle in the silent half: whatever the filter emits here is the floor.
    drive(&mut unit, 8192);

    // Seek backwards into the loud half and prime the filter with it.
    transport.set_beat(Beat::new(LOUD_BEAT));
    let loud = drive(&mut unit, 8192);
    assert!(
        loud > 0.05,
        "the loud half should be audible after seeking into it; peak {loud}"
    );

    // Seek back into the silent half. The source is silent from this playhead
    // on, so anything above the floor is stale.
    transport.set_beat(Beat::new(SILENT_BEAT));
    let after_seek = drive(&mut unit, 2048);

    assert!(
        after_seek < 0.01,
        "vocoder leaked pre-seek audio across a transport jump: \
             peak {after_seek} (the source is silent at this playhead)"
    );
}

/// The same seek flush, for a STANDALONE voice. `VoiceNode` is the other
/// node that plays a stretched voice, and it had no cursor at all — so the
/// pool-level fix covered one of the two paths and left this one smearing.
///
/// Not hypothetical: a spectral resynth adds bare `VoiceNode`s as correction
/// nodes, and a timeline clip is one. A scrub across a stretched correction would drag pre-seek audio
/// over the new region with nothing in the suite to notice, because every
/// other assertion on this path checks only that output is non-zero.
#[test]
fn a_transport_seek_flushes_a_standalone_voice_node() {
    const SR: f64 = 44_100.0;
    const LEN: usize = 441_000;
    // Beat 30 is source frame 330 750 at 2x, in the silent half: see the
    // pool's test above.
    const LOUD_BEAT: f64 = 2.0;
    const SILENT_BEAT: f64 = 30.0;

    let data: Vec<f32> = (0..LEN)
        .map(|i| {
            if i < LEN / 2 {
                0.5 * (((std::f64::consts::TAU * 3000.0 * i as f64 / SR).sin()) as f32)
            } else {
                0.0
            }
        })
        .collect();
    let wave = Arc::new(Wave::from_mono(SampleRate::of(SR), Mono::new(&data)));

    let transport = MockTransport::rolling(Beat::new(SILENT_BEAT), Bpm::of(120.0));
    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    let mut node = VoiceNode::with_channels(
        sampler,
        Playback {
            stretch: StretchFactor::new(2.0),
            ..Playback::default()
        },
        1u16,
    )
    .expect("a finite gain and a nonempty width");
    assert!(
        node.slot.stretch.is_some(),
        "test is vacuous unless the stretch path is live"
    );

    let drive = |node: &mut VoiceNode<f32>, n: usize| {
        let mut peak = 0.0f32;
        let mut out = [0.0f32; 1];
        for _ in 0..n {
            tick(node, &transport, SR, &mut out);
            transport.advance(1, SampleRate::of(SR));
            peak = peak.max(out[0].abs());
        }
        peak
    };

    drive(&mut node, 8192);

    transport.set_beat(Beat::new(LOUD_BEAT));
    let loud = drive(&mut node, 8192);
    assert!(
        loud > 0.05,
        "the loud half should be audible after seeking into it; peak {loud}"
    );

    transport.set_beat(Beat::new(SILENT_BEAT));
    let after_seek = drive(&mut node, 2048);
    assert!(
        after_seek < 0.01,
        "a standalone VoiceNode leaked pre-seek audio across a transport \
             jump: peak {after_seek} (the source is silent at this playhead)"
    );
}

/// The standalone twin of the pool's false-positive guard: a `VoiceNode`
/// playing continuously must never flush.
///
/// A cursor whose jump threshold is too tight resets the vocoder every block,
/// turning its output into a stutter of ~50 ms fragments. Nothing else here
/// would see it — every other assertion on this path is `!= 0.0`, and a
/// stuttering stretcher is still non-zero.
///
#[test]
fn continuous_playback_does_not_flush_a_standalone_voice_node() {
    const SR: f64 = 44_100.0;
    const LEN: usize = 441_000;

    let data: Vec<f32> = (0..LEN)
        .map(|i| 0.5 * (((std::f64::consts::TAU * 3000.0 * i as f64 / SR).sin()) as f32))
        .collect();
    let wave = Arc::new(Wave::from_mono(SampleRate::of(SR), Mono::new(&data)));

    let transport = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    let mut node = VoiceNode::with_channels(
        sampler,
        Playback {
            stretch: StretchFactor::new(2.0),
            ..Playback::default()
        },
        1u16,
    )
    .expect("a finite gain and a nonempty width");

    let mut out = [0.0f32; 1];
    for _ in 0..16_384 {
        tick(&mut node, &transport, SR, &mut out);
        transport.advance(1, SampleRate::of(SR));
    }

    let mut quiet_blocks = 0usize;
    for _ in 0..256 {
        let mut peak = 0.0f32;
        for _ in 0..64 {
            tick(&mut node, &transport, SR, &mut out);
            transport.advance(1, SampleRate::of(SR));
            peak = peak.max(out[0].abs());
        }
        if peak < 0.01 {
            quiet_blocks += 1;
        }
    }
    assert_eq!(
        quiet_blocks, 0,
        "continuous playback flushed the filter: {quiet_blocks}/256 blocks fell silent"
    );
}

/// A stretched placed voice must not transpose, across block boundaries.
///
/// What this pins is the *assembly*, not the DSP: if `VoicePool` did not
/// feed the vocoder the source frames it asks for, the vocoder would be fed one
/// source sample per output sample and the factor would act as plain varispeed
/// (2.0x turning 440 Hz into 880 Hz with the duration unchanged), while
/// every test of the vocoder alone still passed.
///
/// Why this renders many blocks. The failure lives at block boundaries.
/// A placed voice re-derives its origin from the playhead each block, and the
/// playhead runs at wall clock, so a stretched read that covers
/// `block / stretch` source samples gets re-seated a full `block` further on
/// at the next boundary — discarding the stretch, forever. A single-block
/// test cannot see it, and applying the stretch to the within-block step
/// alone makes it *worse* (pitch +35% off, spectral purity 0.95 -> 0.54)
/// rather than failing outright.
///
/// Asserted by measuring the dominant frequency, because every other stretch
/// assertion on this path is `!= 0.0` — and an octave-transposed voice is
/// emphatically non-zero. Found by rendering WAVs and analysing them in
/// numpy (`examples/render_cases.rs`); this is that check brought in-tree so
/// CI can see it.
#[test]
fn a_stretched_placed_voice_holds_its_pitch_across_blocks() {
    const SR: f64 = 48_000.0;
    const BLOCK: usize = 64;
    const BLOCKS: usize = 750;

    for factor in [0.5f64, 1.5, 2.0] {
        let (mut pool, handle) = VoicePool::new().with_handle();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));

        // 440 Hz, long enough that even the fastest consumption stays inside
        // the material for the whole render.
        let data: Vec<f32> = (0..(SR as usize * 4))
            .map(|i| ((std::f64::consts::TAU * 440.0 * i as f64 / SR).sin()) as f32)
            .collect();
        let wave = Arc::new(Wave::from_mono(SampleRate::of(SR), Mono::new(&data)));

        let source = MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave");
        let play = Playback {
            stretch: StretchFactor::new(factor),
            ..Default::default()
        };
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(1),
                voice: Box::new(Voice {
                    source: VoiceSource::Memory(source),
                    play,
                }),
                stretch: None,
            })
            .expect("the command queue has room in a test");

        pool.prepare(&Prepare::new(
            SampleRate::of(SR),
            MaxBlock::of(Samples::new(BLOCK)),
        ));
        let mut out = Vec::with_capacity(BLOCK * BLOCKS);
        for _ in 0..BLOCKS {
            out.extend(run(&mut pool, &transport, SR, BLOCK).swap_remove(0));
            transport.advance(BLOCK as i64, SampleRate::of(SR));
        }

        // Measure well past the vocoder's fill, over a whole number of
        // periods' worth of samples.
        let settled = &out[24_000..24_000 + 8192];
        let got = dominant_hz(settled, SR as f32);
        assert!(
            (got - 440.0).abs() < 440.0 * 0.02,
            "stretch {factor}x transposed a placed voice to {got:.1} Hz \
                 (the source is 440 Hz; stretch must change duration, not pitch)"
        );
    }
}

/// A voice node reads a placed wave at another rate the same in
/// one-frame blocks as in 64-frame ones, the clock moving by each
/// block: a 24 kHz ramp on a 48 kHz clock reads file frame `n / 2` on
/// output frame `n`, forward and reversed, across every block boundary.
#[test]
fn a_voice_node_reads_a_wave_at_another_rate_at_any_block_length() {
    const LEN: usize = 4_096;
    let data: Vec<f32> = (0..LEN).map(|i| (i + 1) as f32).collect();
    let wave = Arc::new(Wave::from_mono(SampleRate::of(24_000.0), Mono::new(&data)));
    for direction in [PlayDirection::Forward, PlayDirection::Reverse] {
        for via_tick in [false, true] {
            let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
            let sampler =
                MemorySource::placed(Arc::clone(&wave), VoiceWindow::from(Beat::new(0.0)))
                    .expect("a placeable wave");
            let mut node = VoiceNode::with_channels(
                sampler,
                Playback {
                    direction,
                    ..Playback::default()
                },
                1u16,
            )
            .expect("a finite gain and a nonempty width");
            node.prepare(&Prepare::new(
                SampleRate::SR_48K,
                MaxBlock::of(Samples::new(64)),
            ));
            let len = if via_tick { 1 } else { 64 };
            let out = crate::testing::play(&mut node, &transport, SampleRate::SR_48K, 512, len)
                .swap_remove(0);
            for (n, &got) in out.iter().enumerate().skip(4) {
                let at = n as f32 / 2.0;
                let want = match direction {
                    PlayDirection::Forward => at + 1.0,
                    PlayDirection::Reverse => LEN as f32 - at,
                };
                assert!(
                    (got - want).abs() < 1e-2,
                    "{direction:?}, one-frame blocks {via_tick}: output frame {n} read {got}, want {want}"
                );
            }
        }
    }
}

/// The disk tier must consume its source at the stretched rate too.
///
/// The memory tier's fix scales a cursor; the disk tier has no cursor to
/// scale. It pops from a streamer-fed ring, advancing an internal
/// `fractional_pos` by `Cells::read_rate` (speed × src_ratio), so the rate
/// has to reach *that* accumulator instead.
///
/// Asserted as source consumption, not as pitch. A ring-fed reader has no
/// absolute position to measure a frequency against — it renders whatever the
/// streamer last handed it — so the observable property is how fast the ring
/// drains: at stretch `s` the vocoder takes `1 / s` source frames per output
/// frame, whatever the block size. Over 128 blocks of 64 frames (8192 output
/// frames) past the vocoder's start, the ring drains `8192 / s` frames, to within
/// the hop's worth of source the vocoder reads in steps of: each reading of the
/// position is within `hop / s` of the stream's, and two are compared, with one
/// more for the burst the last block read.
///
/// This is the same defect as
/// `a_stretched_placed_voice_holds_its_pitch_across_blocks`, in the tier
/// where it shows up differently.
#[test]
fn a_stretched_disk_voice_consumes_its_source_at_the_stretched_rate() {
    use crate::streamer::Cells;

    const BLOCK: usize = 64;
    const WARM: usize = 64;
    const MEASURED: usize = 128;
    /// The vocoder's hop, in frames.
    const HOP: f64 = 512.0;

    use crate::streamer::{RegionBuffer, StreamId};
    use crate::voice::VoiceWindow;
    use crate::voice::disk_voice::{DiskVoiceConfig, tests::voice_over};
    use std::path::PathBuf;

    // How far into its source the reader got over the measured blocks, at a
    // given stretch factor: the position it last published to the streamer
    // (`Ring::play`, what the streamer fills ahead of) after the warm-up, and
    // after the measured blocks, so this measures where the reader actually
    // read rather than what anything claims it should have.
    let consumed = |factor: f64| -> f64 {
        let (mut pool, handle) = VoicePool::new().with_handle();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));

        // Enough ring that the unstretched (fastest) case cannot underrun,
        // so a short read means the rate applied rather than that the ring
        // ran dry — the control assertion below checks the other direction.
        let flat: Vec<f32> = (0..20_000)
            .flat_map(|i| {
                let s = ((std::f64::consts::TAU * 440.0 * i as f64 / 44_100.0).sin()) as f32;
                [s, s]
            })
            .collect();
        let (mut writer, reader, pos) =
            RegionBuffer::with_capacity(StreamId::of(1), PathBuf::new(), 20_000 + 64, 2u16);
        writer.push_interleaved(&flat).expect("the ring\'s width");
        let ring = std::sync::Arc::clone(&reader);
        // ONE `Cells`, shared by the source and the gate — `DiskVoice::new`
        // requires it, and the published stretch rate is read back through
        // the same cell.
        let state = Arc::new(Cells::new());
        let voice = voice_over(
            reader,
            pos,
            Arc::clone(&state),
            DiskVoiceConfig {
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                file_sample_rate: SampleRate::SR_44K1,
            },
        );

        let play = Playback {
            stretch: StretchFactor::new(factor),
            ..Default::default()
        };
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(1),
                voice: Box::new(Voice {
                    source: VoiceSource::Disk(voice),
                    play,
                }),
                stretch: None,
            })
            .expect("the command queue has room in a test");

        let mut play = 0;
        for block in 0..WARM + MEASURED {
            if block == WARM {
                play = ring.play();
            }
            run(&mut pool, &transport, 44_100.0, BLOCK);
            transport.advance(BLOCK as i64, SampleRate::SR_44K1);
        }
        (ring.play() - play) as f64
    };

    let unstretched = consumed(1.0);
    assert!(
        unstretched > 0.0,
        "the unstretched control consumed nothing — the ring never fed, so \
             this comparison would prove nothing"
    );
    for factor in [1.0, 2.0] {
        let got = consumed(factor);
        let want = (MEASURED * BLOCK) as f64 / factor;
        assert!(
            (got - want).abs() <= 3.0 * HOP / factor,
            "{factor}x stretch consumed {got} source frames over {} output frames, \
             want {want}: the stretch rate never reached the disk tier's ring, \
             so the factor acts as varispeed",
            MEASURED * BLOCK
        );
    }
}

/// Stream ids for the patterns that match a command's stream.
#[cfg(test)]
const STREAM_1: crate::streamer::StreamId = crate::streamer::StreamId::of(1);
#[cfg(test)]
const STREAM_2: crate::streamer::StreamId = crate::streamer::StreamId::of(2);
#[cfg(test)]
const STREAM_3: crate::streamer::StreamId = crate::streamer::StreamId::of(3);

/// Dominant frequency by Hann-windowed DFT scan — a test helper, not
/// production code. Coarse (2 Hz) because the assertion tolerance is 2%.
#[cfg(test)]
fn dominant_hz(x: &[f32], sample_rate: f32) -> f32 {
    let n = x.len();
    let mut best = (0.0f32, 0.0f32);
    let mut f = 60.0f32;
    while f < 2500.0 {
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (i, &s) in x.iter().enumerate() {
            let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos();
            let p = std::f32::consts::TAU * f * i as f32 / sample_rate;
            re += s * w * p.cos();
            im -= s * w * p.sin();
        }
        let m = (re * re + im * im).sqrt();
        if m > best.1 {
            best = (f, m);
        }
        f += 2.0;
    }
    best.0
}

/// Continuous playback must NOT flush — the false positive that matters.
///
/// A jump threshold set too tight fires on ordinary blocks, so the vocoder
/// resets constantly and its output becomes a stutter of ~50 ms fragments.
/// Nothing else in the suite would see it: every other stretch assertion is
/// `!= 0.0` or `> 1e-6`, and a stuttering stretcher is still non-zero. So
/// this asserts *continuity of level* across hundreds of ordinary blocks —
/// after the pipeline fills, no block may collapse to silence.
///
/// Two independent causes produce the same symptom (a flush that leaves a gap,
/// and a vocoder that publishes `stretch x` as many output samples as it
/// consumed against a host that takes exactly one per call), which is the argument for asserting
/// level continuity here rather than merely "output is non-zero".
#[test]
fn continuous_playback_does_not_flush_the_stretch_filter() {
    const SR: f64 = 44_100.0;
    const LEN: usize = 441_000;

    // Steady 3 kHz throughout: any dropout is the filter being reset, not the
    // material.
    let data: Vec<f32> = (0..LEN)
        .map(|i| 0.5 * (((std::f64::consts::TAU * 3000.0 * i as f64 / SR).sin()) as f32))
        .collect();
    let wave = Arc::new(Wave::from_mono(SampleRate::of(SR), Mono::new(&data)));

    let transport = MockTransport::rolling(Beat::new(1.0), Bpm::of(120.0));
    let (mut unit, handle) = VoicePool::with_channels(1u16)
        .expect("a width the sampler reads")
        .with_handle();

    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    handle
        .send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
            }),
            stretch: None,
        })
        .expect("the command queue has room in a test");

    let mut out = [0.0f32; 1];
    // Fill the vocoder pipeline first: the opening blocks are legitimately
    // quiet while the FIFOs and overlap-add tail prime.
    for _ in 0..16_384 {
        tick(&mut unit, &transport, SR, &mut out);
        transport.advance(1, SampleRate::of(SR));
    }
    assert!(
        unit.voices[0].stretch.is_some(),
        "stretch path must be live"
    );

    // Steady state: measure per-block peaks and require every one to carry
    // signal. A reset mid-run empties the FIFO, so the following block is
    // silent — which is exactly what this catches.
    let mut quiet_blocks = 0usize;
    for _ in 0..256 {
        let mut peak = 0.0f32;
        for _ in 0..64 {
            tick(&mut unit, &transport, SR, &mut out);
            transport.advance(1, SampleRate::of(SR));
            peak = peak.max(out[0].abs());
        }
        if peak < 0.01 {
            quiet_blocks += 1;
        }
    }
    assert_eq!(
        quiet_blocks, 0,
        "continuous playback flushed the filter: {quiet_blocks}/256 blocks fell silent"
    );
}

#[test]
fn add_and_remove_clips() {
    let (mut unit, handle) = VoicePool::new().with_handle();
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let wave = make_wave(100);

    let sampler = MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    add_ram_clip(&handle, SlotId(1), sampler);

    let mut out = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out);
    assert!(out[0] != 0.0 || out[1] != 0.0, "voice should produce audio");

    handle
        .send(VoiceCommand::Remove(SlotId(1)))
        .expect("the command queue has room in a test");
    tick(&mut unit, &transport, SR, &mut out);
    assert_eq!(out[0], 0.0);
    assert_eq!(out[1], 0.0);
}

#[test]
fn silence_when_transport_stopped() {
    let (mut unit, handle) = VoicePool::new().with_handle();
    let transport = MockTransport::stopped(Beat::new(0.0), Bpm::of(120.0));
    let wave = make_wave(100);

    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    add_ram_clip(&handle, SlotId(1), sampler);

    let mut out = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out);
    assert_eq!(out[0], 0.0);
    assert_eq!(out[1], 0.0);
}

#[test]
fn clips_sum_together() {
    let (mut unit, handle) = VoicePool::new().with_handle();
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let wave = make_wave(100);

    for i in 0..3 {
        let sampler = MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave");
        add_ram_clip(&handle, SlotId(i), sampler);
    }

    let mut out_3 = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out_3);

    let (mut unit2, handle2) = VoicePool::new().with_handle();
    let sampler = MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    add_ram_clip(&handle2, SlotId(0), sampler);
    let mut out_1 = [0.0f32; 2];
    tick(&mut unit2, &transport, SR, &mut out_1);

    let tolerance = 0.001;
    assert!((out_3[0] - out_1[0] * 3.0).abs() < tolerance);
    assert!((out_3[1] - out_1[1] * 3.0).abs() < tolerance);
}

#[test]
fn clone_snapshots_clips() {
    let (mut unit, handle) = VoicePool::new().with_handle();
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let wave = make_wave(100);

    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    add_ram_clip(&handle, SlotId(1), sampler);

    let mut out = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out);

    let mut cloned = unit.clone();
    let mut out_clone = [0.0f32; 2];
    tick(&mut cloned, &transport, SR, &mut out_clone);

    assert!(out_clone[0] != 0.0, "cloned unit should have the voice");
}

/// A pool no handle was taken for (what a fork builds) is empty and
/// silent, and a voice inserted directly still plays.
#[test]
fn a_pool_with_no_handle_has_no_channel_and_is_empty() {
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let wave = make_wave(100);

    // A pool nothing has taken the handle of (what a fork builds) is born
    // empty and channel-less: there is no sender for its `rx`, so no
    // command can ever reach it.
    let mut unit = VoicePool::new();
    assert_eq!(unit.voice_count(), 0, "a new pool starts empty");

    let mut out = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out); // drains its (permanently empty) queue
    assert_eq!(
        unit.voice_count(),
        0,
        "a pool with no handle receives no commands"
    );
    assert_eq!(out[0], 0.0);
    assert_eq!(out[1], 0.0);

    // But voices inserted directly (the Populate path) are audible.
    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    unit.insert_voice(
        SlotId(1),
        Voice {
            play: Playback {
                gain: sampler.gain(),
                speed: sampler.speed(),
                loop_: sampler.loop_(),
                direction: PlayDirection::Forward,
                stretch: StretchFactor::new(1.0),
                pitch: Cents::new(0.0),
            },
            source: VoiceSource::Memory(sampler),
        },
    );
    tick(&mut unit, &transport, SR, &mut out);
    assert!(out[0] != 0.0 || out[1] != 0.0, "inserted voice is audible");
}

#[test]
fn update_gain() {
    let (mut unit, handle) = VoicePool::new().with_handle();
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let wave = make_wave(100);

    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    add_ram_clip(&handle, SlotId(1), sampler);

    let mut out_before = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out_before);

    handle
        .send(VoiceCommand::UpdateGain {
            id: SlotId(1),
            gain: Amplitude::new(0.5),
        })
        .expect("the command queue has room in a test");

    let mut out_after = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out_after);

    assert!((out_after[0] - out_before[0] * 0.5).abs() < 0.01);
}

/// How many of `planes` channels a vocoder sounds on: fed a constant on every
/// one at unity, it writes a nonzero tail to the channels it has and leaves the
/// rest silent.
fn sounding_width(vocoder: &mut tutti_dsp::PhaseVocoder<f32>, planes: usize) -> usize {
    use tutti_buffers::{Planes, PlanesMut};
    let frames = 4096;
    let needed = vocoder.input_needed(Samples::new(frames)).get();
    let input = vec![vec![1.0f32; needed]; planes];
    let mut output = vec![vec![0.0f32; frames]; planes];
    vocoder.process(
        Planes::from_vecs(&input).expect("equal planes"),
        PlanesMut::from_vecs(&mut output).expect("equal planes"),
    );
    output
        .iter()
        .filter(|plane| plane[frames - 1].abs() > 0.5)
        .count()
}

#[test]
fn update_stretch_enables_processor() {
    let (mut unit, handle) = VoicePool::new().with_handle();
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let wave = make_wave(4096);

    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0))).expect("a placeable wave");
    add_ram_clip(&handle, SlotId(1), sampler);

    let mut out = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out);
    assert!(unit.voices[0].stretch.is_none(), "no stretch by default");

    handle
        .send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(2.0),
            pitch_cents: Cents::new(0.0),
            stretch: None,
        })
        .expect("the command queue has room in a test");
    tick(&mut unit, &transport, SR, &mut out);
    assert_eq!(
        unit.voices[0].voice.play.stretch,
        StretchFactor::new(2.0),
        "the intent is recorded"
    );
    // The vocoder has to arrive on the command: this voice spawned at unity, so
    // `AddVoice` correctly gave it none, and the read is dry without one.
    // Checking the intent alone is what let a voice sit "stretching" and dry
    // forever.
    assert!(
        unit.voices[0].stretch.is_some(),
        "turning stretch on must deliver a vocoder, not just record the intent"
    );

    handle
        .send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(1.0),
            pitch_cents: Cents::new(0.0),
            stretch: None,
        })
        .expect("the command queue has room in a test");
    tick(&mut unit, &transport, SR, &mut out);
    assert!(
        unit.voices[0].stretch.is_some(),
        "a vocoder stays resident at unity, so the voice's latency does not change"
    );
}

/// Turning stretch on for a voice that spawned at unity/zero must be audible.
///
/// `AddVoice` correctly attaches no filter to a
/// voice at unity, so if `set_stretch` only flipped atomics on a *resident*
/// filter, `UpdateStretch` would mirror the values into `voice.play`,
/// `stretch.is_some()` would stay false, and
/// `tick_frame_into` would take the dry branch — silently, forever. The
/// filter has to arrive on the command.
///
/// Asserted on the *output*, not on `stretch.is_some()`: the point is that the
/// signal changes, and a future refactor that keeps the field but stops routing
/// through it should still fail here.
#[test]
fn enabling_stretch_mid_flight_changes_the_output() {
    let (mut unit, handle) = VoicePool::new().with_handle();
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let sampler = MemorySource::placed(make_wave(48_000), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    add_ram_clip(&handle, SlotId(1), sampler);

    // Settle the add, then collect a dry reference block.
    let mut out = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out);
    let mut dry = Vec::new();
    for _ in 0..512 {
        tick(&mut unit, &transport, SR, &mut out);
        dry.push(out[0]);
    }

    // Pitch-shift by an octave. `stretch_wanted` is false up to this point
    // (unity factor, zero cents), so no filter is resident.
    handle
        .send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(1.0),
            pitch_cents: Cents::new(1200.0),
            stretch: None,
        })
        .expect("the command queue has room in a test");

    let mut shifted = Vec::new();
    for _ in 0..512 {
        tick(&mut unit, &transport, SR, &mut out);
        shifted.push(out[0]);
    }

    assert!(
        unit.voices[0].stretch.is_some(),
        "a filter must have been adopted from the command"
    );
    // A phase vocoder has latency, so the first blocks can legitimately be
    // near-silent; compare energy over the whole span instead of frame-wise.
    let dry_energy: f32 = dry.iter().map(|s| s * s).sum();
    let shifted_energy: f32 = shifted.iter().map(|s| s * s).sum();
    assert!(
        dry_energy > 0.0,
        "the dry reference must carry signal, else the test proves nothing"
    );
    assert!(
        (shifted_energy - dry_energy).abs() > dry_energy * 0.01,
        "an octave pitch shift must change the signal: dry energy {dry_energy}, \
             shifted {shifted_energy} — equal means the filter was never routed through"
    );
}

/// A surplus filter is handed back for control-thread release, never dropped
/// in the callback.
///
/// `prepare` cannot see whether the target slot already holds a filter — that
/// is audio-thread state — so it builds one whenever the new values ask for
/// stretch. The second `UpdateStretch` here is therefore redundant, and
/// dropping it in the drain would free a vocoder bank (~192 KB at six
/// channels) inside the audio callback. It must arrive at `collect_retired`
/// instead.
#[test]
fn a_redundant_stretch_filter_is_retired_not_freed_on_the_audio_thread() {
    let (mut unit, handle) = VoicePool::new().with_handle();
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let sampler = MemorySource::placed(make_wave(4096), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    add_ram_clip(&handle, SlotId(1), sampler);

    let mut out = [0.0f32; 2];
    tick(&mut unit, &transport, SR, &mut out);

    // First update: adopted, nothing surplus.
    handle
        .send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(2.0),
            pitch_cents: Cents::new(0.0),
            stretch: None,
        })
        .expect("the command queue has room in a test");
    tick(&mut unit, &transport, SR, &mut out);
    assert_eq!(
        handle.collect_retired(),
        0,
        "the first filter is adopted, so nothing should be retired"
    );

    // Second update: a filter is already resident, so this one is surplus.
    handle
        .send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(3.0),
            pitch_cents: Cents::new(0.0),
            stretch: None,
        })
        .expect("the command queue has room in a test");
    tick(&mut unit, &transport, SR, &mut out);
    assert_eq!(
        handle.collect_retired(),
        1,
        "the redundant filter must be retired for control-thread release"
    );
    assert!(
        unit.voices[0].stretch.is_some(),
        "the resident filter must survive — a surplus arrival never swaps it"
    );
}

/// `VoiceNode` is a graph node in its own right: a spectral resynth adds a
/// standalone voice node to its graph, and a single timeline clip is one. It
/// has no inputs, two outputs, and its first frame is the wave's first sample.
#[test]
fn voice_node_is_a_standalone_graph_node() {
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let sampler = MemorySource::placed(make_wave(100), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");

    let mut node =
        VoiceNode::new(sampler, Playback::default()).expect("a finite gain and a nonempty width");
    assert_eq!(node.shape().audio_in().count(), 0);
    assert_eq!(node.shape().audio_out().count(), 2);

    let mut out = [0.0f32; 2];
    tick(&mut node, &transport, SR, &mut out);
    assert_eq!(
        out, [0.01; 2],
        "the wave's first sample, 1/100, on both channels"
    );
}

/// A voice's fork keeps its window and reads its render's transport
/// — the offline render's path: the fork is `Voice::fork_copy`, and there is no clock to swap, because a
/// voice reads the transport from each block's `Env`.
///
/// Asserted behaviourally, on what the fork plays under the render's
/// transport: silent before its window (beat 1), sounding inside it (beat
/// 3), silent on a stopped render.
#[test]
fn a_voices_fork_keeps_its_window_and_reads_the_renders_transport() {
    let wave = make_wave(44_100);
    let sampler =
        MemorySource::placed(wave, VoiceWindow::from(Beat::new(2.0))).expect("a placeable wave");
    let voice = Voice {
        source: VoiceSource::Memory(sampler),
        play: Playback::default(),
    };
    let fork = voice
        .fork_copy(tutti_graph::RenderMode::Realtime)
        .expect("a memory voice forks");
    let Voice {
        source: VoiceSource::Memory(forked),
        play,
    } = fork
    else {
        panic!("expected a Memory source");
    };
    assert_eq!(forked.window().map(|w| w.start), Some(Beat::new(2.0)));
    let mut node =
        VoiceNode::with_channels(forked, play, 1u16).expect("a finite gain and a nonempty width");
    node.prepare(&Prepare::new(
        SampleRate::of(SR),
        MaxBlock::of(Samples::new(64)),
    ));
    let at = |beat: f64| MockTransport::rolling(Beat::new(beat), Bpm::of(120.0));
    assert!(
        run(&mut node, &at(1.0), SR, 8)[0].iter().all(|&s| s == 0.0),
        "before the window"
    );
    assert!(
        run(&mut node, &at(3.0), SR, 8)[0].iter().all(|&s| s != 0.0),
        "inside the window"
    );
    let stopped = MockTransport::stopped(Beat::new(3.0), Bpm::of(120.0));
    assert!(
        run(&mut node, &stopped, SR, 8)[0].iter().all(|&s| s == 0.0),
        "a stopped render"
    );
}

// --- streaming voices through the handle ---

/// A streaming voice over a ring whose writer is gone: enough for a loop
/// to be set on the stream the handle holds for it.
fn streaming_voice() -> Voice<f32> {
    streaming_voice_of(1)
}

/// [`streaming_voice`] over stream `StreamId::of(region)`.
fn streaming_voice_of(region: u64) -> Voice<f32> {
    use crate::streamer::{Cells, RegionBuffer, StreamId};
    use crate::voice::disk_voice::{DiskVoiceConfig, tests::voice_over};

    let (writer, reader, pos) =
        RegionBuffer::with_capacity(StreamId::of(region), std::path::PathBuf::new(), 128, 2u16);
    drop(writer);
    let state = Arc::new(Cells::new());
    Voice {
        source: VoiceSource::Disk(voice_over(
            reader,
            pos,
            state,
            DiskVoiceConfig {
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                file_sample_rate: SampleRate::SR_44K1,
            },
        )),
        play: Playback::default(),
    }
}

fn loop_on(id: SlotId) -> VoiceCommand<f32> {
    loop_to(id, 64)
}

/// The loop command `[0, end)` for slot `id`.
fn loop_to(id: SlotId, end: usize) -> VoiceCommand<f32> {
    VoiceCommand::UpdateLoop {
        id,
        setting: Some(loop_frames(0, end, 0)),
    }
}

/// One block of `pool`, at [`SR`] on a rolling clock.
fn one_block(pool: &mut VoicePool<f32>) {
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut out = [0.0f32; 2];
    tick(pool, &transport, SR, &mut out);
}

/// A streamer's command queue with no streamer thread behind it: the test
/// holds the receiving end, so it sees what was sent, drains it by hand,
/// or drops it (the streamer gone).
fn test_streamer(
    capacity: usize,
) -> (
    crate::streamer::StreamerPort<f32>,
    std::sync::mpsc::Receiver<crate::streamer::StreamerCommand<f32>>,
) {
    let (tx, rx) = sync_channel(capacity);
    (crate::streamer::StreamerPort::new(tx), rx)
}

/// The stream `StreamId::of(region)` on `port`: a handle whose commands the
/// test sees.
fn stream_of(region: u64, port: &crate::streamer::StreamerPort<f32>) -> crate::StreamHandle<f32> {
    crate::StreamHandle::detached(crate::streamer::StreamId::of(region), port.clone())
}

/// Adds `voice`, built over stream `region`, at `id` with a stream on `port`.
fn add_with_stream(
    handle: &VoicePoolHandle<f32>,
    id: SlotId,
    region: u64,
    voice: Voice<f32>,
    port: &crate::streamer::StreamerPort<f32>,
) {
    add_with_stream_result(handle, id, region, voice, port)
        .expect("the command queue has room in a test");
}

fn add_with_stream_result(
    handle: &VoicePoolHandle<f32>,
    id: SlotId,
    region: u64,
    voice: Voice<f32>,
    port: &crate::streamer::StreamerPort<f32>,
) -> Result<(), Box<StreamingAddError<f32>>> {
    let stream = stream_of(region, port);
    let VoiceSource::Disk(disk) = voice.source else {
        unreachable!("a streaming voice")
    };
    handle.add_streaming(id, disk, stream, voice.play)
}

fn looped(mut voice: Voice<f32>) -> Voice<f32> {
    voice.play.loop_ = Some(loop_frames(0, 64, 0));
    voice
}

/// Reads the stream the handle holds for `id`: its loop and direction cells.
fn held<R>(
    handle: &VoicePoolHandle<f32>,
    id: SlotId,
    read: impl FnOnce(&crate::StreamHandle<f32>) -> R,
) -> R {
    let inner = handle.lock();
    let Some(LiveVoice::Disk { stream }) = inner.slots.get(&id).map(|l| &l.voice) else {
        panic!("the handle holds the voice's stream")
    };
    read(stream)
}

/// A loop the stream takes is recorded, and it was published to the stream's
/// loop cell by the handle, before the drain ran: the add's own loop and the
/// update's, two publishes, the second the one in force.
#[test]
fn a_loop_sent_through_the_handle_is_published_to_the_stream_and_recorded() {
    let (commands, _streamer) = test_streamer(4);
    let (mut pool, handle) = VoicePool::new().with_handle();
    let id = SlotId(3);
    add_with_stream(&handle, id, 1, looped(streaming_voice()), &commands);
    handle
        .send(loop_to(id, 32))
        .expect("the command queue has room in a test");
    held(&handle, id, |stream| {
        assert_eq!(
            stream.looping_cell().seq(),
            2,
            "the add's loop, then the update's"
        );
        assert_eq!(stream.loop_(), Some(loop_frames(0, 32, 0)));
    });
    one_block(&mut pool);
    assert!(matches!(
        pool.playback_of(id).expect("slot exists").loop_,
        Some(l) if l.end() == Samples::new(32)
    ));
    assert_eq!(handle.drops(), PoolDrops::default());
}

/// The drain publishes nothing to the stream: what a streaming voice's loop
/// needs from the streamer is published by the handle as it queues the
/// command, and the drain only records it.
#[test]
fn the_drain_publishes_nothing_to_the_stream() {
    let (commands, _streamer) = test_streamer(8);
    let (mut pool, handle) = VoicePool::new().with_handle();
    let id = SlotId(3);
    add_with_stream(&handle, id, 1, looped(streaming_voice()), &commands);
    handle
        .send(loop_on(id))
        .expect("the command queue has room in a test");
    let seq = held(&handle, id, |s| s.looping_cell().seq());
    one_block(&mut pool);
    assert!(pool.playback_of(id).expect("slot exists").loop_.is_some());
    assert_eq!(held(&handle, id, |s| s.looping_cell().seq()), seq);
    assert_eq!(handle.drops(), PoolDrops::default());
}

/// A direction set through the handle reaches the voice's stream, which owns
/// it: the stream's direction cell holds it, and the slot records it once the
/// drain runs. A direction on the add is published the same way.
#[test]
fn a_direction_set_through_the_handle_reaches_the_stream() {
    let (commands, _streamer) = test_streamer(8);
    let (mut pool, handle) = VoicePool::new().with_handle();
    let id = SlotId(2);
    let mut voice = streaming_voice();
    voice.play.direction = PlayDirection::Reverse;
    add_with_stream(&handle, id, 1, voice, &commands);
    assert_eq!(
        held(&handle, id, |s| s.direction()),
        PlayDirection::Reverse,
        "the add's direction"
    );
    handle
        .send(VoiceCommand::UpdateReverse {
            id,
            direction: PlayDirection::Forward,
        })
        .expect("room");
    assert_eq!(
        held(&handle, id, |s| s.direction()),
        PlayDirection::Forward,
        "the update's"
    );
    one_block(&mut pool);
    assert_eq!(
        pool.playback_of(id).expect("slot exists").direction,
        PlayDirection::Forward
    );
}

/// A stream ends with its voice: the handle holds it from the add, and a
/// `Remove` drops it, which sends the streamer its close. An add at a live id
/// is refused and hands its stream back, and the resident stream stays open.
#[test]
fn a_stream_ends_when_its_voice_is_removed() {
    use crate::streamer::StreamerCommand as C;
    let (commands, streamer) = test_streamer(8);
    let (_pool, handle) = VoicePool::new().with_handle();
    let id = SlotId(1);
    add_with_stream(&handle, id, 1, streaming_voice_of(1), &commands);
    assert_eq!(
        streamer.try_iter().count(),
        0,
        "an add with nothing to set sends nothing"
    );
    handle.remove(id).expect("room");
    assert!(matches!(
        streamer.try_iter().collect::<Vec<_>>().as_slice(),
        [C::Close { stream: STREAM_1 }]
    ));

    add_with_stream(&handle, id, 2, streaming_voice_of(2), &commands);
    let refused = add_with_stream_result(&handle, id, 3, streaming_voice_of(3), &commands)
        .expect_err("the id is live");
    assert_eq!(refused.error, SendError::SlotInUse(id));
    assert_eq!(
        streamer.try_iter().count(),
        0,
        "the refused add ended nothing: the resident voice's stream stays open"
    );
    drop(refused);
    assert!(
        matches!(
            streamer.try_iter().collect::<Vec<_>>().as_slice(),
            [C::Close { stream: STREAM_3 }]
        ),
        "only the stream handed back ends, when its caller drops it"
    );
    handle.remove(id).expect("room");
    assert!(
        matches!(
            streamer.try_iter().collect::<Vec<_>>().as_slice(),
            [C::Close { stream: STREAM_2 }]
        ),
        "the resident voice's stream ended, the refused one's did not open"
    );
}

/// A voice out of its window adds nothing to the mix, at a finite gain and at
/// an infinite or NaN one alike (a non-finite gain is silence), so the pool's
/// skip of a voice with no position agrees with reading it, and the
/// sounding voice beside it is intact.
///
/// Exact, analytically: the sounding voice reads whole frames (44.1 kHz on
/// both sides, from beat 0), where the cubic returns the frame, gained by
/// 0.5.
#[test]
fn a_voice_out_of_its_window_adds_nothing_at_any_gain() {
    const BLOCK: usize = 64;
    let value = |i: usize| (i + 1) as f32 / 1024.0;
    let mut wave = Wave::zero(2, SampleRate::of(SR), Seconds::new(0.1));
    for i in 0..wave.len().get() {
        wave.set(0, i, value(i));
        wave.set(1, i, -value(i));
    }
    let wave = Arc::new(wave);
    let voice = |start: f64, len: Option<f64>, gain: f64| {
        let play = Playback {
            gain: Amplitude::new(gain),
            ..Playback::default()
        };
        Voice {
            source: VoiceSource::Memory(
                MemorySource::placed(
                    Arc::clone(&wave),
                    match len {
                        Some(d) => VoiceWindow::span(Beat::new(start), BeatDuration::new(d)),
                        None => VoiceWindow::from(Beat::new(start)),
                    },
                )
                .expect("a placeable wave"),
            ),
            play,
        }
    };
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    for silent_gain in [3.0f64, f64::INFINITY, f64::NAN] {
        let mut pool = VoicePool::with_channels(2u16).expect("stereo");
        pool.insert_voice(SlotId(0), voice(0.0, None, 0.5));
        pool.insert_voice(SlotId(1), voice(1_000.0, None, silent_gain));
        // In its window for 20 frames, then out of it: audible only at a
        // finite gain.
        if !silent_gain.is_finite() {
            pool.insert_voice(SlotId(2), voice(0.0, Some(20.0 / 22_050.0), silent_gain));
        }
        let mut pool = crate::testing::prepared(pool, SampleRate::of(SR), BLOCK);
        let out = run(&mut pool, &t, SR, BLOCK);
        for (c, sign) in [(0, 1.0f32), (1, -1.0)] {
            for (i, &got) in out[c].iter().enumerate() {
                let want = sign * value(i) * 0.5;
                assert_eq!(
                    got.to_bits(),
                    want.to_bits(),
                    "channel {c}, frame {i}, gain {silent_gain}"
                );
            }
        }
    }
}

/// Fills `handle`'s command queue to `COMMAND_CAPACITY - room` with
/// removes of ids nothing uses.
fn fill_queue(handle: &VoicePoolHandle<f32>, queued: usize, room: usize) {
    for i in queued..COMMAND_CAPACITY - room {
        handle
            .send(VoiceCommand::Remove(SlotId(1000 + i as u128)))
            .expect("below capacity");
    }
}

/// A streaming loop or add that meets a full command queue is refused
/// before the stream hears of it: `Err` means the command is gone, and the
/// stream stays on the loop it had (the add's stream is handed back).
#[test]
fn a_full_queue_refuses_a_streaming_loop_or_add_before_the_stream() {
    let (commands, _streamer) = test_streamer(8);
    let (mut pool, handle) = VoicePool::new().with_handle();
    let id = SlotId(2);
    add_with_stream(&handle, id, 1, looped(streaming_voice()), &commands);
    fill_queue(&handle, 1, 0);
    let err = handle.send(loop_on(id)).expect_err("the queue is full");
    assert!(matches!(err, SendError::Full));
    assert_eq!(
        held(&handle, id, |s| s.looping_cell().seq()),
        1,
        "only the add's own loop was published"
    );
    let voice = looped(streaming_voice_of(5));
    let stream = stream_of(5, &commands);
    let VoiceSource::Disk(disk) = voice.source else {
        unreachable!("a streaming voice")
    };
    let refused = handle
        .add_streaming(SlotId(5), disk, stream, voice.play)
        .expect_err("the queue is full");
    assert!(matches!(refused.error, SendError::Full));
    assert_eq!(
        refused.stream.loop_(),
        None,
        "the stream is handed back as it came"
    );
    one_block(&mut pool);
    assert!(pool.playback_of(id).expect("slot exists").loop_.is_some());
}

/// Channel `c` carries the constant `c + 1`.
fn indexed_wave(channels: usize, len: usize) -> Arc<Wave> {
    let mut w = Wave::zero(
        channels,
        SampleRate::SR_44K1,
        Seconds::new(len as f64) / 44_100.0,
    );
    for i in 0..w.len().get() {
        for c in 0..channels {
            w.set(c, i, (c + 1) as f32);
        }
    }
    Arc::new(w)
}

#[test]
fn reader_and_voice_node_default_to_stereo() {
    let (unit, _h) = VoicePool::<f32>::new().with_handle();
    assert_eq!(unit.channels(), ChannelLayout::STEREO);
    assert_eq!(unit.shape().audio_out(), ChannelLayout::STEREO);
}

/// A pool wider than the sampler reads is refused, by name, where one
/// at the ceiling is built: past `MAX_CHANNELS` it would declare
/// outputs its read never writes.
#[test]
fn a_pool_wider_than_the_sampler_reads_is_refused() {
    let ceiling = tutti_buffers::MAX_CHANNELS;
    assert!(
        VoicePool::<f32>::with_channels(
            ChannelLayout::try_from(ceiling).expect("a test width is small")
        )
        .is_ok()
    );
    let err = VoicePool::<f32>::with_channels(
        ChannelLayout::try_from(ceiling + 1).expect("a test width is small"),
    )
    .expect_err("a pool past the ceiling must be refused");
    assert!(matches!(
        err,
        PlaceError::WidthTooWide { channels, .. } if channels as usize == ceiling + 1
    ));
    assert_eq!(
        VoicePool::<f32>::with_channels(0u16).err(),
        Some(PlaceError::EmptyWidth),
        "no channels is refused, not coerced to mono"
    );
}

/// A voice node's constructor refuses a width the sampler cannot run, and a
/// gain that is not finite, by name: past `MAX_CHANNELS` it would declare
/// outputs its read never writes, and no channels is a node nothing can be
/// wired to.
#[test]
fn a_voice_node_refuses_a_width_or_gain_the_sampler_cannot_run() {
    let voice = |gain: f64| {
        (
            MemorySource::placed(make_wave(4096), VoiceWindow::from(Beat::new(0.0)))
                .expect("a placeable wave"),
            Playback {
                gain: Amplitude::new(gain),
                ..Playback::default()
            },
        )
    };
    let node = |gain: f64, channels: ChannelLayout| {
        let (source, play) = voice(gain);
        VoiceNode::with_channels(source, play, channels)
    };
    let max = tutti_buffers::MAX_CHANNELS as u16;
    assert_eq!(
        node(1.0, ChannelLayout::from(max)).err(),
        None,
        "the ceiling itself is carried"
    );
    assert_eq!(
        node(1.0, ChannelLayout::from(max + 2)).err(),
        Some(VoiceNodeError::Width(PlaceError::WidthTooWide {
            channels: max + 2,
            max
        }))
    );
    assert_eq!(
        node(1.0, ChannelLayout::from(0u16)).err(),
        Some(VoiceNodeError::Width(PlaceError::EmptyWidth))
    );
    for gain in [f64::NAN, f64::INFINITY] {
        assert_eq!(
            node(gain, ChannelLayout::STEREO).err(),
            Some(VoiceNodeError::Gain(GainError::NotFinite))
        );
    }
}

/// Both nodes declare the width they were built at, as generators: no
/// inputs, no latency, never skipped.
#[test]
fn both_nodes_declare_their_width() {
    for w in [1usize, 2, 6, 8] {
        let unit = VoicePool::<f32>::with_channels(
            ChannelLayout::try_from(w).expect("a test width is small"),
        )
        .expect("a width the sampler reads");
        let sampler = MemorySource::with_config(
            indexed_wave(6, 64),
            MemorySourceConfig {
                channels: ChannelLayout::try_from(w).expect("a test width is small"),
                placement: Some(VoiceWindow::default()),
                ..Default::default()
            },
        )
        .expect("a placeable wave");
        let vn = VoiceNode::with_channels(
            sampler,
            Playback::default(),
            ChannelLayout::try_from(w).expect("a test width is small"),
        )
        .expect("a finite gain and a nonempty width");
        // A pool takes voices while it plays, so it promises no ceiling; a
        // node over a placed voice is the programme, which has no ring-out.
        for (what, shape, tail) in [
            ("pool", unit.shape(), tutti_units::Tail::Unbounded),
            ("voice node", vn.shape(), tutti_units::Tail::None),
        ] {
            assert_eq!(shape.audio_out().count() as usize, w, "{what} at width {w}");
            assert_eq!(shape.audio_in().count(), 0, "{what}: no inputs");
            assert_eq!(
                shape.latency.samples(),
                Samples::new(0),
                "{what}: no latency"
            );
            assert_eq!(shape.tail, tail, "{what}: its ring-out");
        }
    }
}

/// A 6-channel voice in a 6-wide reader must reach all six outputs, in a
/// one-frame block and a longer one.
#[test]
fn six_channel_clip_reaches_all_six_reader_outputs() {
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let (mut unit, _h) = VoicePool::with_channels(6u16)
        .expect("a width the sampler reads")
        .with_handle();
    let sampler = MemorySource::with_config(
        indexed_wave(6, 512),
        MemorySourceConfig {
            channels: ChannelLayout::from(6u16),
            placement: Some(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            }),
            ..Default::default()
        },
    )
    .expect("a placeable wave");
    unit.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(sampler),
            play: Playback::default(),
        },
    );

    let mut out = [0.0f32; 6];
    tick(&mut unit, &transport, SR, &mut out);
    for (c, &got) in out.iter().enumerate() {
        assert!(
            (got - (c + 1) as f32).abs() < 1e-3,
            "tick: channel {c} should carry {}, got {got} ({out:?})",
            c + 1
        );
    }
    assert!(
        out[2..].iter().all(|&s| s.abs() > 0.5),
        "channels 2..6 were dropped: {out:?}"
    );
}

/// The stretch branch is separate code from the direct read, and it is the
/// one R6 warns about: a slot whose stretcher is narrower than the reader
/// truncates silently, and ONLY when stretch is enabled. Nothing else in the
/// suite exercises that combination at width 6.
#[test]
fn six_channel_clip_with_stretch_reaches_all_six_outputs() {
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let (mut unit, _h) = VoicePool::with_channels(6u16)
        .expect("a width the sampler reads")
        .with_handle();
    let sampler = MemorySource::with_config(
        indexed_wave(6, 4096),
        MemorySourceConfig {
            channels: ChannelLayout::from(6u16),
            placement: Some(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            }),
            ..Default::default()
        },
    )
    .expect("a placeable wave");
    unit.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(sampler),
            play: Playback {
                // Off unity, so the vocoder is built.
                stretch: StretchFactor::new(2.0),
                ..Default::default()
            },
        },
    );

    // The phase vocoder has FFT latency, so early frames are legitimately
    // silent; drive until every channel has produced something. The
    // playhead moves a frame per one-frame block, as a host moves it: a
    // frozen one re-seats every block at one position.
    let mut seen = [false; 6];
    let mut out = [0.0f32; 6];
    for n in 0..16_384 {
        tick(&mut unit, &transport, SR, &mut out);
        transport.advance(1, SampleRate::of(SR));
        for (c, &s) in out.iter().enumerate() {
            if s.abs() > 1e-6 {
                seen[c] = true;
            }
        }
        if seen.iter().all(|&b| b) {
            break;
        }
        let _ = n;
    }
    assert!(
        seen.iter().all(|&b| b),
        "channels {:?} never produced output through the stretch path",
        seen.iter()
            .enumerate()
            .filter(|&(_, &b)| !b)
            .map(|(c, _)| c)
            .collect::<Vec<_>>()
    );
}

/// The SENDER builds the stretch vocoder, not the drain.
///
/// `VoicePoolHandle::send` runs on the control thread and fills
/// `AddVoice::stretch` when the voice asks for stretching; `drain_commands`
/// (which runs inside `tick`/`process`) only moves it in. If construction
/// ever moves back into the drain, an `AddVoice` sent with `stretch: None`
/// would still end up with a filter — so this asserts the filter is present
/// only because the send path put it there.
#[test]
fn send_builds_the_stretch_filter_not_the_drain() {
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let (mut unit, handle) = VoicePool::with_channels(6u16)
        .expect("a width the sampler reads")
        .with_handle();

    let mk = |stretch: StretchFactor| {
        let sampler = MemorySource::with_config(
            indexed_wave(6, 128),
            MemorySourceConfig {
                channels: ChannelLayout::from(6u16),
                placement: Some(VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                }),
                ..Default::default()
            },
        )
        .expect("a placeable wave");
        Voice {
            source: VoiceSource::Memory(sampler),
            play: Playback {
                stretch,
                ..Default::default()
            },
        }
    };

    // A stretching voice: `send` must attach a filter, at the READER's width.
    // Observe the queued command BEFORE the drain sees it — that is what
    // distinguishes "the sender built it" from "the drain built it", and it
    // is the only observation that can: after draining, a filter is present
    // either way.
    let peeked = {
        let (probe_tx, mut probe_rx) = tutti_rt::ring(4, tutti_rt::Overflow::Refuse);
        let probe = VoicePoolHandle {
            inner: Arc::new(Mutex::new(Inner {
                slots: Default::default(),
                commands: probe_tx,
            })),
            retired: Arc::new(Mutex::new(tutti_rt::ring(0, tutti_rt::Overflow::Refuse).1)),
            channels: ChannelLayout::from(6u16),
            counters: Default::default(),
            room: Default::default(),
            stamps: Default::default(),
            loops: Default::default(),
        };
        probe
            .send(VoiceCommand::AddVoice {
                id: SlotId(9),
                voice: Box::new(mk(StretchFactor::new(2.0))),
                stretch: None,
            })
            .expect("the command queue has room in a test");
        match probe_rx.pop().map(|q| q.cmd) {
            Some(VoiceCommand::AddVoice { stretch, .. }) => stretch,
            other => panic!("expected a queued AddVoice, got {other:?}"),
        }
    };
    let peeked = peeked.expect(
        "send must attach the filter BEFORE queueing — if this is None the \
             construction has moved back into the audio-thread drain",
    );
    let mut peeked = peeked;
    assert_eq!(
        sounding_width(&mut peeked.live, 8),
        6,
        "the sender must build at the reader's width"
    );

    handle
        .send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(mk(StretchFactor::new(2.0))),
            stretch: None,
        })
        .expect("the command queue has room in a test");
    // A non-stretching voice: no filter, because none is needed.
    handle
        .send(VoiceCommand::AddVoice {
            id: SlotId(2),
            voice: Box::new(mk(StretchFactor::new(1.0))),
            stretch: None,
        })
        .expect("the command queue has room in a test");

    let mut out = [0.0f32; 6];
    tick(&mut unit, &transport, SR, &mut out); // drains

    let stretching = unit.voices.iter_mut().find(|s| s.id == SlotId(1)).unwrap();
    let filter = stretching
        .stretch
        .as_mut()
        .expect("send must have built a vocoder for the stretching voice");
    assert_eq!(
        sounding_width(&mut filter.live, 8),
        6,
        "the vocoder must match the reader's width, not a default"
    );
    let plain = unit.voices.iter().find(|s| s.id == SlotId(2)).unwrap();
    assert!(
        plain.stretch.is_none(),
        "a non-stretching voice must not carry a vocoder — that is the whole \
             point of building lazily"
    );
}

/// A slot whose intent says stretch but whose filter has not arrived reads
/// DRY, not silent.
///
/// `active_stretch` requires both the intent and the unit. Degrading to a
/// dry read means a late filter costs one block of un-stretched audio rather
/// than a gap — and, critically, never an allocation in the callback.
#[test]
fn a_missing_stretch_filter_reads_dry_not_silent() {
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let (mut unit, _h) = VoicePool::with_channels(6u16)
        .expect("a width the sampler reads")
        .with_handle();

    let sampler = MemorySource::with_config(
        indexed_wave(6, 512),
        MemorySourceConfig {
            channels: ChannelLayout::from(6u16),
            placement: Some(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            }),
            ..Default::default()
        },
    )
    .expect("a placeable wave");
    // Insert DIRECTLY with no filter, simulating one that has not arrived.
    unit.insert_voice_inner(
        SlotId(1),
        Box::new(Voice {
            source: VoiceSource::Memory(sampler),
            play: Playback {
                stretch: StretchFactor::new(2.0),
                ..Default::default()
            },
        }),
        None,
    );

    let mut out = [0.0f32; 6];
    tick(&mut unit, &transport, SR, &mut out);
    for (c, &s) in out.iter().enumerate() {
        assert!(
            (s - (c + 1) as f32).abs() < 1e-3,
            "channel {c} should read dry ({}), got {s} — a missing filter \
                 must not silence the slot",
            c + 1
        );
    }
}
