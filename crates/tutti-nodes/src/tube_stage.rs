//! [`TubeStageNode`]: a 12AX7 common-cathode gain stage, oversampled.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, InputOutput, MaxBlock, Mono};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, Lerp, OversamplingFactor, TubeDrive, TubeStage,
    TubeStageControls,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Db, SampleRate, Samples, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode, unassigned_bus};
use crate::oversampled::Oversampled;

/// What a [`TubeStageNode`] is made from: its width, its controls and its oversampling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TubeStageConfig {
    width: ChannelLayout,
    controls: TubeStageControls,
    factor: OversamplingFactor,
}

impl TubeStageConfig {
    /// A stage of `width` channels at [`TubeStageControls::DEFAULT`] (+6 dB), oversampled 4
    /// times: 192 kHz at 48 kHz, the rate the solver's residual is tested at (the bound holds
    /// at every factor).
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            controls: TubeStageControls::DEFAULT,
            factor: OversamplingFactor::X4,
        }
    }

    /// The drive it starts at.
    #[must_use]
    pub fn drive(mut self, drive: TubeDrive) -> Self {
        self.controls.drive = drive;
        self
    }

    /// The oversampling the stage runs at; the default is 4 times.
    #[must_use]
    pub fn oversampling(mut self, factor: OversamplingFactor) -> Self {
        self.factor = factor;
        self
    }
}

/// A tube stage of any width: every channel a 12AX7 common-cathode stage with a plate load and
/// a coupling capacitor ([`TubeStage`]), run at [`OversamplingFactor`] times the rate. A small
/// signal at drive 0 dB has gain 1 and the stage is asymmetric: a hot sine has even
/// harmonics.
///
/// # Controls
///
/// The drive is a live [`Db`] cell (`UnitParam::GainDb`), sampled at the stream grid (16
/// frames) and ramping over [`GAIN_RAMP`](tutti_dsp::GAIN_RAMP). A control signal has no caller
/// to refuse a value to: a drive out of [`TubeDrive`]'s range reads as its nearest end
/// ([`TubeDrive::held`]), and a value that is not finite reads as the last one that was.
///
/// # Latency and tail
///
/// It delays the audio by the oversampler's cascade, which it declares. The tail is that
/// latency plus the frames the coupling capacitor rings after the input stops
/// ([`TubeStageControls::tail`]).
///
/// Heavy: four Newton steps and a seed per sample at the oversampled rate. A [`KernelNode`],
/// so a non-finite sample is heard in at most one block. Made from a [`TubeStageConfig`]
/// (`TubeStageNode::from`).
///
/// # Panics
///
/// `TubeStageNode::from` panics past 16 channels: a stage is one bus.
///
/// `F` is the float of the graph it runs in, and of the solve.
pub type TubeStageNode<F> = KernelNode<TubeStageKernel<F>>;

impl<F: Float + Lerp> From<TubeStageConfig> for TubeStageNode<F> {
    fn from(config: TubeStageConfig) -> Self {
        let input = unassigned_bus(config.width, "a tube stage");
        Self::with_kernel(
            TubeStageKernel::<F>::new(&config, input),
            TubeStageSet,
            config.controls,
        )
    }
}

/// The cell of a [`TubeStageNode`]: the drive.
#[derive(Clone, Copy, Debug)]
pub struct TubeStageSet;

control_set! {
    TubeStageSet(_s): TubeStageControls = TubeStageControls::DEFAULT;
    live: (all);
    GainDb => Db = drive via TubeDrive;

    fn defaults(&self) -> TubeStageControls {
        TubeStageControls::DEFAULT
    }
}

/// The [`Kernel`] of a [`TubeStageNode`].
#[derive(Clone, Debug)]
pub struct TubeStageKernel<F: Float> {
    input: BusTopology,
    factor: OversamplingFactor,
    /// The rate the stage runs at: the oversampled one, or the stream's where no half-band can
    /// be designed.
    rate: SampleRate,
    stage: TubeStage<F>,
    applied: bool,
    oversampled: Option<Oversampled<F>>,
}

impl<F: Float + Lerp> TubeStageKernel<F> {
    fn new(config: &TubeStageConfig, input: BusTopology) -> Self {
        Self {
            input,
            factor: config.factor,
            rate: SampleRate::SR_48K,
            stage: TubeStage::<F>::new(config.width, SampleRate::SR_48K),
            applied: false,
            oversampled: None,
        }
    }

    /// The frames of the stream the stage rings: the coupling capacitor's, at the oversampled
    /// rate, over the factor, and the cascade.
    fn ring(&self) -> Samples {
        let (factor, cascade) = self.oversampled.as_ref().map_or((1, Samples::ZERO), |o| {
            (o.plan.factor().get(), o.plan.latency())
        });
        let coupling = TubeStageControls::DEFAULT
            .tail(self.rate)
            .get()
            .div_ceil(factor);
        Samples::new(coupling + cascade.get() + 1)
    }
}

