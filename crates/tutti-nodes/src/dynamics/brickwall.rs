//! The hard-clipping [`BrickwallLimiterNode`].
//!
//! It is not [`LimiterNode`](super::LimiterNode). The lookahead limiter delays its audio
//! so the gain decision runs ahead of a peak, which costs latency and sounds transparent;
//! the brickwall clamps in place, which costs nothing and distorts. Reach for the
//! first to limit musically, the second as a safety catch.

use tutti_buffers::Float;
use tutti_buffers::{ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, BrickwallBank, BrickwallControls, BrickwallSignals, COEFF_INTERVAL,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Db, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// Hard clipper at a ceiling: `channels` wide, no lookahead, zero latency, no tail.
///
/// A [`KernelNode`] over [`BrickwallKernel`], made from a [`BrickwallLimiterConfig`]
/// (`BrickwallLimiterNode::from`), the clip being [`BrickwallBank`]'s; this file
/// holds the cell and how the graph's control signal reaches it.
///
/// # The ceiling
///
/// The ceiling (dB) is the node's one control, modulatable by the graph. It is
/// **read per frame**: a control signal on the ceiling's port is the bank's own ([`BrickwallSignals`]),
/// a frame clamps at the amplitude of its own value, and the cell is what a frame nobody
/// drives clamps at. A cell that moves reaches the clamp within 16 frames ([`Kernel::GRID`]). Unmodulated, the node
/// reads its cell once per block, and a render with a control signal that holds the cell's value
/// is the render with no control signal.
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, its controls are a
/// [`ParamSet`](tutti_graph::ParamSet) over the ceiling.
///
/// # A NaN ceiling is ignored
///
/// A NaN ceiling has no level to clamp to, so it is ignored wherever it comes from: a NaN
/// in the cell keeps the last ceiling the node clamped at, a NaN frame on the ceiling
/// port clamps at the cell's ceiling, and a NaN given to [`BrickwallLimiterConfig::new`] starts the
/// node at 0 dB. `+-inf` dB are ceilings: `+inf` passes everything, `-inf` mutes.
///
/// `F` is the float of the graph it runs in, and the precision the ceiling is held and the clamp
/// runs in.
pub type BrickwallLimiterNode<F> = KernelNode<BrickwallKernel<F>>;

/// What a [`BrickwallLimiterNode`] is made from: its bus and the ceiling it clamps at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrickwallLimiterConfig {
    input: ChannelTopology,
    ceiling: Db,
}

impl BrickwallLimiterConfig {
    /// A brickwall limiter on the bus `input`, as wide as it, clamping at `ceiling`. The clip
    /// is stateless, so every channel is clamped to the same (linked) ceiling. An
    /// `n`-channel one is `new(ChannelTopology::unassigned(n)?, ..)`, which refuses a width past
    /// one bus.
    ///
    /// **This is a hard clipper, not [`LimiterNode`](super::LimiterNode).** It has no
    /// lookahead, no envelope and no latency: a sample past the ceiling is simply
    /// clamped, which distorts rather than ducking. Reach for it as a safety catch on
    /// an output, and for musical limiting use [`LimiterNode`](super::LimiterNode).
    #[must_use]
    pub fn new(input: ChannelTopology, ceiling: Db) -> Self {
        Self { input, ceiling }
    }
}

impl<F: Float> From<BrickwallLimiterConfig> for BrickwallLimiterNode<F> {
    fn from(config: BrickwallLimiterConfig) -> Self {
        Self::with_kernel(
            BrickwallKernel::new(config.input),
            BrickwallSet,
            BrickwallControls {
                ceiling: config.ceiling,
            },
        )
    }
}

/// The brickwall's one control: the ceiling.
#[derive(Clone, Copy, Debug)]
pub struct BrickwallSet;

/// The ceiling's linear step under which a write is not a new design: what the node has
/// always held (it re-derived its clamp only for a move past this).
const DEAD_BAND: f64 = 0.0001;

control_set! {
    BrickwallSet(_s): BrickwallControls = BrickwallControls::DEFAULT;
    live: (all);
    Ceiling => Db = ceiling { fast, not_nan };

    fn defaults(&self) -> BrickwallControls {
        BrickwallControls::DEFAULT
    }

    /// Designs again only for a ceiling that moved more than `DEAD_BAND` in amplitude.
    fn moved(&self, applied: &BrickwallControls, target: &BrickwallControls) -> bool {
        let amplitude = |c: &BrickwallControls| c.ceiling.to_amplitude().get();
        (amplitude(target) - amplitude(applied)).abs() > DEAD_BAND
    }
}

/// The hard clipper as a [`Kernel`]: a [`BrickwallBank`], memoryless, so no latency and no
/// tail. It holds no ring-out and never parks: its tail is none.
#[derive(Clone, Debug)]
pub struct BrickwallKernel<F: Float> {
    input: ChannelTopology,
    bank: BrickwallBank<F>,
}

