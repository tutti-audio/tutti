//! [`StereoWidthNode`]: the side of a stereo pair scaled by a width.

use tutti_buffers::Float;
use tutti_buffers::{ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, COEFF_INTERVAL, Lerp, WidthBank, WidthControls};
use tutti_graph::{Cost, Shape};
use tutti_units::{SampleRate, StereoWidth, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// Stereo width: left/right in, left/right out, `L = M + w S` and `R = M - w S` with the
/// mid `M = (L + R) / 2` and the side `S = (L - R) / 2` of the input.
///
/// Width 0 is the mono sum (`L = R = M`), 1 the pair as it is and 2 doubles the side. The
/// width is a live [`StereoWidth`] cell, read from `0` up with no upper bound (the unit
/// holds `0..`), set through the node's [`ParamSet`](tutti_graph::ParamSet)
/// (`UnitParam::StereoWidth`) or the graph's `set_param`. A cell that moves is sampled at
/// the stream grid (16 frames, [`Kernel::GRID`]) and ramps over
/// [`GAIN_RAMP`](tutti_dsp::GAIN_RAMP), 5 ms, so a write is no step and renders the same
/// whatever the blocks; a value that is not finite reads as the last one that was.
///
/// Memoryless past the ramp: no latency and no tail. Made from the width it starts at
/// ([`StereoWidthConfig`], `StereoWidthNode::from`).
///
/// `F` is the float of the graph it runs in, and of the width's ramp and the mid/side
/// arithmetic.
pub type StereoWidthNode<F> = KernelNode<StereoWidthKernel<F>>;

/// What a [`StereoWidthNode`] is made from: the width it starts at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoWidthConfig {
    controls: WidthControls,
}

impl StereoWidthConfig {
    /// A width node at [`StereoWidth::NATURAL`], the pair as it is.
    #[must_use]
    pub fn new() -> Self {
        Self {
            controls: WidthControls::DEFAULT,
        }
    }

    /// The width it starts at.
    #[must_use]
    pub fn width(self, width: StereoWidth) -> Self {
        Self {
            controls: WidthControls { width },
        }
    }
}

impl Default for StereoWidthConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Float + Lerp> From<StereoWidthConfig> for StereoWidthNode<F> {
    fn from(config: StereoWidthConfig) -> Self {
        Self::with_kernel(
            StereoWidthKernel::<F>::new(),
            StereoWidthSet,
            config.controls,
        )
    }
}

/// The width's control.
#[derive(Clone, Copy, Debug)]
pub struct StereoWidthSet;

control_set! {
    StereoWidthSet(_s): WidthControls = WidthControls::DEFAULT;
    live: (all);
    StereoWidth => StereoWidth = width { whole };

    fn defaults(&self) -> WidthControls {
        WidthControls::DEFAULT
    }
}

/// The width as a [`Kernel`]: a [`WidthBank`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct StereoWidthKernel<F: Float> {
    bank: WidthBank<F>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
}

impl<F: Float + Lerp> StereoWidthKernel<F> {
    /// A bank at a placeholder rate: an unprepared node renders nothing.
    fn new() -> Self {
        Self {
            bank: WidthBank::<F>::new(SampleRate::SR_48K),
            applied: false,
        }
    }
}

impl<F: Float + Lerp> Kernel for StereoWidthKernel<F> {
    type Float = F;
    type Set = StereoWidthSet;

    const GRID: usize = COEFF_INTERVAL;

    fn shape(&self) -> Shape {
        Shape::through(ChannelTopology::STEREO).cost(Cost::Light)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = WidthBank::<F>::new(rate);
        self.applied = false;
    }

    /// Silence, and the next design lands at once, as a fresh node's first does.
    fn reset(&mut self) {
        self.bank.jump_to_target();
        self.applied = false;
    }

