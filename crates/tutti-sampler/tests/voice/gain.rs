//! Voice gain, end to end.
//!
//! The gain of a pool's row is its source config's, applied exactly once: the
//! failure mode is applying it twice (the source's own and the voice's), which is
//! inaudible at unity, subtle at 0.9, and 12 dB down at 0.25.
//!
//! # DC, not a tone
//!
//! Gain is a pure scalar, so the cleanest probe is a constant: the expected
//! output is the input times the gain, exactly, with no windowing or phase to
//! reason about. A wrong exponent or a doubled application is then a plain
//! numeric mismatch.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_buffers::Interleaved;
use tutti_buffers::Planes;

use tutti_buffers::ChannelLayout;
use tutti_buffers::Wave;
use tutti_graph::{Env, Node, TransportChanges, contract};
use tutti_sampler::testing::placed_config;
use tutti_sampler::{
    MemorySource, MemorySourceConfig, SlotId, VoiceNodeConfig, VoicePool, VoicePoolConfig,
    VoiceWindow,
};
use tutti_units::{Amplitude, Beat, Bpm, Frame, SamplePosition, SampleRate, Samples};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
/// The constant every source below carries, on both channels.
const LEVEL: f32 = 0.5;

/// A rolling transport advanced by hand, once per block, handed to each
/// block in its `Env`.
///
/// Gain reaches the memory tier through `PlaybackSlot`, which only reads a
/// placed voice — on a stopped transport the gate gives no position and
/// the slot emits silence. Same reason `reverse_and_loop.rs` needs one.
struct Clock {
    beat: AtomicU64,
    tempo: f64,
}

impl Clock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            beat: AtomicU64::new(0f64.to_bits()),
            tempo: 120.0,
        })
    }

    fn advance(&self, samples: usize) {
        let beats = samples as f64 * self.tempo / 60.0 / SR;
        let now = f64::from_bits(self.beat.load(Ordering::Relaxed));
        self.beat.store((now + beats).to_bits(), Ordering::Relaxed);
    }
}

impl Clock {
    fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }

    /// One block of `node` under this clock (not moved); planar.
    fn block(&self, node: &mut dyn Node) -> Vec<Vec<f32>> {
        let env = Env {
            frame: Frame::new(0),
            sample_rate: SampleRate::of(SR),
            block_len: Samples::new(BLOCK),
            transport: tutti_graph::TransportPosition::new(
                true,
                Bpm::of(self.tempo),
                self.beat(),
                None,
            ),
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        };
        contract::drive_input(node, &env, Planes::default(), &[], &[]).audio
    }
}

/// A constant-valued source, long enough not to run dry.
fn dc_wave(len: usize) -> Arc<Wave> {
    let mut w = Wave::new(2, SampleRate::of(SR));
    for _ in 0..len {
        w.append(Interleaved::new(&[LEVEL, LEVEL], w.layout()).unwrap())
            .unwrap();
    }
    Arc::new(w)
}

/// A pool of placed voices, one per `gains` entry (each at its gain), plus the clock driving it.
fn pool_of(gains: &[f64]) -> (VoicePool<f32>, Arc<Clock>) {
    let config = gains.iter().zip(1u128..).fold(
        VoicePoolConfig::<f32>::new(ChannelLayout::STEREO).expect("stereo is a pool width"),
        |config, (&gain, id)| {
            let voice = VoiceNodeConfig::placed(
                placed_config(dc_wave(SR as usize), VoiceWindow::default())
                    .gain(Amplitude::new(gain)),
            )
            .expect("a finite gain");
            config.row(SlotId(id), voice).expect("a free key")
        },
    );
    (
        contract::prepared(VoicePool::from(config), SampleRate::of(SR), BLOCK),
        Clock::new(),
    )
}

/// A placed voice in a pool at `play_gain`, plus the clock driving it.
fn pool_at_gain(play_gain: f64) -> (VoicePool<f32>, Arc<Clock>) {
    pool_of(&[play_gain])
}

/// Render a placed pool, advancing its clock once per block. Returns channel 0.
fn render_placed(pool: &mut VoicePool<f32>, clock: &Clock, blocks: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(blocks * BLOCK);
    for _ in 0..blocks {
        out.extend(clock.block(pool).swap_remove(0));
        clock.advance(BLOCK);
    }
    out
}

/// The mean of a settled span, skipping the first block.
fn settled_mean(out: &[f32]) -> f64 {
    let body = &out[BLOCK..];
    body.iter().map(|&s| s as f64).sum::<f64>() / body.len() as f64
}