impl<F: Float> BrickwallKernel<F> {
    /// A clipper on `input`, at the full scale until set.
    fn new(input: ChannelTopology) -> Self {
        Self {
            input,
            bank: BrickwallBank::new(),
        }
    }
}

impl<F: Float> Kernel for BrickwallKernel<F> {
    type Float = F;
    type Set = BrickwallSet;

    /// A cell that moves reaches the clamp within 16 frames, wherever the blocks fall.
    const GRID: usize = COEFF_INTERVAL;

    fn shape(&self) -> Shape {
        Shape::through(self.input).cost(Cost::Light)
    }

    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {}

    fn reset(&mut self) {}

    fn retarget(&mut self, controls: &BrickwallControls) {
        self.bank.retarget(controls);
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        signals: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let signals = BrickwallSignals {
            ceiling: signals.get(BrickwallSet::CEILING_SIGNAL),
        };
        self.bank.process(signals, block)
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

impl BrickwallSet {
    /// The control signal of the ceiling: the one [`fast`](crate::ControlRead::Fast) row.
    const CEILING_SIGNAL: usize = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        RATE_44K, fed_sample_gain, prepared, prepared_at, sample_gain, tick, tick_fed,
    };
    use tutti_buffers::ChannelLayout;
    use tutti_buffers::Planes;
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_units::UnitParam;

    // The nodes under test are `f32` (the session's audio format), so their tolerances are
    // `f32`'s; each node has an `f64` kernel test below.

    #[test]
    fn a_brickwall_fork_shares_no_cell() {
        assert_param_fork(BrickwallLimiterNode::<f32>::from(
            BrickwallLimiterConfig::new(ChannelTopology::STEREO, Db::new(-1.0)),
        ));
    }

    /// A ceiling set through the brickwall's `ParamSet` reaches the clip on
    /// the next block: the node designs from the cell once per block.
    #[test]
    fn a_brickwall_ceiling_set_by_address_reaches_the_clip() {
        let mut bw = prepared_at(
            BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
                ChannelTopology::STEREO,
                Db::new(0.0),
            )),
            RATE_44K,
        );
        let set = bw.param_set();
        assert!(set.set(UnitParam::Ceiling, -6.0));
        let mut out = [0.0f32; 2];
        tick(&mut bw, &[1.0, -1.0], &mut out);
        let ceiling = sample_gain(Db::new(-6.0));
        assert!((out[0] - ceiling).abs() < 1e-4, "clipped at {}", out[0]);
    }

    #[test]
    fn the_configured_ceiling_is_the_clip_level() {
        let mut bw = prepared(BrickwallLimiterNode::<f32>::from(
            BrickwallLimiterConfig::new(ChannelTopology::STEREO, Db::new(-6.0)),
        ));
        let ceiling_lin = sample_gain(Db::new(-6.0));

        let mut out = [0.0f32; 2];
        tick(&mut bw, &[1.0, -1.0], &mut out);

        assert!(
            (out[0] - ceiling_lin).abs() < 0.001,
            "Should clip to {ceiling_lin}, got {}",
            out[0]
        );
    }

    #[test]
    fn brickwall_fed_ceiling_modulates_clip() {
        // A fed ceiling held low clips harder than one held high.
        let mut bw = prepared(BrickwallLimiterNode::<f32>::from(
            BrickwallLimiterConfig::new(ChannelTopology::STEREO, Db::new(0.0)),
        ));
        let mut out = [0.0f32; 2];
        // Ceiling -12 dB ≈ 0.251 linear: 1.0 clips to ~0.251.
        tick_fed(&mut bw, &[1.0, 1.0], &[Some(-12.0)], &mut out);
        let low_ceiling = out[0];
        // Ceiling 0 dB = 1.0 linear: 1.0 passes through.
        tick_fed(&mut bw, &[1.0, 1.0], &[Some(0.0)], &mut out);
        let high_ceiling = out[0];
        assert!(
            high_ceiling > low_ceiling + 0.1,
            "higher fed ceiling should clip less: low={low_ceiling}, high={high_ceiling}"
        );
    }

    /// A NaN fed on the ceiling port clamps at the cell's ceiling rather
    /// than panicking in `f32::clamp`, and the next finite frame clamps at
    /// its own.
    #[test]
    fn brickwall_a_fed_nan_ceiling_clamps_at_the_cell() {
        let mut bw = prepared_at(
            BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
                ChannelTopology::STEREO,
                Db::new(-6.0),
            )),
            RATE_44K,
        );
        let cell = sample_gain(Db::new(-6.0));
        let mut out = [0.0f32; 2];
        tick_fed(&mut bw, &[1.0, -1.0], &[Some(f32::NAN)], &mut out);
        assert_eq!(out, [cell, -cell]);
        tick_fed(&mut bw, &[1.0, -1.0], &[Some(-12.0)], &mut out);
        let fed = fed_sample_gain(-12.0);
        assert_eq!(out, [fed, -fed]);
    }

    /// A NaN written to the ceiling cell keeps the ceiling the node last
    /// clamped at, and a later finite write takes effect; a NaN at
    /// construction starts at 0 dB.
    #[test]
    fn brickwall_a_nan_cell_keeps_the_last_ceiling() {
        let mut bw = prepared_at(
            BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
                ChannelTopology::STEREO,
                Db::new(-6.0),
            )),
            RATE_44K,
        );
        let set = bw.param_set();
        let mut out = [0.0f32; 2];
        assert!(set.set(UnitParam::Ceiling, f64::NAN));
        tick(&mut bw, &[1.0, -1.0], &mut out);
        let held = sample_gain(Db::new(-6.0));
        assert_eq!(out, [held, -held]);
        assert!(set.set(UnitParam::Ceiling, -12.0));
        // The move lands on the next grid point, 16 frames from the last reset.
        for _ in 0..16 {
            tick(&mut bw, &[1.0, -1.0], &mut out);
        }
        let next = sample_gain(Db::new(-12.0));
        assert!((out[0] - next).abs() < 1e-6, "{}", out[0]);

        let mut born_nan = prepared_at(
            BrickwallLimiterNode::<f32>::from(BrickwallLimiterConfig::new(
                ChannelTopology::STEREO,
                Db::new(f64::NAN),
            )),
            RATE_44K,
        );
        tick(&mut born_nan, &[2.0, -2.0], &mut out);
        assert_eq!(out, [1.0, -1.0], "a NaN-built brickwall clamps at 0 dB");
        assert!(born_nan.param_set().set(UnitParam::Ceiling, f64::NAN));
        tick(&mut born_nan, &[2.0, -2.0], &mut out);
        assert_eq!(out, [1.0, -1.0], "a NaN write keeps the ceiling");
    }

    /// A block of every channel clips each sample against its own frame's
    /// fed ceiling: frame `i` is fed `-i/4` dB and every channel holds
    /// `±2`, so each output is exactly that frame's linear ceiling, signed,
    /// and a quiet sample passes through.
    #[test]
    fn brickwall_clips_each_frame_at_its_own_ceiling() {
        use crate::test_support::RATE;
        use tutti_graph::contract::drive;
        const N: usize = 64;
        let fed: Vec<f32> = (0..N).map(|i| -(i as f32) / 4.0).collect();
        let ins: Vec<Vec<f32>> = (0..3)
            .map(|c| {
                (0..N)
                    .map(|i| match (i + c) % 3 {
                        0 => 2.0,
                        1 => -2.0,
                        _ => 0.0625,
                    })
                    .collect()
            })
            .collect();
        let refs: Vec<&[f32]> = ins.iter().map(|c| &c[..]).collect();
        let mut bw = prepared(BrickwallLimiterNode::<f32>::from(
            BrickwallLimiterConfig::new(
                ChannelTopology::unassigned(ChannelLayout::from(3u16)).unwrap(),
                Db::new(0.0),
            ),
        ));
        let out = drive(&mut bw, RATE, Planes::new(&refs).unwrap(), &[Some(&fed)]);
        for (c, (o, x)) in out.iter().zip(&ins).enumerate() {
            for (i, (&y, &x)) in o.iter().zip(x).enumerate() {
                let ceiling = fed_sample_gain(fed[i]);
                let want = if x.abs() > ceiling {
                    ceiling.copysign(x)
                } else {
                    x
                };
                assert_eq!(y, want, "fed: channel {c}, frame {i}");
            }
        }
        // Unfed: one ceiling, 0 dB, for the whole block.
        let out = drive(&mut bw, RATE, Planes::new(&refs).unwrap(), &[None]);
        for (c, (o, x)) in out.iter().zip(&ins).enumerate() {
            for (i, (&y, &x)) in o.iter().zip(x).enumerate() {
                let want = if x.abs() > 1.0 { x.signum() } else { x };
                assert_eq!(y, want, "unfed: channel {c}, frame {i}");
            }
        }
    }

    #[test]
    fn a_brickwall_reports_its_bus_width_and_clips_all() {
        let mut bw = prepared(BrickwallLimiterNode::<f32>::from(
            BrickwallLimiterConfig::new(
                ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap(),
                Db::new(0.0),
            ),
        ));
        assert_eq!(bw.shape().audio_in().count(), 6);
        assert_eq!(bw.shape().audio_out().count(), 6);
        let mut out = [0.0f32; 6];
        tick(&mut bw, &[2.0, -2.0, 3.0, -3.0, 0.5, -0.5], &mut out);
        for (c, &y) in out.iter().enumerate() {
            assert!(
                (-1.0 - 1e-6..=1.0 + 1e-6).contains(&y),
                "ch{c} not clipped: {y}"
            );
        }
        assert!((out[0] - 1.0).abs() < 1e-4);
        assert!((out[1] + 1.0).abs() < 1e-4);
    }
}
