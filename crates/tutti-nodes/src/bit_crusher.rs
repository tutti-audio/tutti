//! [`BitCrusherNode`]: a sample-and-hold and a quantizer.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{BitCrusher, BitCrusherControls, BlockMismatch, COEFF_INTERVAL};
use tutti_graph::{Cost, Shape};
use tutti_units::{BitDepth, HoldFactor, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`BitCrusherNode`] is made from: its width and, optionally, its bit depth and hold.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BitCrusherConfig {
    width: ChannelLayout,
    controls: BitCrusherControls,
}

impl BitCrusherConfig {
    /// A crusher of `width` channels at 8 bits and no hold.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>) -> Self {
        Self {
            width: width.into(),
            controls: BitCrusherControls::DEFAULT,
        }
    }

    /// The word length it starts at, read within [`BitDepth::MIN`] and [`BitDepth::MAX`].
    #[must_use]
    pub fn bits(self, bits: BitDepth) -> Self {
        Self {
            controls: BitCrusherControls {
                bits,
                ..self.controls
            },
            ..self
        }
    }

    /// The frames per held sample it starts at, read rounded to a whole number.
    #[must_use]
    pub fn hold(self, hold: HoldFactor) -> Self {
        Self {
            controls: BitCrusherControls {
                hold,
                ..self.controls
            },
            ..self
        }
    }
}

/// A bit crusher of any width: every `N` frames the input is held, and the held value `x`
/// is quantized to `clamp(round(x q) / q, -1, 1)` with `q = 2^(b-1)` for `b` bits, on every
/// channel alone and every channel on the same frames.
///
/// The hold comes first, then the quantizer. The rounding is half to even and a fractional
/// depth is a real `q`, so its levels are not all whole steps ([`tutti_dsp::quantize`]). `N` is the hold factor
/// rounded to the nearest whole number, ties to even ([`tutti_dsp::hold_frames`]), and its
/// count of frames is continuous across blocks.
///
/// Both controls are live and land whole, as a step is the effect: the bit depth is a
/// [`BitDepth`] (`UnitParam::BitDepth`, 8 by default) and the hold a [`HoldFactor`]
/// (`UnitParam::HoldFactor`, 1 by default), each sampled at the stream grid (16 frames). A
/// cell written with a value that is not finite reads as the last one that was.
///
/// No tail and no latency. A [`KernelNode`], so a non-finite sample is heard in at most one
/// block: the node resets the held values after it. Made from a [`BitCrusherConfig`]
/// (`BitCrusherNode::from`).
///
/// # Panics
///
/// `BitCrusherNode::from` panics past 16 channels, when the kernel's shape is built: a
/// crusher is one bus.
///
/// `F` is the float of the graph it runs in, and of the held values and the quantizer.
pub type BitCrusherNode<F> = KernelNode<BitCrusherKernel<F>>;

impl<F: Float> From<BitCrusherConfig> for BitCrusherNode<F> {
    fn from(config: BitCrusherConfig) -> Self {
        Self::with_kernel(
            BitCrusherKernel::<F>::new(config.width, config.controls),
            BitCrusherSet,
            config.controls,
        )
    }
}

/// The crusher's controls: the bit depth and the hold.
#[derive(Clone, Copy, Debug)]
pub struct BitCrusherSet;

control_set! {
    BitCrusherSet(_s): BitCrusherControls = BitCrusherControls::DEFAULT;
    live: (all);
    BitDepth => BitDepth = bits { whole },
    HoldFactor => HoldFactor = hold { whole };

    fn defaults(&self) -> BitCrusherControls {
        BitCrusherControls::DEFAULT
    }
}

/// The crusher as a [`Kernel`]: a [`BitCrusher`].
#[derive(Clone, Debug)]
pub struct BitCrusherKernel<F: Float> {
    width: ChannelLayout,
    crusher: BitCrusher<F>,
}

impl<F: Float> BitCrusherKernel<F> {
    fn new(width: ChannelLayout, controls: BitCrusherControls) -> Self {
        Self {
            width,
            crusher: BitCrusher::<F>::new(width, controls),
        }
    }
}

