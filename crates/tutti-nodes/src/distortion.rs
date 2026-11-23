//! The waveshaper [`DistortionNode`]: seven curves ([`ShapeKind`]) at a drive.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{
    Antialiasing, AntialiasingError, BlockMismatch, COEFF_INTERVAL, Lerp, OversamplingFactor,
    ShapeKind, WaveshaperBank, WaveshaperControls,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Drive, SampleRate, Samples, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode, unassigned_bus};
use crate::oversampled::Oversampled;

/// Waveshaping distortion of any width: `N` audio inputs and `N` outputs, every channel
/// shaped alone by the same curve at the same drive.
///
/// A [`KernelNode`] over [`DistortionKernel`], the curves and the drive's ramp being
/// [`WaveshaperBank`]'s; this file holds the cell and how the graph reaches it. The curve
/// ([`ShapeKind`]) is fixed at construction: a node with another one is another node. Made
/// from a [`DistortionConfig`] (`DistortionNode::from`).
///
/// # Drive
///
/// The drive is input gain into the curve, floored at 0 ([`ShapeKind::apply`] says what the
/// two staircases make of it). It is a live [`Params`](crate::Params) cell shared across
/// clones, set through the node's [`ParamSet`](tutti_graph::ParamSet) or the graph's
/// `set_param` (`UnitParam::Drive`); a value that is not finite reads as the last one that
/// was.
///
/// **It is read per frame** when the graph drives it: a control signal on its port is the bank's own, and a
/// frame of it that is not finite reads the drive in force: the cell's, ramping to its value. A
/// cell that moves is sampled at the stream grid (16 frames from the last reset, [`Kernel::GRID`])
/// and ramps over [`GAIN_RAMP`](tutti_dsp::GAIN_RAMP), 5 ms, so a write is no step at a block's
/// edge and renders the same whatever the blocks.
///
/// # Oversampling
///
/// A curve shapes a signal into harmonics past the Nyquist frequency, which fold back as
/// aliases. [`DistortionConfig::oversampling`] shapes at 2, 4 or 8 times the
/// rate instead: each channel goes up through [`Oversampler`](tutti_dsp::Oversampler)'s linear-phase half-band
/// cascade, through the curve and back down, so the harmonics the down-sampler's stopband
/// removes never fold. The cascade's round trip is a whole number of frames, which the node
/// declares as its latency, and its ring-out, as many frames again, as its tail. The drive's
/// ramp runs at the oversampled rate, and a drive fed per frame holds across the frames of
/// each input frame. Without oversampling or antialiasing the node is memoryless: it declares no
/// latency and no tail, a silent input is a silent output.
///
/// # Antialiasing
///
/// [`DistortionConfig::antialiasing`] shapes with antiderivative antialiasing instead
/// ([`Antialiasing`] has the forms): each frame is the curve's mean over the segment from the
/// input before it, which needs no extra rate and composes with oversampling, where it runs at
/// the oversampled rate. The history is the input before the drive, so a drive change makes no
/// step. The node then keeps state: it delays the audio by half a frame at first order and a
/// whole frame at second, which it declares rounded to a whole frame (half a frame as one), and
/// its tail is the frames the history rings.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, a fork of it starts from the
/// value last set through its [`ParamSet`](tutti_graph::ParamSet). The graph prepares it at
/// the device rate before its first block.
///
/// `F` is the float of the graph it runs in, and of the drive's ramp, the curve, the
/// antialiasing history and the oversampling.
pub type DistortionNode<F> = KernelNode<DistortionKernel<F>>;

/// What a [`DistortionNode`] is made from: its width, its curve, the drive it starts at and
/// how it avoids aliasing.
///
/// A config starts at unity drive, without oversampling or antialiasing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DistortionConfig {
    width: ChannelLayout,
    kind: ShapeKind,
    controls: WaveshaperControls,
    factor: Option<OversamplingFactor>,
    antialiasing: Antialiasing,
}