    fn retarget(&mut self, controls: &WidthControls) {
        self.bank.retarget(controls);
        if !self.applied {
            self.bank.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let rendered = self.bank.process(block);
        // A design after a render is a move, which ramps.
        self.applied = true;
        rendered
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE, assert_cell_ramps, drive_block, prepared};
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_units::UnitParam;

    // The node tests run `F = f32`, the session's audio format.
    fn node(width: f64) -> StereoWidthNode<f32> {
        prepared(StereoWidthNode::<f32>::from(
            StereoWidthConfig::new().width(StereoWidth::new(width)),
        ))
    }

    /// A fork starts from the width last set through the `ParamSet` and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(StereoWidthNode::<f32>::from(
            StereoWidthConfig::new().width(StereoWidth::new(0.5)),
        ));
    }

    /// The shape is stereo, `Light`, and declares the width param, whose base is the
    /// width built with.
    #[test]
    fn the_shape_declares_the_width_param() {
        let n = StereoWidthNode::<f32>::from(StereoWidthConfig::new().width(StereoWidth::new(1.5)));
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (2, 2)
        );
        assert_eq!(shape.cost, Cost::Light);
        assert_eq!(shape.params.as_slice(), &[UnitParam::StereoWidth][..]);
        assert_eq!(n.param_base(0), Some(1.5));
    }

    /// A node built at width `w` renders `M + w S`: `(1, 0.5)` is `(0.75, 0.75)` at 0,
    /// itself at 1 and `(1.25, 0.25)` at 2, from the first frame.
    #[test]
    fn a_built_width_is_mid_plus_width_times_side() {
        for (w, want) in [(0.0, (0.75, 0.75)), (1.0, (1.0, 0.5)), (2.0, (1.25, 0.25))] {
            let mut n = node(w);
            let out = drive_block(&mut n, &[&[1.0; 64], &[0.5; 64]]);
            assert!(out[0].iter().all(|&y| y == want.0), "left at {w}");
            assert!(out[1].iter().all(|&y| y == want.1), "right at {w}");
        }
    }

    /// A cell written between blocks ramps on the stream clock whatever the blocks, and
    /// lands on the formula: a side-only pair `(1, -1)` reads `w` on the left, `-w` on the
    /// right, and `0.25` after the ramp.
    #[test]
    fn a_written_width_ramps_and_lands_on_the_formula() {
        let make = || StereoWidthNode::<f32>::from(StereoWidthConfig::new());
        let change =
            |n: &StereoWidthNode<f32>| assert!(n.param_set().set(UnitParam::StereoWidth, 0.25));
        let (l, r) = (vec![1.0f32; 64], vec![-1.0f32; 64]);
        let (la, ra) = (vec![1.0f32; 600], vec![-1.0f32; 600]);
        assert_cell_ramps(make, change, &[&l, &r], &[&la, &ra], "width");
        let (_, out) = crate::test_support::render_change(make, change, &[&l, &r], &[&la, &ra], 64);
        assert!(out[0][599] == 0.25 && out[1][599] == -0.25);
        assert!(out[0][0] < 1.0 && out[0][0] > 0.9, "starts down the line");
    }

    /// An `f64` block through a `StereoWidthKernel::<f64>` built at width 1 is the pair
    /// exactly (a 29-bit signal, which `f32` cannot hold), and at width `w` is
    /// `L' = M + w S`, `R' = M - w S` with `M = (L + R) / 2` and `S = (L - R) / 2` in `f64`,
    /// to 2^-45.
    #[test]
    fn an_f64_kernel_is_the_identity_at_unity_and_the_mid_side_matrix_at_other_widths() {
        use crate::test_support::drive_kernel;
        use tutti_fixtures::signal::{assert_close_f64, wide_signal};
        use tutti_units::Samples;
        let len = 100;
        let l = wide_signal(len);
        let r: Vec<f64> = l.iter().rev().map(|v| v * 0.75).collect();
        let run = |w: f64| {
            let mut k = StereoWidthKernel::<f64>::new();
            k.prepare(RATE, MaxBlock::of(Samples::new(len)));
            let controls = WidthControls {
                width: StereoWidth::new(w),
            };
            drive_kernel(&mut k, &[(0, controls)], &[l.clone(), r.clone()])
        };
        let out = run(1.0);
        assert_close_f64(&out[0], &l, 0.0);
        assert_close_f64(&out[1], &r, 0.0);
        let w = 0.3;
        let out = run(w);
        for (n, (a, b)) in out[0].iter().zip(&out[1]).enumerate() {
            let (mid, side) = ((l[n] + r[n]) / 2.0, w * (l[n] - r[n]) / 2.0);
            assert_close_f64(&[*a, *b], &[mid + side, mid - side], 2.0f64.powi(-45));
        }
    }
}