/// Gain scales the output by exactly its own value — no more, no less.
///
/// The headline claim, checked across the range where a squared gain is
/// distinguishable from a linear one. At 1.0 the two coincide, which is why
/// unity alone would prove nothing; 0.5 squared is 0.25, a 6 dB error.
#[test]
fn voice_gain_scales_the_output_by_exactly_its_value() {
    for g in [1.0f64, 0.5, 0.25, 0.75, 0.1] {
        let (mut pool, clock) = pool_at_gain(g);
        let out = render_placed(&mut pool, &clock, 8);
        let got = settled_mean(&out);
        let want = f64::from(LEVEL) * g;

        assert!(
            (got - want).abs() < 1e-4,
            "gain {g}: output is {got:.6}, expected {want:.6} \
             (a squared gain would give {:.6})",
            f64::from(LEVEL) * g * g
        );
    }
}

/// Zero gain is silence, not near-silence.
///
/// The boundary a clamp or a `max(epsilon)` would quietly break. A voice muted
/// by gain must contribute nothing at all, or a "silent" track still sums into
/// the mix.
#[test]
fn zero_gain_is_exactly_silent() {
    let (mut pool, clock) = pool_at_gain(0.0);
    let out = render_placed(&mut pool, &clock, 8);

    for (i, &s) in out.iter().enumerate() {
        assert!(
            s == 0.0,
            "sample {i} is {s} at zero gain — a muted voice must be exactly silent"
        );
    }
}

/// Gain above unity amplifies rather than clamping to 1.0.
///
/// `Amplitude` is not bounded at unity, and the sampler is not the place to
/// limit — that is the mixer's job. A reader that clamped here would silently
/// cap every boosted clip.
#[test]
fn gain_above_unity_amplifies() {
    for g in [1.5f64, 2.0, 4.0] {
        let (mut pool, clock) = pool_at_gain(g);
        let out = render_placed(&mut pool, &clock, 8);
        let got = settled_mean(&out);
        let want = f64::from(LEVEL) * g;

        assert!(
            (got - want).abs() < 1e-4,
            "gain {g}: output is {got:.6}, expected {want:.6} — \
             a clamp at unity would give {:.6}",
            LEVEL as f64
        );
    }
}

/// Gain applies equally to every channel.
///
/// One scalar across the frame, per the source's own doc: per-channel level is
/// the mixer strip's job. A gain that reached only channel 0 would shift the
/// stereo image toward one side while the level meter still looked plausible.
#[test]
fn gain_scales_every_channel_equally() {
    let (mut pool, clock) = pool_at_gain(0.4);

    let want = (LEVEL * 0.4) as f64;

    // Skip one block, then check both channels of every frame.
    clock.block(&mut pool);
    clock.advance(BLOCK);
    for _ in 0..4 {
        let b = clock.block(&mut pool);
        for (i, (&l, &r)) in b[0].iter().zip(&b[1]).enumerate() {
            let (l, r) = (l as f64, r as f64);
            assert!(
                (l - want).abs() < 1e-4 && (r - want).abs() < 1e-4,
                "frame {i}: L {l:.6} R {r:.6}, both expected {want:.6}"
            );
        }
        clock.advance(BLOCK);
    }
}

/// A bare `MemorySource` applies its own gain.
///
/// The other half of the two-field story. Ticked directly — not through a pool —
/// the source is the only thing that can apply a gain, so `sample_into` must
/// do it. This is what makes the pooled case above a real distinction rather
/// than a claim about a field nobody reads.
#[test]
fn a_bare_memory_source_applies_its_own_gain() {
    for g in [1.0f64, 0.5, 0.25] {
        let mut source = MemorySource::from(
            MemorySourceConfig::new(dc_wave(SR as usize), ChannelLayout::STEREO)
                .expect("a placeable wave")
                .gain(Amplitude::new(g)),
        );
        source.trigger_at(SamplePosition::new(0.0));
        source.play();

        let b = Clock::new().block(&mut source);
        let got = b[0][10] as f64;
        let want = f64::from(LEVEL) * g;
        assert!(
            (got - want).abs() < 1e-4,
            "bare source at gain {g}: got {got:.6}, expected {want:.6}"
        );
    }
}

/// Two voices at half gain sum to one voice at full gain.
///
/// Gain has to be linear in amplitude for a mix to behave, and a pool is a
/// summing mixer. This is the property a dB/amplitude confusion breaks:
/// `Db * 2.0` squares the amplitude (the engine's unit docs call this out
/// explicitly), so a gain path that went through dB by mistake would fail here
/// while still passing every single-voice test above.
#[test]
fn two_half_gain_voices_sum_to_one_full_gain_voice() {
    let (mut pool, clock) = pool_of(&[0.5, 0.5]);

    let out = render_placed(&mut pool, &clock, 8);
    let got = settled_mean(&out);
    // 2 x (0.5 level x 0.5 gain) == 0.5, i.e. the source level at unity.
    let want = LEVEL as f64;

    assert!(
        (got - want).abs() < 1e-4,
        "two voices at 0.5 gain summed to {got:.6}, expected {want:.6} — \
         gain is not linear in amplitude"
    );
}
