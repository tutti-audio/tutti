//! [`StereoWidthNode`]: the side of a stereo pair scaled by a width.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{COEFF_INTERVAL, WidthBank, WidthControls};
use tutti_graph::{Cost, Shape};
use tutti_types::{BusTopology, MaxBlock, SampleRate, StereoWidth, Tail};

use crate::kernel::{ControlLanes, Kernel, KernelNode};

/// Stereo width: left/right in, left/right out, `L = M + w S` and `R = M - w S` with the
/// mid `M = (L + R) / 2` and the side `S = (L - R) / 2` of the input.
///
/// Width 0 is the mono sum (`L = R = M`), 1 the pair as it is and 2 doubles the side. The
/// width is a live [`StereoWidth`] cell, read from `0` up with no upper bound (the unit
/// holds `0..`), set through the node's [`ParamSet`](tutti_graph::ParamSet)
/// (`UnitParam::StereoWidth`) or the graph's `set_param`. A cell that moves is sampled at
/// the stream grid (16 frames, [`Kernel::GRID`]) and glides over
/// [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE), 5 ms, so a write is no step and renders the same
/// whatever the blocks; a value that is not finite reads as the last one that was.
///
/// Memoryless past the glide: no latency and no tail. Made from the width it starts at
/// ([`StereoWidthConfig`], `StereoWidthNode::from`).
pub type StereoWidthNode = KernelNode<StereoWidthKernel>;

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

impl From<StereoWidthConfig> for StereoWidthNode {
    fn from(config: StereoWidthConfig) -> Self {
        Self::with_kernel(StereoWidthKernel::new(), StereoWidthSet, config.controls)
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
pub struct StereoWidthKernel {
    bank: WidthBank,
    /// Whether the first retarget after `prepare` has landed.
    landed: bool,
}

impl StereoWidthKernel {
    /// A bank at a placeholder rate: an unprepared node renders nothing.
    fn new() -> Self {
        Self {
            bank: WidthBank::new(SampleRate::SR_48K),
            landed: false,
        }
    }
}

impl Kernel for StereoWidthKernel {
    type Set = StereoWidthSet;

    const GRID: usize = COEFF_INTERVAL;

    fn shape(&self) -> Shape {
        Shape::through(BusTopology::STEREO).with_cost(Cost::Light)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = WidthBank::new(rate);
        self.landed = false;
    }

    /// Silence, and the next design lands at once, as a fresh node's first does.
    fn reset(&mut self) {
        self.bank.settle();
        self.landed = false;
    }

    fn retarget(&mut self, controls: &WidthControls) {
        self.bank.aim(controls);
        if !self.landed {
            self.bank.settle();
            self.landed = true;
        }
    }

    fn render<S: Float>(
        &mut self,
        input: Planes<'_, S>,
        _: ControlLanes<'_, S>,
        output: PlanesMut<'_, '_, S>,
    ) {
        self.bank.process(input, output);
        // A design after a render is a move, which glides.
        self.landed = true;
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{assert_cell_glides, drive_block, prepared};
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_types::UnitParam;

    fn node(width: f32) -> StereoWidthNode {
        prepared(StereoWidthNode::from(
            StereoWidthConfig::new().width(StereoWidth(width)),
        ))
    }

    /// A fork starts from the width last set through the `ParamSet` and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(StereoWidthNode::from(
            StereoWidthConfig::new().width(StereoWidth(0.5)),
        ));
    }

    /// The shape is stereo, `Light`, and declares the width param, whose base is the
    /// width built with.
    #[test]
    fn the_shape_declares_the_width_param() {
        let n = StereoWidthNode::from(StereoWidthConfig::new().width(StereoWidth(1.5)));
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

    /// A cell written between blocks glides on the stream clock whatever the blocks, and
    /// lands on the formula: a side-only pair `(1, -1)` reads `w` on the left, `-w` on the
    /// right, and `0.25` after the glide.
    #[test]
    fn a_written_width_glides_and_lands_on_the_formula() {
        let make = || StereoWidthNode::from(StereoWidthConfig::new());
        let change = |n: &StereoWidthNode| assert!(n.param_set().set(UnitParam::StereoWidth, 0.25));
        let (l, r) = (vec![1.0f32; 64], vec![-1.0f32; 64]);
        let (la, ra) = (vec![1.0f32; 600], vec![-1.0f32; 600]);
        assert_cell_glides(make, change, &[&l, &r], &[&la, &ra], "width");
        let (_, out) = crate::test_support::render_change(make, change, &[&l, &r], &[&la, &ra], 64);
        assert!(out[0][599] == 0.25 && out[1][599] == -0.25);
        assert!(out[0][0] < 1.0 && out[0][0] > 0.9, "starts down the line");
    }
}
