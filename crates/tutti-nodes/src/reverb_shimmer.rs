//! The shimmer reverb, [`ShimmerNode`]: a feedback delay network whose feedback is shifted in
//! pitch.
//!
//! The network is [`FdnBank`](tutti_dsp::FdnBank)'s, run by an [`FdnKernel`] on a
//! [shimmering](FdnLayout::shimmering) layout; this file holds the controls it adds to the
//! [`FdnSet`](crate::FdnSet)'s and how the graph reaches them.

use tutti_buffers::{InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, FdnControls, FdnLayout, FftFloat, Lerp, MaxDecay};
use tutti_graph::Shape;
use tutti_units::{
    Bipolar, Cents, Db, Feedback, Hz, Mix, SampleRate, Samples, Seconds, StereoWidth, Tail,
};

use crate::kernel::{ControlSignals, Kernel, KernelNode};
use crate::reverb_fdn::FdnKernel;

/// A feedback delay network with a pitch shifter in its feedback: mono in, stereo out, wet only.
///
/// A [`KernelNode`] over [`ShimmerKernel`]. Each trip round the loop, the share of the feedback
/// the **send** sets goes through one phase vocoder ([`PhaseVocoder`](tutti_dsp::PhaseVocoder),
/// fixed-rate) and comes back shifted by the **pitch**, so the tail climbs: at +1200 cents each
/// trip puts what it carried an octave higher. Its blend with its input, for use as an insert,
/// is the graph's mix (`Editor::set_mix`). Made from a [`ShimmerConfig`] (`ShimmerNode::from`).
///
/// # The controls
///
/// The ports are the [`FdnNode`](crate::FdnNode)'s without its freeze (the tail always
/// decays), then the shift (`UnitParam::Pitch`, a [`Cents`]: a move applies at the vocoder's
/// next hop) and the send (`UnitParam::Feedback`, a [`Feedback`]: at `0` the plain network,
/// and a value past [`Feedback::MAX_STABLE`] is held there).
///
/// # Stability
///
/// The loop's gain stays below 1 for any send and any shift: the shifted share of the feedback
/// is a contraction (the vocoder's gain over `l2` is 1 at most, and it comes back divided by
/// a bound a little above), and the rest of the loop is the network's own absorption
/// ([`FdnBank`](tutti_dsp::FdnBank)'s `shimmer` module has the derivation).
///
/// # Latency and tail
///
/// The node reports **no latency**: the vocoder sits inside the loop, so its delay (one chunk
/// of 32 frames and the vocoder's latency, [`FdnBank::shimmer_delay`](tutti_dsp::FdnBank::shimmer_delay))
/// is the first shifted echo's arrival, part of the reverb's onset like its pre-delay, not
/// processing latency to compensate. The model reads that delay as part of every line's loop
/// while the send is above zero, so the declared tail is the plain network's with its
/// trips lengthened by it, and the node narrows it each block as the controls in force ring.
///
/// `F` is the float of the graph it runs in, as the [`FdnNode`](crate::FdnNode)'s.
pub type ShimmerNode<F> = KernelNode<ShimmerKernel<F>>;

/// What a [`ShimmerNode`] is made from: the network's longest decay and the controls its cells
/// start at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShimmerConfig {
    max_decay: MaxDecay,
    controls: FdnControls,
}

impl ShimmerConfig {
    /// Eight lines, RT60s up to [`MaxDecay::DEFAULT`], the cells at [`FdnControls::DEFAULT`]:
    /// an octave up (`1200` cents), at a send of `0.5`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_decay: MaxDecay::DEFAULT,
            controls: FdnControls {
                shimmer_send: Feedback::new_clamped(0.5),
                ..FdnControls::DEFAULT
            },
        }
    }

    /// The longest RT60 a control may ask for.
    #[must_use]
    pub const fn max_decay(mut self, max_decay: MaxDecay) -> Self {
        self.max_decay = max_decay;
        self
    }

    /// The controls the cells start at; their `freeze` is ignored.
    #[must_use]
    pub const fn controls(mut self, controls: FdnControls) -> Self {
        self.controls = controls;
        self
    }

    /// The shift the cell starts at.
    #[must_use]
    pub const fn pitch(mut self, pitch: Cents) -> Self {
        self.controls.shimmer_pitch = pitch;
        self
    }

    /// The share of the feedback the cell starts at sending through the shifter.
    #[must_use]
    pub const fn send(mut self, send: Feedback) -> Self {
        self.controls.shimmer_send = send;
        self
    }
}

