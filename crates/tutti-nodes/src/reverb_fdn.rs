//! The feedback delay network, [`FdnNode`].
//!
//! [`FdnBank`] is the network over buffers views (the lines, the absorption, the mix, the
//! ramps); this file holds the kernel that retargets it, the cells and how the graph reaches
//! them.

use tutti_buffers::{ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, FdnBank, FdnControls, FdnKind, FdnLayout, FdnRamps, FftFloat, FreezeControls,
    Lerp, MaxDecay,
};
use tutti_graph::{Buses, Cost, MAX_NODE_TAIL, Shape};
use tutti_units::{Bipolar, Db, Hz, Mix, SampleRate, Seconds, StereoWidth, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// The feedback delay network as a graph node: mono in, stereo out, wet only.
///
/// A [`KernelNode`] over [`FdnKernel`] of a plain [`FdnLayout`], the network being [`FdnBank`]'s. Its blend with its
/// input, for use as an insert, is the graph's mix (`Editor::set_mix`); the node declares no
/// latency (its pre-delay is part of the wet sound). Made from an [`FdnConfig`]
/// (`FdnNode::from`).
///
/// # The controls
///
/// The ports are the three decays (`Decay[0]` the mid band, `[1]` the low, `[2]` the high),
/// the room size (a move lands over seconds by design: half a frame per 32-frame chunk,
/// about 6 s for the longest line going from 0 to 1, to avoid pitch artefacts), the
/// diffusion, the pre-delay, the modulation's depth and rate, the width, the wet gain and
/// the freeze: a cell at or above 0.5. Each lands whole on the
/// 32-frame grid and ramps in the bank.
///
/// # The tail
///
/// The declared tail is the ceiling, unbounded since the freeze can hold the tail. The node
/// narrows it each block to what the controls in force ring, and once the input has been
/// silent for that long it cuts the room off and parks ([`KernelNode`]). A signal on a decay
/// reports the figure at the longest decay the layout holds, the other controls as their cells
/// are; a signal on the freeze reports the ceiling; one on a control that states no longest
/// value (the size, the diffusion, the pre-delay, the modulation, the rate and the width)
/// reports the ceiling.
///
/// `F` is the float of the graph it runs in, and of the lines, the pre-delay and the ramps;
/// the lines are the layout's worst case (its most lines at the longest length the size and the
/// modulation reach), so an `f32` graph halves the memory of an `f64` one.
pub type FdnNode<F> = KernelNode<FdnKernel<F, FdnLayout>>;

/// What an [`FdnNode`] is made from: the network's layout and the controls its cells start at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FdnConfig {
    layout: FdnLayout,
    controls: FdnNodeControls<FdnLayout>,
}

impl FdnConfig {
    /// A network of [`FdnLayout::default`] with its cells at [`FdnNodeControls::DEFAULT`]. It has
    /// no rate until the graph's `prepare` gives it one: it declares no latency and an unknown
    /// tail before, and renders silence.
    #[must_use]
    pub fn new() -> Self {
        Self {
            layout: FdnLayout::default(),
            controls: FdnNodeControls::<FdnLayout>::DEFAULT,
        }
    }

    /// The network's lines and their longest decay.
    #[must_use]
    pub const fn layout(mut self, layout: FdnLayout) -> Self {
        self.layout = layout;
        self
    }

    /// The controls the cells start at.
    #[must_use]
    pub const fn controls(mut self, controls: FdnNodeControls<FdnLayout>) -> Self {
        self.controls = controls;
        self
    }
}

impl Default for FdnConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: FftFloat + Lerp> From<FdnConfig> for FdnNode<F> {
    /// Allocates: build it before the node goes live.
    fn from(config: FdnConfig) -> Self {
        Self::with_kernel(FdnKernel::new(config.layout), FdnSet, config.controls)
    }
}

/// The controls of an FDN node of kind `L`: the network's, which every kind has, and the
/// kind's own ([`FreezeControls`] for a plain network, [`ShimmerControls`](tutti_dsp::ShimmerControls)
/// for a shimmer).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FdnNodeControls<L: FdnKind> {
    /// The decays, the size, the diffusion, the pre-delay, the modulation's depth and rate,
    /// the width and the wet gain.
    pub network: FdnControls,
    /// What the kind adds.
    pub kind: L::Controls,
}

impl FdnNodeControls<FdnLayout> {
    /// [`FdnControls::DEFAULT`], not frozen.
    pub const DEFAULT: Self = Self {
        network: FdnControls::DEFAULT,
        kind: FreezeControls::DEFAULT,
    };
}