impl<F: Float + Lerp> Kernel for TubeStageKernel<F> {
    type Float = F;
    type Set = TubeStageSet;

    const GRID: usize = COEFF_INTERVAL;

    fn shape(&self) -> Shape {
        Shape::through(self.input).cost(Cost::Heavy)
    }

    fn prepare(&mut self, rate: SampleRate, max_block: MaxBlock) {
        let width = usize::from(self.input.width().count());
        self.oversampled = Oversampled::new(self.factor, rate, max_block, width);
        self.rate = self
            .oversampled
            .as_ref()
            .map_or(rate, |o| o.plan.high_rate());
        self.stage = TubeStage::<F>::new(self.input.width(), self.rate);
        self.applied = false;
    }

    fn reset(&mut self) {
        self.stage.reset();
        self.applied = false;
        for state in self.oversampled.iter_mut().flat_map(|o| &mut o.channels) {
            state.reset();
        }
    }

    fn retarget(&mut self, controls: &TubeStageControls) {
        self.stage.retarget(controls);
        if !self.applied {
            self.stage.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let rendered = match &mut self.oversampled {
            Some(oversampled) => oversampled.render(None::<Mono<'_, F>>, block, |_, wide| {
                self.stage.process(wide)
            }),
            None => self.stage.process(block),
        };
        // A design after a render is a move, which ramps.
        self.applied = true;
        rendered
    }

    fn latency(&self) -> Samples {
        self.oversampled
            .as_ref()
            .map_or(Samples::ZERO, |o| o.plan.latency())
    }

    /// The ring-out of the coupling capacitor, which no control changes.
    fn tail(&self) -> Tail {
        Tail::Finite(self.ring())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{amplitude_at, drive_block, prepared, render_chunked, sine};

    use tutti_graph::{Node, ParamNode};
    use tutti_units::UnitParam;

    const RATE: f64 = 48_000.0;

    fn node(drive: f64) -> TubeStageNode<f32> {
        prepared(TubeStageNode::<f32>::from(
            TubeStageConfig::new(1u16).drive(TubeDrive::of(Db::new(drive))),
        ))
    }

    #[test]
    fn the_shape_is_heavy_and_declares_the_drive() {
        let n = TubeStageNode::<f32>::from(TubeStageConfig::new(3u16));
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.cost, Cost::Heavy);
        assert_eq!(shape.params.as_slice(), &[UnitParam::GainDb][..]);
        assert_eq!(n.param_base(0), Some(6.0));
        let empty = TubeStageNode::<f32>::from(TubeStageConfig::new(0u16));
        assert_eq!(empty.shape().audio_in().count(), 0);
    }

    /// The impulse of a small input peaks at the oversampler's declared latency, and the
    /// output has rung out by the declared tail.
    #[test]
    fn the_oversampler_delays_the_audio_by_the_declared_latency_and_the_tail_covers_the_ring() {
        for factor in [
            OversamplingFactor::X2,
            OversamplingFactor::X4,
            OversamplingFactor::X8,
        ] {
            let mut n = prepared(TubeStageNode::<f32>::from(
                TubeStageConfig::new(1u16)
                    .drive(TubeDrive::of(Db::new(-40.0)))
                    .oversampling(factor),
            ));
            let latency = n.shape().latency.samples().get();
            let Tail::Finite(tail) = n.shape().tail else {
                panic!("{factor:?}: no finite tail");
            };
            assert!(latency > 0, "{factor:?}");
            let mut x = vec![0.0f32; 40_000];
            x[0] = 0.5;
            let y = render_chunked(&mut n, &[x], 100).remove(0);
            let peak = (0..y.len())
                .max_by(|&a, &b| y[a].abs().total_cmp(&y[b].abs()))
                .unwrap();
            assert_eq!(peak, latency, "{factor:?}");
            assert!(
                y[latency + tail.get() + 1..]
                    .iter()
                    .all(|&s| s.abs() < 1e-5),
                "{factor:?}: rings past the declared tail"
            );
        }
    }

    /// The drive is a live cell: +20 dB raises a hot output by less than the 10 times it
    /// raises the input, since the tube saturates.
    #[test]
    fn the_drive_is_settable_by_unit_param_and_the_tube_saturates() {
        let mut n = node(0.0);
        let x: Vec<f32> = sine(1_000.0, RATE, 4_800).iter().map(|v| v * 0.5).collect();
        let before = render_chunked(&mut n, std::slice::from_ref(&x), 256).remove(0);
        assert!(n.param_set().set(UnitParam::GainDb, 20.0));
        let _ = drive_block(&mut n, &[&x[..64]]);
        let after = render_chunked(&mut n, &[x], 256).remove(0);
        let (a, b) = (
            amplitude_at(&before[2_400..], 1_000.0, RATE),
            amplitude_at(&after[2_400..], 1_000.0, RATE),
        );
        assert!(b > a && b < 10.0 * a, "{a} -> {b}");
        assert!(!n.param_set().set(UnitParam::Cutoff, 2.0));
    }
}