impl DistortionConfig {
    /// A waveshaper of `width` channels with the curve `kind`: every channel is shaped by the
    /// same (linked) drive and kind. An empty layout is no channels.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, kind: ShapeKind) -> Self {
        Self {
            width: width.into(),
            kind,
            controls: WaveshaperControls::DEFAULT,
            factor: None,
            antialiasing: Antialiasing::Off,
        }
    }

    /// The drive at the start: input gain into the shaping curve, floored at 0. Higher pushes
    /// further into the nonlinearity and distorts harder.
    #[must_use]
    pub fn drive(mut self, drive: Drive) -> Self {
        self.controls.drive = drive;
        self
    }

    /// Shapes at `factor` times the rate (see "Oversampling" on [`DistortionNode`]), up through
    /// the half-band cascade and back, instead of at the rate the node runs at.
    ///
    /// Drive changes, from the cell or a control signal, reach the shaper about half the latency
    /// ahead of the audio they were meant for: under 1 ms at 48 kHz for any factor.
    ///
    /// At a rate no half-band can be designed for (one where 0.1 times the rate overflows or
    /// underflows `f32`), the node runs without oversampling and declares no latency: a
    /// prepare does not fail.
    #[must_use]
    pub fn oversampling(mut self, factor: OversamplingFactor) -> Self {
        self.factor = Some(factor);
        self
    }

    /// Shapes with `order` of antiderivative antialiasing (see "Antialiasing" on
    /// [`DistortionNode`]); the default is [`Antialiasing::Off`], the memoryless curve.
    ///
    /// # Errors
    ///
    /// [`AntialiasingError`] when the kind has no antiderivative of that order
    /// ([`ShapeKind::max_antialiasing`]): `SecondOrder` is refused for `Tanh`, `Atan`,
    /// `Crush` and `SoftCrush`.
    pub fn antialiasing(mut self, order: Antialiasing) -> Result<Self, AntialiasingError> {
        self.antialiasing = self.kind.antialiasing(order)?;
        Ok(self)
    }
}

impl<F: Float + Lerp> From<DistortionConfig> for DistortionNode<F> {
    fn from(config: DistortionConfig) -> Self {
        let input = unassigned_bus(config.width, "a waveshaper");
        Self::with_kernel(
            DistortionKernel::<F>::new(&config, input),
            DistortionSet,
            config.controls,
        )
    }
}

/// The waveshaper's control: the drive.
#[derive(Clone, Copy, Debug)]
pub struct DistortionSet;

control_set! {
    DistortionSet(_s): WaveshaperControls = WaveshaperControls::DEFAULT;
    live: (all);
    Drive => Drive = drive { fast };

    fn defaults(&self) -> WaveshaperControls {
        WaveshaperControls::DEFAULT
    }
}

impl DistortionSet {
    /// The control signal of the drive: the one [`fast`](crate::ControlRead::Fast) row.
    const DRIVE_SIGNAL: usize = 0;
}

/// The waveshaper as a [`Kernel`]: a [`WaveshaperBank`] of one curve, at the prepared rate.
#[derive(Clone, Debug)]
pub struct DistortionKernel<F: Float> {
    kind: ShapeKind,
    input: ChannelTopology,
    /// The bank, at the rate it shapes at: the oversampled one when there is oversampling.
    bank: WaveshaperBank<F>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
    /// What the node was asked to shape at.
    factor: Option<OversamplingFactor>,
    /// The oversampling, once prepared at a rate its filters can be designed for.
    oversampled: Option<Oversampled<F>>,
    /// What the node was asked to shape with.
    antialiasing: Antialiasing,
}

impl<F: Float + Lerp> DistortionKernel<F> {
    /// A waveshaper of `config`'s curve on the bus `input`. Its rate until `prepare` is a
    /// placeholder: an unprepared node renders nothing.
    fn new(config: &DistortionConfig, input: ChannelTopology) -> Self {
        Self {
            kind: config.kind,
            input,
            bank: WaveshaperBank::<F>::new(config.kind, SampleRate::SR_48K),
            applied: false,
            factor: config.factor,
            oversampled: None,
            antialiasing: config.antialiasing,
        }
    }

    /// The oversampling factor the bank runs at: `None` without it.
    fn factor(&self) -> Option<OversamplingFactor> {
        self.oversampled.as_ref().map(|o| o.plan.factor())
    }
}

impl<F: Float + Lerp> Kernel for DistortionKernel<F> {
    type Float = F;
    type Set = DistortionSet;

    /// A cell that moves reaches the render within 16 frames, wherever the blocks fall.
    const GRID: usize = COEFF_INTERVAL;