impl Default for ShimmerConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: FftFloat + Lerp> From<ShimmerConfig> for ShimmerNode<F> {
    /// Allocates: build it before the node goes live.
    fn from(config: ShimmerConfig) -> Self {
        let layout = FdnLayout::shimmering(8, config.max_decay).expect("eight lines is a layout");
        Self::with_kernel(
            ShimmerKernel {
                fdn: FdnKernel::new(layout),
            },
            ShimmerSet,
            config.controls,
        )
    }
}

/// The shimmer's controls: the [`FdnSet`](crate::FdnSet)'s (the three decays, the size, the
/// diffusion, the pre-delay, the modulation's depth and rate, the width and the wet gain), the
/// shift and the send.
#[derive(Clone, Copy, Debug)]
pub struct ShimmerSet;

control_set! {
    ShimmerSet(_s): FdnControls = FdnControls { freeze: false, ..FdnControls::DEFAULT };
    live: (all);
    Decay[0] => Seconds = decay[0] { whole },
    Decay[1] => Seconds = decay[1] { whole },
    Decay[2] => Seconds = decay[2] { whole },
    RoomSize => Mix = size { whole },
    Diffusion => Mix = diffusion { whole },
    DelayTime => Seconds = pre_delay { whole },
    Depth => Bipolar = modulation { whole },
    Rate => Hz = rate { whole },
    StereoSpread => StereoWidth = width { whole },
    GainDb => Db = wet { whole, no_ring },
    Pitch => Cents = shimmer_pitch { whole, no_ring },
    Feedback => Feedback = shimmer_send { whole };

    fn defaults(&self) -> FdnControls {
        FdnControls::DEFAULT
    }
}

/// The shimmer as a [`Kernel`]: an [`FdnKernel`] on a shimmering layout, with the shimmer's
/// controls.
#[derive(Clone, Debug)]
pub struct ShimmerKernel<F: FftFloat> {
    fdn: FdnKernel<F>,
}

impl<F: FftFloat + Lerp> Kernel for ShimmerKernel<F> {
    type Float = F;
    type Set = ShimmerSet;

    const GRID: usize = <FdnKernel<F> as Kernel>::GRID;

    fn shape(&self) -> Shape {
        self.fdn.shape()
    }

    fn prepare(&mut self, rate: SampleRate, max_block: MaxBlock) {
        self.fdn.prepare(rate, max_block);
    }

    fn reset(&mut self) {
        self.fdn.reset();
    }

    fn retarget(&mut self, controls: &FdnControls) {
        self.fdn.retarget(controls);
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        signals: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.fdn.render(block, signals)
    }

    /// None: the shifter's delay is the reverb's onset (see [`ShimmerNode`]).
    fn latency(&self) -> Samples {
        Samples::ZERO
    }

    fn tail(&self) -> Tail {
        self.fdn.tail()
    }

    fn tail_of(&mut self, controls: &FdnControls) -> Tail {
        self.fdn.tail_of(controls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::non_ringing;
    use tutti_units::{UnitAddr, UnitParam};

    /// The shift and the wet gain are the controls the tail does not read.
    #[test]
    fn only_the_shift_and_the_wet_gain_are_flagged_no_ring() {
        assert_eq!(
            non_ringing(&ShimmerSet),
            [
                UnitAddr::first(UnitParam::GainDb),
                UnitAddr::first(UnitParam::Pitch)
            ]
        );
    }
}
