//! [`AutoPanNode`]: an LFO places a stereo pair between left and right.

use tutti_buffers::Float;
use tutti_buffers::{ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{AutoPan, AutoPanControls, BlockMismatch, COEFF_INTERVAL, OscillatorWave, PanLaw};
use tutti_graph::{Cost, Shape};
use tutti_units::{Bipolar, Hz, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What an [`AutoPanNode`] is made from: the wave of its LFO, the pan law and where its
/// controls start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoPanConfig {
    wave: OscillatorWave,
    law: PanLaw,
    controls: AutoPanControls,
}

impl AutoPanConfig {
    /// An auto-pan on a sine LFO under [`PanLaw::Rp036`], at [`AutoPanControls::DEFAULT`]
    /// (1 Hz across the whole field).
    #[must_use]
    pub fn new() -> Self {
        Self {
            wave: OscillatorWave::Sine,
            law: PanLaw::Rp036,
            controls: AutoPanControls::DEFAULT,
        }
    }

    /// The wave of the LFO.
    #[must_use]
    pub fn wave(self, wave: OscillatorWave) -> Self {
        Self { wave, ..self }
    }

    /// The pan law the position is placed by.
    #[must_use]
    pub fn law(self, law: PanLaw) -> Self {
        Self { law, ..self }
    }

    /// The rate it starts at.
    #[must_use]
    pub fn rate(mut self, rate: Hz) -> Self {
        self.controls.rate = rate;
        self
    }

    /// The depth it starts at.
    #[must_use]
    pub fn depth(mut self, depth: Bipolar) -> Self {
        self.controls.depth = depth;
        self
    }
}

impl Default for AutoPanConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// An auto-pan of a stereo pair: left scaled by the left gain of [`pan_gains`](tutti_dsp::pan_gains)
/// and right by its right gain, at the position `d u` of the depth `d` and the LFO `u` in
/// `-1..=1` ([`AutoPan`]).
///
/// Depth 1 sweeps hard left to hard right and a negative depth sweeps the other way; each
/// channel is the law's -3 dB at the centre, and the gains' power sums to 1 at every
/// position. The LFO starts at phase 0 and runs on across blocks. The rate
/// (`UnitParam::Rate`, a [`Hz`]) and the depth (`UnitParam::Depth`, a [`Bipolar`]) are live
/// cells, read as the [`TremoloNode`](crate::TremoloNode)'s are.
///
/// No latency and no tail. A [`KernelNode`]. Made from an [`AutoPanConfig`]
/// (`AutoPanNode::from`).
pub type AutoPanNode<F> = KernelNode<AutoPanKernel<F>>;

impl<F: Float> From<AutoPanConfig> for AutoPanNode<F> {
    fn from(config: AutoPanConfig) -> Self {
        // A placeholder rate: an unprepared node renders nothing.
        let kernel = AutoPanKernel {
            wave: config.wave,
            law: config.law,
            pan: AutoPan::new(config.wave, config.law, SampleRate::SR_48K),
            float: core::marker::PhantomData,
        };
        Self::with_kernel(kernel, AutoPanSet, config.controls)
    }
}

/// The auto-pan's controls: the rate and the depth.
#[derive(Clone, Copy, Debug)]
pub struct AutoPanSet;

control_set! {
    AutoPanSet(_s): AutoPanControls = AutoPanControls::DEFAULT;
    live: (all);
    Rate => Hz = rate { whole },
    Depth => Bipolar = depth;

    fn defaults(&self) -> AutoPanControls {
        AutoPanControls::DEFAULT
    }
}

/// The auto-pan as a [`Kernel`]: an [`AutoPan`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct AutoPanKernel<F: Float> {
    wave: OscillatorWave,
    law: PanLaw,
    pan: AutoPan,
    float: core::marker::PhantomData<fn() -> F>,
}

impl<F: Float> Kernel for AutoPanKernel<F> {
    type Float = F;
    type Set = AutoPanSet;

    const GRID: usize = COEFF_INTERVAL;

    /// Left/right in and out, `Moderate`: a sine and a pan law per sample.
    fn shape(&self) -> Shape {
        Shape::through(ChannelTopology::STEREO).cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.pan = AutoPan::new(self.wave, self.law, rate);
    }

    fn reset(&mut self) {
        self.pan.reset();
    }

    fn retarget(&mut self, controls: &AutoPanControls) {
        self.pan.retarget(controls);
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.pan.process(block)
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{drive_block, prepared};
    use tutti_graph::Node;

    use tutti_units::Tail;
    use tutti_units::UnitParam;

    /// The shape is a stereo pair in and out, `Moderate`, no tail, with the rate and depth
    /// declared; the default is 1 Hz at full depth.
    #[test]
    fn the_shape_is_a_stereo_pair_with_rate_and_depth() {
        let n = prepared(AutoPanNode::from(AutoPanConfig::new()));
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (2, 2)
        );
        assert_eq!(shape.cost, Cost::Moderate);
        assert_eq!(shape.tail, Tail::None);
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Rate, UnitParam::Depth][..]
        );
        assert_eq!((n.param_base(0), n.param_base(1)), (Some(1.0), Some(1.0)));
    }

    /// At 12 kHz and 48 kHz the sine reads 0, 1, 0, -1: full depth puts frame 1 hard right and
    /// frame 3 hard left under the RP-036 law.
    #[test]
    fn the_wave_places_the_pair_hard_right_at_its_peak_and_hard_left_at_its_trough() {
        let mut n = prepared(AutoPanNode::from(
            AutoPanConfig::new().rate(Hz::new(12_000.0)),
        ));
        let ones = [1.0f32; 4];
        let out = drive_block(&mut n, &[&ones, &ones]);
        assert!(out[0][1].abs() < 1e-6 && (out[1][1] - 1.0).abs() < 1e-6);
        assert!((out[0][3] - 1.0).abs() < 1e-6 && out[1][3].abs() < 1e-6);
    }
}
