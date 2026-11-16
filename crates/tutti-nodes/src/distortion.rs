//! The waveshaper [`DistortionNode`]: six memoryless curves ([`ShapeKind`]) at a drive.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{COEFF_INTERVAL, ShapeKind, WaveshaperBank, WaveshaperControls};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_types::{BusTopology, ChannelLayout, Drive, MaxBlock, SampleRate, Tail};

use crate::kernel::{ControlLanes, Kernel, KernelNode};

/// Waveshaping distortion of any width: `N` audio inputs and `N` outputs, every channel
/// shaped alone by the same curve at the same drive.
///
/// A [`KernelNode`] over [`DistortionKernel`], the curves and the drive's glide being
/// [`WaveshaperBank`]'s; this file holds the cell and how the graph reaches it. The curve
/// ([`ShapeKind`]) is fixed at construction: a node with another one is another node.
///
/// # Drive
///
/// The drive is input gain into the curve, floored at 0 ([`ShapeKind::apply`] says what the
/// two staircases make of it). It is a live [`Params`](crate::Params) cell shared across
/// clones, set through the node's [`ParamSet`](tutti_graph::ParamSet) or the graph's
/// `set_param` (`UnitParam::Drive`); a value that is not finite reads as the last one that
/// was.
///
/// **It is read per frame** when the graph drives it: a lane on its port is the bank's own,
/// and a frame of it that is not finite reads the cell's drive. A cell that moves is sampled
/// at the stream grid (16 frames from the last reset, [`Kernel::GRID`]) and glides over
/// [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE), 5 ms, so a write is no step at a block's edge and
/// renders the same whatever the blocks.
///
/// The node declares no tail: the curves are memoryless, so a silent input is a silent
/// output.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, a fork of it starts from the
/// value last set through its [`ParamSet`](tutti_graph::ParamSet). The graph prepares it at
/// the device rate before its first block.
pub type DistortionNode = KernelNode<DistortionKernel>;

impl DistortionNode {
    /// A stereo waveshaper of `kind` at `drive`.
    ///
    /// [`Drive`] is input gain into the shaping curve, floored at 0: higher
    /// pushes further into the nonlinearity and distorts harder.
    pub fn new(kind: ShapeKind, drive: impl Into<Drive>) -> Self {
        Self::with_channels(ChannelLayout::STEREO, kind, drive)
    }