    /// The input bus and one output bus where it is. The cost is the curve's: `Softsign` and
    /// `HardClip` are arithmetic, `Light`; `Crush` and `SoftCrush` (a rounding call) and
    /// `Atan` and `Tanh` (a libm call) are `Moderate`. With oversampling it is `Heavy`: every
    /// channel runs two half-band cascades and the curve at up to eight times the rate.
    /// Antialiasing makes every curve at least `Moderate`: the curve and its antiderivatives
    /// run in `f64`.
    fn shape(&self) -> Shape {
        let cost = match (self.factor, self.kind) {
            (Some(_), _) => Cost::Heavy,
            (None, ShapeKind::Softsign | ShapeKind::HardClip)
                if self.antialiasing == Antialiasing::Off =>
            {
                Cost::Light
            }
            (None, _) => Cost::Moderate,
        };
        Shape::through(self.input).cost(cost)
    }

    fn prepare(&mut self, rate: SampleRate, max_block: MaxBlock) {
        let width = usize::from(self.input.width().count());
        self.oversampled = self
            .factor
            .and_then(|factor| Oversampled::new(factor, rate, max_block, width));
        let shaping = self
            .oversampled
            .as_ref()
            .map_or(rate, |o| o.plan.high_rate());
        self.bank = WaveshaperBank::<F>::new(self.kind, shaping)
            .antialiasing(self.antialiasing)
            .expect("the config refused an order the kind lacks");
        self.applied = false;
    }

    /// Silence, and the next design lands at once, as a fresh node's first does.
    fn reset(&mut self) {
        self.bank.reset();
        self.applied = false;
        for state in self.oversampled.iter_mut().flat_map(|o| &mut o.channels) {
            state.reset();
        }
    }