/// The FDN's controls: the three decays, the size, the diffusion, the pre-delay, the
/// modulation's depth and rate, the width, the wet gain and the freeze.
#[derive(Clone, Copy, Debug)]
pub struct FdnSet;

// In port order after the mix. Every row
// lands whole: the bank's ramps are the ramp.
control_set! {
    FdnSet(_s): FdnNodeControls<FdnLayout> = FdnNodeControls::<FdnLayout>::DEFAULT;
    live: (all);
    Decay[0] => Seconds = network.decay[0] { whole, longest = MaxDecay::CAP.get() },
    Decay[1] => Seconds = network.decay[1] { whole, longest = MaxDecay::CAP.get() },
    Decay[2] => Seconds = network.decay[2] { whole, longest = MaxDecay::CAP.get() },
    RoomSize => Mix = network.size { whole },
    Diffusion => Mix = network.diffusion { whole },
    DelayTime => Seconds = network.pre_delay { whole },
    Depth => Bipolar = network.modulation { whole },
    Rate => Hz = network.rate { whole },
    StereoSpread => StereoWidth = network.width { whole },
    GainDb => Db = network.wet { whole, no_ring },
    Freeze => bool = kind.freeze { whole, longest = 1.0 };

    fn defaults(&self) -> FdnNodeControls<FdnLayout> {
        FdnNodeControls::<FdnLayout>::DEFAULT
    }
}

/// A [`FdnBank`] and its ramps, once the rate is known.
#[derive(Clone, Debug)]
struct Running<F: FftFloat + Lerp, L: FdnKind> {
    /// The rate the bank was sized at: what a tail bound is converted at.
    rate: SampleRate,
    bank: FdnBank<F, L>,
    ramps: FdnRamps<F, L>,
}

/// The feedback delay network as a [`Kernel`]: a [`FdnBank`] sized at the prepared rate and
/// the ramps its design is set through, of the kind `L`: [`FdnLayout`] for the plain network
/// ([`FdnSet`]) or [`ShimmerLayout`](tutti_dsp::ShimmerLayout) for the shimmer
/// ([`ShimmerSet`](crate::ShimmerSet)).
///
/// The bank's state is `F`, the float of the graph.
#[derive(Clone, Debug)]
pub struct FdnKernel<F: FftFloat + Lerp, L: FdnKind> {
    layout: L,
    /// `None` until [`prepare`](Kernel::prepare).
    running: Option<Running<F, L>>,
}

impl<F: FftFloat + Lerp, L: FdnKind> FdnKernel<F, L> {
    /// A network of `layout`. It has no lines until `prepare` gives it a rate.
    pub(crate) fn new(layout: L) -> Self {
        Self {
            layout,
            running: None,
        }
    }
}

mod sealed {
    use tutti_dsp::{FdnKind, FdnLayout, ShimmerLayout};

    use super::FdnNodeControls;
    use crate::ControlSet;

    /// A network kind the nodes run: its layout and the control set its node declares.
    pub trait Sealed: FdnKind {
        /// The rows of the kind's node.
        type Set: ControlSet<Controls = FdnNodeControls<Self>>;
    }

    impl Sealed for FdnLayout {
        type Set = super::FdnSet;
    }

    impl Sealed for ShimmerLayout {
        type Set = crate::ShimmerSet;
    }
}

impl<F: FftFloat + Lerp, L: sealed::Sealed> Kernel for FdnKernel<F, L> {
    type Float = F;
    type Set = L::Set;

    /// A cell that moves reaches the render at a chunk's start, wherever the blocks
    /// fall.
    const GRID: usize = FdnBank::<F, L>::CHUNK;

    fn shape(&self) -> Shape {
        Shape::buses(
            Buses::main(ChannelTopology::MONO),
            Buses::main(ChannelTopology::STEREO),
        )
        .cost(Cost::Moderate)
    }