impl<F: Float> Kernel for BitCrusherKernel<F> {
    type Float = F;
    type Set = BitCrusherSet;

    const GRID: usize = COEFF_INTERVAL;

    /// `width` channels in and out, `Light`: a round and a divide per sample.
    fn shape(&self) -> Shape {
        let bus = ChannelTopology::unassigned(self.width)
            .expect("a bit crusher is at most 16 channels wide");
        Shape::through(bus).cost(Cost::Light)
    }

    fn prepare(&mut self, _: SampleRate, _: MaxBlock) {
        self.crusher.reset();
    }

    /// Silence: the next frame captures.
    fn reset(&mut self) {
        self.crusher.reset();
    }

    fn retarget(&mut self, controls: &BitCrusherControls) {
        self.crusher.retarget(controls);
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.crusher.process(block)
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE, drive_block, prepared};
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_units::{Latency, UnitParam};

    // The node tests run `F = f32`, the session's audio format.
    fn node(width: u16, bits: f64, hold: f64) -> BitCrusherNode<f32> {
        prepared(BitCrusherNode::<f32>::from(
            BitCrusherConfig::new(width)
                .bits(BitDepth::new(bits))
                .hold(HoldFactor::new(hold)),
        ))
    }

    /// A fork starts from the controls last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(BitCrusherNode::<f32>::from(BitCrusherConfig::new(2u16)));
    }

    /// The shape is the width in and out, `Light`, no tail, declares the depth and the hold
    /// as `BitDepth` and `HoldFactor`; the defaults are 8 bits and a hold of 1.
    #[test]
    fn the_shape_declares_the_two_controls() {
        let n = node(3, 8.0, 1.0);
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.cost, Cost::Light);
        assert_eq!(shape.tail, Tail::None);
        assert_eq!(shape.latency, Latency::ZERO);
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::BitDepth, UnitParam::HoldFactor][..]
        );
        let d = BitCrusherNode::<f32>::from(BitCrusherConfig::new(1u16));
        assert_eq!((d.param_base(0), d.param_base(1)), (Some(8.0), Some(1.0)));
    }

    /// An empty layout is no channels, not one.
    #[test]
    fn an_empty_layout_has_no_channels() {
        let n = BitCrusherNode::<f32>::from(BitCrusherConfig::new(0u16));
        assert_eq!(n.shape().audio_in().count(), 0);
    }

    /// Both controls are settable by their kinds and land whole: at 4 bits (`q = 8`) with a
    /// hold of 2, the frames are `round(x q) / q` of every other input frame.
    #[test]
    fn the_controls_are_settable_by_their_kinds() {
        let mut n = node(1, 8.0, 1.0);
        assert!(n.param_set().set(UnitParam::BitDepth, 4.0));
        assert!(n.param_set().set(UnitParam::HoldFactor, 2.0));
        let out = drive_block(&mut n, &[&[0.3, 0.9, 0.5, 0.1]]);
        assert_eq!(out[0], [0.25, 0.25, 0.5, 0.5]);
    }

    /// An `f64` block through a `BitCrusherKernel::<f64>` at 32 bits (`q = 2^31`) holding 2
    /// frames is the input's frame `2 floor(k / 2)` exactly: a 29-bit signal, which `f32`
    /// cannot hold, passes `round(x q) / q`.
    #[test]
    fn an_f64_kernel_at_thirty_two_bits_holds_a_twenty_nine_bit_signal_exactly() {
        use crate::test_support::drive_kernel;
        use tutti_fixtures::signal::{assert_close_f64, wide_signal};
        let len = 100;
        let x = wide_signal(len);
        let want: Vec<f64> = (0..len).map(|k| x[2 * (k / 2)]).collect();
        let controls = BitCrusherControls {
            bits: BitDepth::MAX,
            hold: HoldFactor::new(2.0),
        };
        let mut k = BitCrusherKernel::<f64>::new(ChannelLayout::MONO, controls);
        k.prepare(RATE, MaxBlock::of(tutti_units::Samples::new(len)));
        let y = drive_kernel(&mut k, &[(0, controls)], &[x]).remove(0);
        assert_close_f64(&y, &want, 0.0);
    }
}