    /// An `n`-channel waveshaper: every channel is shaped by the same (linked) drive and
    /// kind. An empty layout is taken as mono, as the other nodes do.
    ///
    /// # Panics
    ///
    /// Past 16 channels: a waveshaper is one bus.
    pub fn with_channels(
        channels: impl Into<ChannelLayout>,
        kind: ShapeKind,
        drive: impl Into<Drive>,
    ) -> Self {
        let width = ChannelLayout::from_count(channels.into().count().max(1));
        let input =
            BusTopology::unassigned(width).expect("a waveshaper is at most 16 channels wide");
        Self::with_kernel(
            DistortionKernel::new(kind, input),
            DistortionSet,
            WaveshaperControls {
                drive: drive.into(),
            },
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
    /// The lane of the drive: the one [`fast`](crate::ControlRead::Fast) row.
    const DRIVE_LANE: usize = 0;
}

/// The waveshaper as a [`Kernel`]: a [`WaveshaperBank`] of one curve, at the prepared rate.
#[derive(Clone, Debug)]
pub struct DistortionKernel {
    kind: ShapeKind,
    input: BusTopology,
    bank: WaveshaperBank,
    /// Whether the first retarget after `prepare` has landed.
    landed: bool,
}

impl DistortionKernel {
    /// A waveshaper of `kind` on the bus `input`. Its rate until `prepare` is a placeholder:
    /// an unprepared node renders nothing.
    fn new(kind: ShapeKind, input: BusTopology) -> Self {
        Self {
            kind,
            input,
            bank: WaveshaperBank::new(kind, SampleRate::SR_48K),
            landed: false,
        }
    }
}

impl Kernel for DistortionKernel {
    type Set = DistortionSet;

    /// A cell that moves reaches the render within 16 frames, wherever the blocks fall.
    const GRID: usize = COEFF_INTERVAL;

    /// The input bus and one output bus where it is. The cost is the curve's: `Softsign` and
    /// `HardClip` are arithmetic, `Light`; `Crush` and `SoftCrush` (a rounding call) and
    /// `Atan` and `Tanh` (a libm call) are `Moderate`.
    fn shape(&self) -> Shape {
        let output = Bus::banded(self.input, BusRole::Main, 1).expect("one band is a bus");
        let cost = match self.kind {
            ShapeKind::Softsign | ShapeKind::HardClip => Cost::Light,
            _ => Cost::Moderate,
        };
        Shape::buses(
            Buses::main(self.input),
            Buses::NONE.with(output).expect("one bus"),
        )
        .with_cost(cost)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = WaveshaperBank::new(self.kind, rate);
        self.landed = false;
    }

    fn reset(&mut self) {
        self.bank.settle();
    }

    fn retarget(&mut self, controls: &WaveshaperControls) {
        self.bank.aim(controls);
        if !self.landed {
            self.bank.settle();
            self.landed = true;
        }
    }

    fn render<S: Float>(
        &mut self,
        input: Planes<'_, S>,
        lanes: ControlLanes<'_, S>,
        output: PlanesMut<'_, '_, S>,
    ) {
        self.bank
            .process(lanes.get(DistortionSet::DRIVE_LANE), input, output);
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE, assert_cell_glides, drive_block, prepared, render_change};
    use tutti_graph::contract::{assert_param_fork, drive};
    use tutti_graph::{Node, ParamNode};
    use tutti_types::UnitParam;

    const KINDS: [ShapeKind; 6] = [
        ShapeKind::Tanh,
        ShapeKind::Atan,
        ShapeKind::Softsign,
        ShapeKind::HardClip,
        ShapeKind::Crush,
        ShapeKind::SoftCrush,
    ];

    fn hard_clip(channels: u16, drive: f32) -> DistortionNode {
        prepared(DistortionNode::with_channels(
            channels,
            ShapeKind::HardClip,
            Drive::new(drive),
        ))
    }

    /// A fork starts from the drive last set through the node's `ParamSet` and shares no
    /// cell with it.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(DistortionNode::new(ShapeKind::Tanh, Drive::new(2.0)));
    }

    /// The node is stereo unless told otherwise, and the shape declares the drive.
    #[test]
    fn the_shape_is_stereo_and_declares_its_drive_param() {
        let n = DistortionNode::new(ShapeKind::Tanh, Drive::new(1.5));
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

    /// An empty layout is taken as mono.
    #[test]
    fn an_empty_layout_is_mono() {
        let n = DistortionNode::with_channels(0u16, ShapeKind::Tanh, Drive::new(1.0));
        assert_eq!(n.shape().audio_in().count(), 1);
    }

    /// Drive is reachable by address through the node's `ParamSet`, and an address it does
    /// not own is refused.
    #[test]
    fn drive_is_settable_by_unit_param() {
        let n = DistortionNode::new(ShapeKind::Tanh, Drive::new(1.0));
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
        let out = drive(&mut n, RATE, &ins, &[Some(&step)]);
        for (c, o) in out.iter().enumerate() {
            assert_eq!(o[31], 0.25, "channel {c} before the step");
            assert_eq!(o[32], 0.75, "channel {c} on the step's frame");
        }
        let out = drive(&mut n, RATE, &ins, &[None]);
        assert!(
            out.iter().all(|o| o.iter().all(|&y| y == 0.25)),
            "the cell again"
        );
    }

    /// A cell written between blocks glides on the stream clock, whatever the blocks, and the
    /// glide is the line from the old drive to the new over `GAIN_GLIDE`: frame `i` of a hard
    /// clip of `0.2` is `0.2 (1 + 2 (i + 1) / n)`, and the frames after it are `0.6`.
    #[test]
    fn a_drive_written_between_blocks_glides_over_the_glide() {
        let make = || DistortionNode::with_channels(1u16, ShapeKind::HardClip, Drive::new(1.0));
        let change = |n: &DistortionNode| {
            assert!(n.param_set().set(UnitParam::Drive, 3.0));
        };
        let before = vec![0.2f32; 64];
        let after = vec![0.2f32; 600];
        assert_cell_glides(make, change, &[&before], &[&after], "drive");
        let (_, out) = render_change(make, change, &[&before], &[&after], 64);
        let n = tutti_dsp::GAIN_GLIDE.to_samples_ceil(RATE).get();
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
            assert!(n.param_set().set(UnitParam::Drive, bad));
            let out = drive_block(&mut n, &[&x]);
            assert!(out[0].iter().all(|&y| y == 0.5), "written {bad}");
            let fed = vec![bad; 64];
            let out = drive(&mut n, RATE, &[&x], &[Some(&fed)]);
            assert!(out[0].iter().all(|&y| y == 0.5), "fed {bad}");
        }
        let mut built = hard_clip(1, f32::NAN);
        let out = drive_block(&mut built, &[&x]);
        assert!(out[0].iter().all(|&y| y == 0.25), "built with NaN");
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
        assert!(mid[0][9] < 0.2 * 3.0, "still gliding");
        Node::reset(&mut n);
        let out = drive_block(&mut n, &[&x]);
        assert!(out[0].iter().all(|&y| y == 0.2 * 3.0));
    }

    /// The declared cost follows the curve: the two arithmetic-only curves are `Light`, the
    /// four that call a math routine `Moderate`.
    #[test]
    fn the_declared_cost_follows_the_curve() {
        for kind in KINDS {
            let want = match kind {
                ShapeKind::Softsign | ShapeKind::HardClip => Cost::Light,
                _ => Cost::Moderate,
            };
            assert_eq!(
                DistortionNode::new(kind, Drive::new(1.0)).shape().cost,
                want,
                "{kind:?}"
            );
        }
    }
}