    /// Sizes the lines, the pre-delay and the scratch of a design at `rate`: a
    /// re-prepare empties them.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        let bank = FdnBank::new(&self.layout, rate);
        let ramps = bank.ramps();
        self.running = Some(Running { rate, bank, ramps });
    }

    fn reset(&mut self) {
        if let Some(r) = &mut self.running {
            r.bank.reset();
        }
    }

    fn retarget(&mut self, controls: &FdnNodeControls<L>) {
        let Some(r) = &mut self.running else { return };
        r.bank
            .retarget(&mut r.ramps, &controls.network, &controls.kind);
    }

    fn jump_to_target(&mut self) {
        if let Some(r) = &mut self.running {
            r.ramps.jump_to_target();
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        match &mut self.running {
            Some(r) => r.bank.process(&mut r.ramps, block),
            None => Ok(()),
        }
    }

    /// The bank's ceiling, or [`Tail::Unbounded`] when that is past what a graph takes
    /// as a finite figure ([`MAX_NODE_TAIL`] at the prepared rate): a longer ring-out
    /// is declared unbounded, not as a figure the graph would refuse or count as never
    /// decaying. The node still narrows it each block to what rings.
    fn tail(&self) -> Tail {
        match self.running.as_ref().map(|r| (r.rate, r.bank.tail())) {
            None => Tail::Unknown,
            Some((rate, Tail::Finite(n))) if n > MAX_NODE_TAIL.to_samples_ceil(rate) => {
                Tail::Unbounded
            }
            Some((_, tail)) => tail,
        }
    }

    fn tail_of(&mut self, controls: &FdnNodeControls<L>) -> Tail {
        self.running.as_mut().map_or(Tail::Unknown, |r| {
            r.bank.tail_of(&r.ramps, &controls.network, &controls.kind)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::non_ringing;
    use tutti_units::{UnitAddr, UnitParam};

    /// The wet gain is the one control the tail does not read.
    #[test]
    fn only_the_wet_gain_is_flagged_no_ring() {
        assert_eq!(non_ringing(&FdnSet), [UnitAddr::first(UnitParam::GainDb)]);
    }

    /// The rows that state a longest value (the three decays and the freeze) never ring shorter
    /// as they move to it, whatever the other controls are.
    #[test]
    fn the_tail_is_longest_at_the_stated_decays_and_freeze() {
        use tutti_buffers::MaxBlock;
        use tutti_units::Samples;
        let mut kernel = FdnKernel::<f64, FdnLayout>::new(FdnLayout::default());
        kernel.prepare(SampleRate::SR_48K, MaxBlock::of(Samples::new(64)));
        // The layout clamps a decay at its longest (the default 8 s), so the sweep spans the
        // decays that differ; the stated value past it reads as that longest.
        let decay = (0.05, MaxDecay::DEFAULT.get().get());
        crate::test_support::assert_tail_longest_at_stated(
            &mut kernel,
            &FdnSet,
            &[
                decay,
                decay,
                decay,
                (0.0, 1.0),
                (0.0, 1.0),
                (0.0, 0.5),
                (-1.0, 1.0),
                (0.05, 5.0),
                (0.0, 2.0),
                (0.0, 0.0),
                (0.0, 1.0),
            ],
        );
    }

    /// A decay under the layout's longest never reports more than the longest does, where
    /// the other two bands hold the largest gain or sit just beside the driven one (the
    /// profiles whose shape, not their largest gain, could move a bound read off the
    /// response): every band, three sizes, two rates, the other bands and the driven one over
    /// a grid that crowds the layout's 8 s.
    #[test]
    fn a_decay_under_the_layouts_longest_never_reports_more_than_the_longest() {
        use tutti_buffers::MaxBlock;
        use tutti_units::{Mix, Samples, Seconds};
        let grid = [0.05, 0.3, 1.0, 2.0, 4.0, 6.0, 7.5, 7.9, 7.99, 8.0];
        for rate in [SampleRate::SR_48K, SampleRate::SR_96K] {
            let mut kernel = FdnKernel::<f64, FdnLayout>::new(FdnLayout::default());
            kernel.prepare(rate, MaxBlock::of(Samples::new(64)));
            for size in [0.0, 0.5, 1.0] {
                for band in 0..3 {
                    for other in grid {
                        for third in grid {
                            let mut c = FdnNodeControls::<FdnLayout>::DEFAULT;
                            c.network.size = Mix::new(size);
                            c.network.decay[(band + 1) % 3] = Seconds::new(other);
                            c.network.decay[(band + 2) % 3] = Seconds::new(third);
                            c.network.decay[band] = Seconds::new(MaxDecay::CAP.get());
                            let Tail::Finite(longest) = kernel.tail_of(&c) else {
                                panic!("a finite tail with the freeze off")
                            };
                            for decay in grid {
                                c.network.decay[band] = Seconds::new(decay);
                                let Tail::Finite(n) = kernel.tail_of(&c) else {
                                    panic!("a finite tail with the freeze off")
                                };
                                assert!(
                                    n <= longest,
                                    "band {band} at {decay} s, the others {other} and {third} s, \
                                     size {size}, {rate:?}: {n:?} past {longest:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
