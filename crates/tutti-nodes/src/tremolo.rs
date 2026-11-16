//! [`TremoloNode`]: an LFO scales the level.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{COEFF_INTERVAL, OscillatorWave, Tremolo, TremoloControls};
use tutti_graph::{Cost, Shape};
use tutti_types::{BusTopology, ChannelLayout, Depth, Hz, MaxBlock, SampleRate, Tail};

use crate::kernel::{ControlLanes, Kernel, KernelNode};

/// What a [`TremoloNode`] is made from: its width, the wave of its LFO and where its
/// controls start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TremoloConfig {
    width: ChannelLayout,
    wave: OscillatorWave,
    controls: TremoloControls,
}

impl TremoloConfig {
    /// A tremolo of `width` channels (an empty layout is taken as mono) on a sine LFO, at [`TremoloControls::DEFAULT`] (5 Hz,
    /// half depth).
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: ChannelLayout::from_count(width.into().count().max(1)),
            wave: OscillatorWave::Sine,
            controls: TremoloControls::DEFAULT,
        }
    }

    /// The wave of the LFO.
    #[must_use]
    pub fn wave(self, wave: OscillatorWave) -> Self {
        Self { wave, ..self }
    }

    /// The rate it starts at.
    #[must_use]
    pub fn rate(mut self, rate: Hz) -> Self {
        self.controls.rate = rate;
        self
    }

    /// The depth it starts at.
    #[must_use]
    pub fn depth(mut self, depth: Depth) -> Self {
        self.controls.depth = depth;
        self
    }
}

/// A tremolo of any width: every channel scaled by `g = 1 - (|d| - d u) / 2` for the depth
/// `d` and the LFO `u` in `-1..=1` ([`Tremolo`]).
///
/// Depth 0 is unity, and depth 1 swings the gain from 1 at the wave's peak down to silence
/// at its trough; a negative depth swings it the other way. The LFO starts at phase 0 and
/// its phase runs on across blocks and rate changes. The rate (`UnitParam::Rate`, a [`Hz`])
/// and the depth (`UnitParam::Depth`, a [`Depth`]) are live cells; a depth that moves is
/// sampled at the stream grid (16 frames) and glides over
/// [`GAIN_GLIDE`](tutti_dsp::GAIN_GLIDE), a rate lands whole, and a value that is not finite
/// reads as the last one that was.
///
/// No latency and no tail. A [`KernelNode`]. Made from a [`TremoloConfig`]
/// (`TremoloNode::from`).
///
/// # Panics
///
/// `TremoloNode::from` panics past 16 channels, where the node's shape is built: a tremolo is one bus.
pub type TremoloNode = KernelNode<TremoloKernel>;

impl From<TremoloConfig> for TremoloNode {
    fn from(config: TremoloConfig) -> Self {
        Self::with_kernel(
            TremoloKernel::new(config.width, config.wave),
            TremoloSet,
            config.controls,
        )
    }
}

/// The tremolo's controls: the rate and the depth.
#[derive(Clone, Copy, Debug)]
pub struct TremoloSet;

control_set! {
    TremoloSet(_s): TremoloControls = TremoloControls::DEFAULT;
    live: (all);
    Rate => Hz = rate { whole },
    Depth => Depth = depth;

    fn defaults(&self) -> TremoloControls {
        TremoloControls::DEFAULT
    }
}

/// The tremolo as a [`Kernel`]: a [`Tremolo`] at the prepared rate.
#[derive(Clone, Debug)]
pub struct TremoloKernel {
    width: ChannelLayout,
    wave: OscillatorWave,
    tremolo: Tremolo,
}

impl TremoloKernel {
    /// A tremolo at a placeholder rate: an unprepared node renders nothing.
    fn new(width: ChannelLayout, wave: OscillatorWave) -> Self {
        Self {
            width,
            wave,
            tremolo: Tremolo::new(wave, SampleRate::SR_48K),
        }
    }
}

impl Kernel for TremoloKernel {
    type Set = TremoloSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Moderate`: a sine per sample and channel.
    fn shape(&self) -> Shape {
        let bus =
            BusTopology::unassigned(self.width).expect("a tremolo is at most 16 channels wide");
        Shape::through(bus).with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.tremolo = Tremolo::new(self.wave, rate);
    }

    fn reset(&mut self) {
        self.tremolo.reset();
    }

    fn retarget(&mut self, controls: &TremoloControls) {
        self.tremolo.aim(controls);
    }

    fn render<S: Float>(
        &mut self,
        input: Planes<'_, S>,
        _: ControlLanes<'_, S>,
        output: PlanesMut<'_, '_, S>,
    ) {
        self.tremolo.process(input, output);
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{drive_block, prepared};
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_types::UnitParam;

    fn node(width: u16, rate: f32, depth: f32) -> TremoloNode {
        prepared(TremoloNode::from(
            TremoloConfig::new(width)
                .rate(Hz(rate))
                .depth(Depth::of(depth)),
        ))
    }

    /// A fork starts from the controls last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(TremoloNode::from(TremoloConfig::new(2u16)));
    }

    /// The shape is the width in and out, `Moderate`, with no tail, and declares the rate and
    /// the depth; the default is 5 Hz at half depth.
    #[test]
    fn the_shape_declares_rate_and_depth_and_no_tail() {
        let n = node(3, 5.0, 0.5);
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.cost, Cost::Moderate);
        assert_eq!(shape.tail, Tail::None);
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Rate, UnitParam::Depth][..]
        );
        let d = TremoloNode::from(TremoloConfig::new(1u16));
        assert_eq!((d.param_base(0), d.param_base(1)), (Some(5.0), Some(0.5)));
    }

    /// Set to depth 1 through `Depth` the node swings to silence at the trough of a 100 Hz
    /// sine (frame 360 of 480), in blocks of 64 frames.
    #[test]
    fn the_depth_is_settable_and_the_trough_is_silent() {
        let mut n = node(1, 100.0, 0.0);
        assert!(n.param_set().set(UnitParam::Depth, 1.0));
        let ones = [1.0f32; 64];
        let mut out = Vec::new();
        for _ in 0..8 {
            out.extend(drive_block(&mut n, &[&ones]).remove(0));
        }
        assert!(out[360].abs() < 1e-4, "{}", out[360]);
    }

    /// An empty layout is taken as mono.
    #[test]
    fn an_empty_layout_is_mono() {
        let n = TremoloNode::from(TremoloConfig::new(ChannelLayout::EMPTY));
        assert_eq!(n.shape().audio_in().count(), 1);
    }
}