    fn retarget(&mut self, controls: &WaveshaperControls) {
        self.bank.retarget(controls);
        if !self.applied {
            self.bank.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        signals: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let signal = signals.get(DistortionSet::DRIVE_SIGNAL);
        let rendered = match &mut self.oversampled {
            Some(oversampled) => {
                oversampled.render(signal, block, |held, wide| self.bank.process(held, wide))
            }
            None => self.bank.process(signal, block),
        };
        // A design after a render is a move, which ramps.
        self.applied = true;
        rendered
    }

    /// The cascade's round trip, in base frames, and the antialiasing's delay rounded to a
    /// whole frame: zero with neither.
    fn latency(&self) -> Samples {
        let cascade = self
            .oversampled
            .as_ref()
            .map_or(Samples::ZERO, |o| o.plan.latency());
        cascade + self.antialiasing.delay(self.factor())
    }

    /// The half-bands' response is symmetric about its delay, so what rings after the input
    /// stops lasts as long as the latency; a frame more covers the rounding. The history rings
    /// on after that.
    fn tail(&self) -> Tail {
        let cascade = self
            .oversampled
            .as_ref()
            .map_or(Samples::ZERO, |o| Samples::new(o.plan.latency().get() + 1));
        match cascade + self.antialiasing.ring_out(self.factor()) {
            Samples::ZERO => Tail::None,
            frames => Tail::Finite(frames),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        RATE, amplitude_at, assert_cell_ramps, drive_block, prepared, render_change,
        render_chunked, sine,
    };
    use tutti_buffers::Planes;
    use tutti_graph::contract::{assert_param_fork, drive};
    use tutti_graph::{Node, ParamNode};
    use tutti_units::UnitParam;

    const KINDS: [ShapeKind; 7] = [
        ShapeKind::Tanh,
        ShapeKind::Atan,
        ShapeKind::Softsign,
        ShapeKind::HardClip,
        ShapeKind::TriangleFold,
        ShapeKind::Crush,
        ShapeKind::SoftCrush,
    ];

    // The node tests run `F = f32`, the session's audio format: their tolerances are `f32`'s.
    fn hard_clip(channels: u16, drive: f64) -> DistortionNode<f32> {
        prepared(DistortionNode::<f32>::from(
            DistortionConfig::new(channels, ShapeKind::HardClip).drive(Drive::new(drive)),
        ))
    }

    /// A fork starts from the drive last set through the node's `ParamSet` and shares no
    /// cell with it.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(DistortionNode::<f32>::from(
            DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Tanh).drive(Drive::new(2.0)),
        ));
    }

    /// The node is stereo unless told otherwise, and the shape declares the drive.
    #[test]
    fn the_shape_is_stereo_and_declares_its_drive_param() {
        let n = DistortionNode::<f32>::from(
            DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Tanh).drive(Drive::new(1.5)),
        );
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (2, 2)
        );
        assert_eq!(shape.params.as_slice(), &[UnitParam::Drive][..]);
        assert_eq!(n.param_base(0), Some(1.5), "the base is the drive control");
    }

    /// An `n`-channel node reports `n` in and out, and shapes each channel at the same
    /// drive: a hard clip at drive 1 is the clamp to ±1 of each channel alone.
    #[test]
    fn a_wide_node_shapes_every_channel_alone() {
        let mut n = hard_clip(6, 1.0);
        assert_eq!(
            (n.shape().audio_in().count(), n.shape().audio_out().count()),
            (6, 6)
        );
        let x = [4.0f32, -4.0, 2.0, -2.0, 0.5, -0.5];
        let ins: Vec<&[f32]> = x.iter().map(std::slice::from_ref).collect();
        let out = drive_block(&mut n, &ins);
        let want = [1.0f32, -1.0, 1.0, -1.0, 0.5, -0.5];
        for (c, (y, w)) in out.iter().zip(want).enumerate() {
            assert_eq!(y[0], w, "channel {c}");
        }
    }

    /// An empty layout is no channels, not one.
    #[test]
    fn an_empty_layout_has_no_channels() {
        let n = DistortionNode::<f32>::from(
            DistortionConfig::new(0u16, ShapeKind::Tanh).drive(Drive::new(1.0)),
        );
        assert_eq!(n.shape().audio_in().count(), 0);
    }

    /// Drive is reachable by address through the node's `ParamSet`, and an address it does
    /// not own is refused.
    #[test]
    fn drive_is_settable_by_unit_param() {
        let n = DistortionNode::<f32>::from(
            DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Tanh).drive(Drive::new(1.0)),
        );
        let set = n.param_set();
        assert!(set.set(UnitParam::Drive, 5.0));
        assert_eq!(set.get(UnitParam::Drive), Some(5.0));
        assert!(!set.set(UnitParam::Cutoff, 1000.0));
    }

    /// A drive fed a step shapes every channel of a wide node from the step's own frame:
    /// each of six channels clips `0.25` at `1` before and `3` from frame 32, and a block
    /// whose port is not fed reads the cell again.
    #[test]
    fn a_fed_drive_shapes_every_channel_of_a_wide_node() {
        let mut n = hard_clip(6, 1.0);
        let x = vec![0.25f32; 64];
        let ins: Vec<&[f32]> = (0..6).map(|_| &x[..]).collect();
        let step: Vec<f32> = (0..64).map(|i| if i < 32 { 1.0 } else { 3.0 }).collect();
        let out = drive(&mut n, RATE, Planes::new(&ins).unwrap(), &[Some(&step)]);
        for (c, o) in out.iter().enumerate() {
            assert_eq!(o[31], 0.25, "channel {c} before the step");
            assert_eq!(o[32], 0.75, "channel {c} on the step's frame");
        }
        let out = drive(&mut n, RATE, Planes::new(&ins).unwrap(), &[None]);
        assert!(
            out.iter().all(|o| o.iter().all(|&y| y == 0.25)),
            "the cell again"
        );
    }

    /// A cell written between blocks ramps on the stream clock, whatever the blocks, and the
    /// ramp is the line from the old drive to the new over `GAIN_RAMP`: frame `i` of a hard
    /// clip of `0.2` is `0.2 (1 + 2 (i + 1) / n)`, and the frames after it are `0.6`.
    #[test]
    fn a_drive_written_between_blocks_ramps_over_the_ramp() {
        let make = || {
            DistortionNode::<f32>::from(
                DistortionConfig::new(1u16, ShapeKind::HardClip).drive(Drive::new(1.0)),
            )
        };
        let change = |n: &DistortionNode<f32>| {
            assert!(n.param_set().set(UnitParam::Drive, 3.0));
        };
        let before = vec![0.2f32; 64];
        let after = vec![0.2f32; 600];
        assert_cell_ramps(make, change, &[&before], &[&after], "drive");
        let (_, out) = render_change(make, change, &[&before], &[&after], 64);
        let n = tutti_dsp::GAIN_RAMP.to_samples_ceil(RATE).get();
        for (i, &y) in out[0].iter().enumerate().take(n) {
            let want = 0.2 * (1.0 + 2.0 * (i + 1) as f64 / n as f64);
            assert!((f64::from(y) - want).abs() < 1e-6, "frame {i}: {y}");
        }
        assert!(out[0][n..].iter().all(|&y| y == 0.2 * 3.0));
    }

    /// A drive that is not finite is refused, written or fed: the render stays at the last
    /// drive that was (a hard clip of `0.25` at 2 is `0.5`), and a built-in non-finite drive
    /// reads as unity.
    #[test]
    fn a_non_finite_drive_is_refused() {
        let mut n = hard_clip(1, 2.0);
        let x = vec![0.25f32; 64];
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(n.param_set().set(UnitParam::Drive, f64::from(bad)));
            let out = drive_block(&mut n, &[&x]);
            assert!(out[0].iter().all(|&y| y == 0.5), "written {bad}");
            let fed = vec![bad; 64];
            let out = drive(&mut n, RATE, Planes::new(&[&x]).unwrap(), &[Some(&fed)]);
            assert!(out[0].iter().all(|&y| y == 0.5), "fed {bad}");
        }
        let mut built = hard_clip(1, f64::NAN);
        let out = drive_block(&mut built, &[&x]);
        assert!(out[0].iter().all(|&y| y == 0.25), "built with NaN");
    }

    /// A cell written after a reset lands on the first frame, as a fresh node's first design
    /// does: a hard clip of `0.2` reads `0.6` at drive 3 from frame 0, though the drive was
    /// 1 before.
    #[test]
    fn a_cell_written_after_a_reset_lands_whole() {
        let mut n = hard_clip(1, 1.0);
        let x = vec![0.2f32; 64];
        drive_block(&mut n, &[&x]);
        Node::reset(&mut n);
        assert!(n.param_set().set(UnitParam::Drive, 3.0));
        let out = drive_block(&mut n, &[&x]);
        assert_eq!(out[0][0], 0.2 * 3.0);
        assert!(out[0].iter().all(|&y| y == 0.2 * 3.0));
    }

    /// A reset lands a drive in flight on its target: the render after it is the held curve
    /// from its first frame.
    #[test]
    fn a_reset_lands_the_drive() {
        let mut n = hard_clip(1, 1.0);
        let x = vec![0.2f32; 64];
        drive_block(&mut n, &[&x]);
        assert!(n.param_set().set(UnitParam::Drive, 3.0));
        let mid = drive_block(&mut n, &[&x[..10]]);
        assert!(mid[0][9] < 0.2 * 3.0, "still ramping");
        Node::reset(&mut n);
        let out = drive_block(&mut n, &[&x]);
        assert!(out[0].iter().all(|&y| y == 0.2 * 3.0));
    }

    /// The declared cost follows the curve: the two arithmetic-only curves are `Light`, the
    /// four that call a math routine `Moderate`; oversampled, every curve is `Heavy`.
    #[test]
    fn the_declared_cost_follows_the_curve() {
        for kind in KINDS {
            let want = match kind {
                ShapeKind::Softsign | ShapeKind::HardClip => Cost::Light,
                _ => Cost::Moderate,
            };
            assert_eq!(
                DistortionNode::<f32>::from(
                    DistortionConfig::new(ChannelLayout::STEREO, kind).drive(Drive::new(1.0))
                )
                .shape()
                .cost,
                want,
                "{kind:?}"
            );
            for factor in [
                OversamplingFactor::X2,
                OversamplingFactor::X4,
                OversamplingFactor::X8,
            ] {
                let n = DistortionNode::<f32>::from(
                    DistortionConfig::new(ChannelLayout::STEREO, kind)
                        .drive(Drive::new(1.0))
                        .oversampling(factor),
                );
                assert_eq!(n.shape().cost, Cost::Heavy, "{kind:?} {factor:?}");
            }
        }
    }

    /// The level, in dB under the fundamental, of the loudest of the aliased odd harmonics of a
    /// 9 kHz sine through `node` at 48 kHz: the 3rd (27 kHz) folds to 21 kHz, the 5th to 3 kHz
    /// and the 7th (63 kHz) to 15 kHz. The window holds 900 periods exactly, after the
    /// latency and the filters' settling.
    fn alias_db(node: DistortionNode<f32>) -> f64 {
        const HZ: f64 = 9_000.0;
        let rate = RATE.get();
        let x = sine(HZ, rate, 600 + 4_800);
        let mut node = prepared(node);
        let y = render_chunked(&mut node, &[x], 256).remove(0);
        let window = &y[600..];
        let fundamental = amplitude_at(window, HZ, rate);
        let alias = [3_000.0, 15_000.0, 21_000.0]
            .into_iter()
            .map(|hz| amplitude_at(window, hz, rate))
            .fold(0.0, f64::max);
        20.0 * (alias / fundamental).log10()
    }

    /// Oversampling declares the oversampler's round trip as the node's latency and the
    /// ring-out after it as its tail, none without it; and the audio is delayed by exactly that
    /// latency. A hard clip at drive 1 is the identity below 1, so an impulse of 0.5 comes out
    /// as the cascade's response: its peak is at the declared latency, and past the latency
    /// plus the tail the output is exactly zero.
    #[test]
    fn oversampling_delays_the_audio_by_the_declared_latency() {
        let plain = prepared(DistortionNode::<f32>::from(
            DistortionConfig::new(ChannelLayout::STEREO, ShapeKind::Tanh).drive(Drive::new(2.0)),
        ));
        assert_eq!(plain.shape().latency.samples(), Samples::ZERO);
        assert_eq!(plain.shape().tail, Tail::None);
        for factor in [
            OversamplingFactor::X2,
            OversamplingFactor::X4,
            OversamplingFactor::X8,
        ] {
            let mut n = prepared(DistortionNode::<f32>::from(
                DistortionConfig::new(1u16, ShapeKind::HardClip)
                    .drive(Drive::new(1.0))
                    .oversampling(factor),
            ));
            let latency = n.shape().latency.samples().get();
            let Tail::Finite(tail) = n.shape().tail else {
                panic!("{factor:?}: no finite tail");
            };
            assert!(latency > 0, "{factor:?}");
            let mut x = vec![0.0f32; 1000];
            x[0] = 0.5;
            let y = render_chunked(&mut n, &[x], 100).remove(0);
            let peak = (0..y.len())
                .max_by(|&a, &b| y[a].abs().total_cmp(&y[b].abs()))
                .unwrap();
            assert_eq!(peak, latency, "{factor:?}");
            assert!(
                y[latency + tail.get() + 1..].iter().all(|&s| s == 0.0),
                "{factor:?}: rings past the declared tail"
            );
        }
    }

    /// A driven sine aliases: tanh at drive 3 puts the 3rd, 5th and 7th harmonics of 9 kHz at
    /// 21, 3 and 15 kHz. The bounds are regression margins set below the measured levels (dB
    /// under the fundamental): plain -12.6 (bound above -20), 2x -39.5 (below -30), 4x -83.0
    /// (below -75) and 8x -128.7 (below -110).
    #[test]
    fn oversampling_removes_the_aliases() {
        let shaped = |factor: Option<OversamplingFactor>| {
            alias_db(match factor {
                None => DistortionNode::<f32>::from(
                    DistortionConfig::new(1u16, ShapeKind::Tanh).drive(Drive::new(3.0)),
                ),
                Some(f) => DistortionNode::<f32>::from(
                    DistortionConfig::new(1u16, ShapeKind::Tanh)
                        .drive(Drive::new(3.0))
                        .oversampling(f),
                ),
            })
        };
        let plain = shaped(None);
        assert!(plain > -20.0, "without oversampling: {plain} dB");
        for (factor, bound) in [
            (OversamplingFactor::X2, -30.0),
            (OversamplingFactor::X4, -75.0),
            (OversamplingFactor::X8, -110.0),
        ] {
            let level = shaped(Some(factor));
            assert!(level < bound, "{factor:?}: {level} dB, not under {bound}");
        }
    }

    /// A config refuses a second order for a kind with only a first, and keeps its order
    /// when it refuses.
    #[test]
    fn a_config_refuses_an_order_the_kind_lacks() {
        let tanh = DistortionConfig::new(1u16, ShapeKind::Tanh);
        let err = tanh.antialiasing(Antialiasing::SecondOrder).unwrap_err();
        assert_eq!(err.kind, ShapeKind::Tanh);
        assert_eq!(err.max, Antialiasing::FirstOrder);
        assert!(tanh.antialiasing(Antialiasing::FirstOrder).is_ok());
        let clip = DistortionConfig::new(1u16, ShapeKind::HardClip);
        assert!(clip.antialiasing(Antialiasing::SecondOrder).is_ok());
    }

    /// Antialiasing declares its delay, rounded to a whole frame, and the frames the history
    /// rings as the node's tail: first order half a frame (declared 1), second a frame; at 2x oversampling the delay is a
    /// fraction of a base frame that rounds into the cascade's.
    #[test]
    fn antialiasing_declares_its_delay_and_tail() {
        let node = |kind, order| {
            prepared(DistortionNode::<f32>::from(
                DistortionConfig::new(1u16, kind)
                    .drive(Drive::new(1.0))
                    .antialiasing(order)
                    .expect("an order the kind has"),
            ))
        };
        for (kind, order, latency, tail) in [
            (ShapeKind::HardClip, Antialiasing::Off, 0, Tail::None),
            (
                ShapeKind::HardClip,
                Antialiasing::FirstOrder,
                1,
                Tail::Finite(Samples::new(1)),
            ),
            (
                ShapeKind::HardClip,
                Antialiasing::SecondOrder,
                1,
                Tail::Finite(Samples::new(2)),
            ),
            (
                ShapeKind::Tanh,
                Antialiasing::FirstOrder,
                1,
                Tail::Finite(Samples::new(1)),
            ),
        ] {
            let shape = node(kind, order).shape();
            assert_eq!(shape.latency.samples().get(), latency, "{kind:?} {order:?}");
            assert_eq!(shape.tail, tail, "{kind:?} {order:?}");
        }
        let plain = prepared(DistortionNode::<f32>::from(
            DistortionConfig::new(1u16, ShapeKind::HardClip)
                .drive(Drive::new(1.0))
                .oversampling(OversamplingFactor::X2),
        ));
        let both = prepared(DistortionNode::<f32>::from(
            DistortionConfig::new(1u16, ShapeKind::HardClip)
                .drive(Drive::new(1.0))
                .oversampling(OversamplingFactor::X2)
                .antialiasing(Antialiasing::SecondOrder)
                .expect("an order the kind has"),
        ));
        assert_eq!(
            both.shape().latency.samples().get(),
            plain.shape().latency.samples().get() + 1,
        );
        assert_eq!(
            both.shape().tail,
            Tail::Finite(Samples::new(
                match plain.shape().tail {
                    Tail::Finite(t) => t.get(),
                    other => panic!("{other:?}"),
                } + 1
            ))
        );
    }

    /// A small impulse inside the clip comes out as the filter's closed form: `[x/2, x/2]` at
    /// first order and `[x/3; 3]` at second, and silence past the declared latency and tail.
    #[test]
    fn antialiasing_rings_for_its_declared_tail() {
        for (order, taps) in [
            (Antialiasing::FirstOrder, 2usize),
            (Antialiasing::SecondOrder, 3),
        ] {
            let mut n = prepared(DistortionNode::<f32>::from(
                DistortionConfig::new(1u16, ShapeKind::HardClip)
                    .drive(Drive::new(1.0))
                    .antialiasing(order)
                    .expect("an order the kind has"),
            ));
            let Tail::Finite(tail) = n.shape().tail else {
                panic!("{order:?}: no finite tail");
            };
            let mut x = vec![0.0f32; 32];
            x[0] = 0.5;
            let y = render_chunked(&mut n, &[x], 8).remove(0);
            for (i, &v) in y.iter().enumerate() {
                let want = if i < taps { 0.5 / taps as f32 } else { 0.0 };
                assert!((v - want).abs() < 1e-7, "{order:?} frame {i}: {v}");
            }
            assert_eq!(tail.get(), taps - 1, "{order:?}");
        }
    }

    /// First-order antialiasing lowers the aliases of the 9 kHz drive-3 tanh `alias_db`
    /// measures, alone and under 2x oversampling, where it runs at the oversampled rate. The
    /// margins are set under what is measured: alone -18.8 dB against the plain -12.6 (6.2 dB
    /// under; bound 4), at 2x -57.7 against -39.5 (18.2 dB under; bound 12).
    #[test]
    fn antialiasing_lowers_the_aliases_alone_and_oversampled() {
        let tanh = || DistortionConfig::new(1u16, ShapeKind::Tanh).drive(Drive::new(3.0));
        let (plain, first) = (
            alias_db(tanh().into()),
            alias_db(
                tanh()
                    .antialiasing(Antialiasing::FirstOrder)
                    .expect("an order the kind has")
                    .into(),
            ),
        );
        assert!(first < plain - 4.0, "{first} dB against {plain}");
        let x2 = || tanh().oversampling(OversamplingFactor::X2);
        let (plain, first) = (
            alias_db(x2().into()),
            alias_db(
                x2().antialiasing(Antialiasing::FirstOrder)
                    .expect("an order the kind has")
                    .into(),
            ),
        );
        assert!(first < plain - 12.0, "at 2x: {first} dB against {plain}");
    }

    /// Antialiasing makes every curve at least `Moderate`.
    #[test]
    fn antialiasing_is_at_least_moderate() {
        for kind in KINDS {
            let n = DistortionNode::<f32>::from(
                DistortionConfig::new(ChannelLayout::STEREO, kind)
                    .drive(Drive::new(1.0))
                    .antialiasing(Antialiasing::FirstOrder)
                    .expect("an order the kind has"),
            );
            assert_eq!(n.shape().cost, Cost::Moderate, "{kind:?}");
        }
    }

    /// A reset clears the oversampler: a block of silence after loud ones is exactly silent.
    #[test]
    fn a_reset_clears_the_oversampler() {
        let mut n = prepared(DistortionNode::<f32>::from(
            DistortionConfig::new(1u16, ShapeKind::Tanh)
                .drive(Drive::new(4.0))
                .oversampling(OversamplingFactor::X4),
        ));
        let loud = sine(1_000.0, RATE.get(), 256);
        assert!(drive_block(&mut n, &[&loud])[0].iter().any(|&y| y != 0.0));
        Node::reset(&mut n);
        let out = drive_block(&mut n, &[&[0.0; 256]]);
        assert!(out[0].iter().all(|&y| y == 0.0));
    }

    /// An `f64` block through a `DistortionKernel::<f64>` of `Softsign` at the drive `h`
    /// is `h x / (1 + |h x|)` in `f64`, to 2^-45, and its
    /// first-order antialiased form on a signal that alternates in sign is
    /// `(F(x[n]) - F(x[n-1])) / (x[n] - x[n-1])` with `F(u) = |u| - ln(1 + |u|)` the
    /// antiderivative of `u / (1 + |u|)` that is 0 at 0, the history starting at 0.
    #[test]
    fn an_f64_kernel_is_the_curve_and_its_divided_difference_in_f64() {
        use crate::test_support::drive_kernel;
        use tutti_fixtures::signal::{assert_close_f64, wide_signal};
        let len = 200;
        let x: Vec<f64> = wide_signal(len)
            .iter()
            .enumerate()
            .map(|(n, v)| if n % 2 == 0 { *v } else { -2.0 * v })
            .collect();
        let h = 1.7;
        let controls = WaveshaperControls {
            drive: Drive::new(h),
        };
        let render = |order: Antialiasing| {
            let config = DistortionConfig::new(ChannelLayout::MONO, ShapeKind::Softsign)
                .antialiasing(order)
                .expect("an order the kind has");
            let mut k = DistortionKernel::<f64>::new(&config, ChannelTopology::MONO);
            k.prepare(RATE, MaxBlock::of(Samples::new(len)));
            drive_kernel(&mut k, &[(0, controls)], std::slice::from_ref(&x)).remove(0)
        };
        let want: Vec<f64> = x.iter().map(|x| h * x / (1.0 + (h * x).abs())).collect();
        assert_close_f64(&render(Antialiasing::Off), &want, 2.0f64.powi(-45));
        let antiderivative = |u: f64| u.abs() - u.abs().ln_1p();
        let mut before = 0.0f64;
        let want: Vec<f64> = x
            .iter()
            .map(|&now| {
                let (a, b) = (h * now, h * before);
                before = now;
                (antiderivative(a) - antiderivative(b)) / (a - b)
            })
            .collect();
        assert_close_f64(&render(Antialiasing::FirstOrder), &want, 2.0f64.powi(-45));
    }
}
