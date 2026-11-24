//! [`DcBlockerNode`]: a first-order high-pass that removes a constant offset.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, COEFF_INTERVAL, DcBlocker, DcBlockerControls, Lerp, MIN_DC_CORNER};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode, unassigned_bus};

/// What a [`DcBlockerNode`] is made from: its width and, optionally, its corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DcBlockerConfig {
    width: ChannelLayout,
    controls: DcBlockerControls,
}

impl DcBlockerConfig {
    /// A blocker of `width` channels at the default corner ([`DC_CORNER`](tutti_dsp::DC_CORNER),
    /// 10 Hz).
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            controls: DcBlockerControls::DEFAULT,
        }
    }

    /// The corner it starts at, read within [`MIN_DC_CORNER`] and the Nyquist.
    #[must_use]
    pub fn corner(self, corner: Hz) -> Self {
        Self {
            controls: DcBlockerControls { corner },
            ..self
        }
    }
}

/// A DC blocker of any width: `y[n] = x[n] - x[n-1] + R y[n-1]` on every channel alone,
/// with `R = exp(-2 pi fc / fs)` for the corner `fc` at the prepared rate `fs`.
///
/// A constant input decays as `R^n`; the gain falls to `1/sqrt(2)` near the corner and is
/// 1 well above it. The corner is a live [`Hz`] cell (`UnitParam::Cutoff`, 10 Hz by
/// default), read within [`MIN_DC_CORNER`] and the Nyquist, sampled at the stream grid (16
/// frames) and reached along a line over the next 16 frames with the state kept, so a move
/// steps the output by no more than the input's step plus `(1 - R) |y|` per frame. A
/// cell written with a value that is not finite reads as the last one that was; a corner
/// built with one reads as the default.
///
/// The tail is finite: the frames for a stopped input to fall under the tail floor,
/// falling as `R^n` ([`DcBlockerControls::tail`]), narrowed per block to the corner in force. No
/// latency. A [`KernelNode`], so a non-finite sample is heard in at most one block. Made
/// from a [`DcBlockerConfig`] (`DcBlockerNode::from`).
///
/// # Panics
///
/// `DcBlockerNode::from` panics past 16 channels: a blocker is one bus.
///
/// `F` is the float of the graph it runs in, and of the recursion and the pole.
pub type DcBlockerNode<F> = KernelNode<DcBlockerKernel<F>>;

impl<F: Float + Lerp> From<DcBlockerConfig> for DcBlockerNode<F> {
    fn from(config: DcBlockerConfig) -> Self {
        Self::with_kernel(
            DcBlockerKernel::<F>::new(config.width),
            DcBlockerSet,
            config.controls,
        )
    }
}

/// The blocker's control: the corner.
#[derive(Clone, Copy, Debug)]
pub struct DcBlockerSet;

control_set! {
    DcBlockerSet(_s): DcBlockerControls = DcBlockerControls::DEFAULT;
    live: (all);
    Cutoff => Hz = corner { whole };

    fn defaults(&self) -> DcBlockerControls {
        DcBlockerControls::DEFAULT
    }
}

/// The blocker as a [`Kernel`]: a [`DcBlocker`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct DcBlockerKernel<F: Float> {
    width: ChannelLayout,
    rate: SampleRate,
    blocker: DcBlocker<F>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
}

impl<F: Float + Lerp> DcBlockerKernel<F> {
    /// A blocker of `width` at a placeholder rate: an unprepared node renders nothing.
    fn new(width: ChannelLayout) -> Self {
        let rate = SampleRate::SR_48K;
        Self {
            width,
            rate,
            blocker: DcBlocker::<F>::new(width, rate),
            applied: false,
        }
    }
}

impl<F: Float + Lerp> Kernel for DcBlockerKernel<F> {
    type Float = F;
    type Set = DcBlockerSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Light`: a multiply-add per sample.
    fn shape(&self) -> Shape {
        let bus = unassigned_bus(self.width, "a DC blocker");
        Shape::through(bus).cost(Cost::Light)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.blocker = DcBlocker::<F>::new(self.width, rate);
        self.applied = false;
    }

    /// Silence, and the next design lands at once, as a fresh node's first does.
    fn reset(&mut self) {
        self.blocker.reset();
        self.applied = false;
    }

    fn retarget(&mut self, controls: &DcBlockerControls) {
        self.blocker.retarget(controls);
        if !self.applied {
            self.blocker.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let rendered = self.blocker.process(block);
        // A design after a render is a move, which ramps.
        self.applied = true;
        rendered
    }

    /// The ring-out at the lowest corner: the longest over every control.
    fn tail(&self) -> Tail {
        Tail::Finite(
            DcBlockerControls {
                corner: MIN_DC_CORNER,
            }
            .tail(self.rate),
        )
    }

    fn tail_of(&mut self, controls: &DcBlockerControls) -> Tail {
        Tail::Finite(controls.tail(self.rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE, drive_block, prepared};
    use tutti_graph::ParamNode;
    use tutti_units::UnitParam;

    // The node tests run `F = f32`, the session's audio format: their tolerances are `f32`'s.
    fn node(width: u16, corner: f64) -> DcBlockerNode<f32> {
        prepared(DcBlockerNode::<f32>::from(
            DcBlockerConfig::new(width).corner(Hz::new(corner)),
        ))
    }

    /// The corner is settable by `Cutoff`: moved to 100 Hz it decays faster (`R^n` of the
    /// new pole, from the next retarget).
    #[test]
    fn the_corner_is_settable_by_cutoff() {
        let mut n = node(1, 10.0);
        assert!(n.param_set().set(UnitParam::Cutoff, 100.0));
        let out = drive_block(&mut n, &[&[1.0; 512]]);
        let r = (-std::f64::consts::TAU * 100.0 / RATE.get()).exp();
        assert!((f64::from(out[0][511]) - r.powi(511)).abs() < 1e-6);
        assert!(out[0][511] < 0.01, "10 Hz would leave 0.5");
    }
}
