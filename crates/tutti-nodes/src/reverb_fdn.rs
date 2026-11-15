//! The feedback delay network, [`FdnNode`].
//!
//! [`FdnBank`] is the network over buffers views (the lines, the absorption, the mix, the
//! glides); this file holds the kernel that aims it, the cells and how the graph reaches
//! them.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{FdnBank, FdnControls, FdnGlides, FdnLayout};
use tutti_graph::{Bus, BusRole, Buses, Cost, MAX_NODE_TAIL, Shape};
use tutti_types::{
    BusTopology, Db, Depth, Hz, MaxBlock, Mix, SampleRate, Seconds, StereoWidth, Tail,
};

use crate::kernel::{ControlLanes, Kernel, KernelNode};

/// The feedback delay network as a graph node: mono in, stereo out, wet only.
///
/// A [`KernelNode`] over [`FdnKernel`], the network being [`FdnBank`]'s. Its blend with its
/// input, for use as an insert, is the graph's mix (`Editor::set_mix`); the node declares no
/// latency (its pre-delay is part of the wet sound).
///
/// # The controls
///
/// The ports are the three decays (`Decay[0]` the mid band, `[1]` the low, `[2]` the high),
/// the room size (a move lands over seconds by design: half a frame per 32-frame chunk,
/// about 6 s for the longest line going from 0 to 1, to avoid pitch artefacts), the
/// diffusion, the pre-delay, the modulation's depth and rate, the width, the wet gain and,
/// if the layout can freeze, the freeze: a cell at or above 0.5. Each lands whole on the
/// 32-frame grid and glides in the bank.
///
/// # The tail
///
/// The declared tail is the ceiling: the layout's longest decay, or unbounded if it can
/// freeze. The node narrows it each block to what the controls in force ring, and once the
/// input has been silent for that long it cuts the room off and parks ([`KernelNode`]).
pub type FdnNode = KernelNode<FdnKernel>;

impl FdnNode {
    /// A network of `layout`, its cells at `controls`. It has no rate until the graph's
    /// `prepare` gives it one: it declares no latency and an unknown tail before, and
    /// renders silence. Allocates: build it before the node goes live.
    #[doc(hidden)]
    #[must_use]
    pub fn live(layout: FdnLayout, controls: FdnControls) -> Self {
        let set = FdnSet {
            freezable: layout.freezable(),
        };
        Self::with_kernel(FdnKernel::new(layout), set, controls)
    }
}

/// The FDN's controls: the three decays, the size, the diffusion, the pre-delay, the
/// modulation's depth and rate, the width, the wet gain and (if the layout can freeze) the
/// freeze.
#[derive(Clone, Copy, Debug)]
pub struct FdnSet {
    freezable: bool,
}

// In port order after the mix; a layout that cannot freeze leaves off the last. Every row
// lands whole: the bank's glides are the ramp.
control_set! {
    FdnSet(s): FdnControls = FdnControls { freeze: false, ..FdnControls::DEFAULT };
    live: (if s.freezable { 11 } else { 10 });
    Decay[0] => Seconds = decay[0] { whole },
    Decay[1] => Seconds = decay[1] { whole },
    Decay[2] => Seconds = decay[2] { whole },
    RoomSize => Mix = size { whole },
    Diffusion => Mix = diffusion { whole },
    DelayTime => Seconds = pre_delay { whole },
    Depth => Depth = modulation { whole },
    Rate => Hz = rate { whole },
    StereoSpread => StereoWidth = width { whole },
    GainDb => Db = wet { whole },
    Freeze => bool = freeze { whole };

    fn defaults(&self) -> FdnControls {
        FdnControls::DEFAULT
    }
}

/// A [`FdnBank`] and its glides, once the rate is known.
#[derive(Clone, Debug)]
struct Running {
    bank: FdnBank,
    glides: FdnGlides,
}

/// The feedback delay network as a [`Kernel`]: a [`FdnBank`] sized at the prepared rate and
/// the glides its design is aimed through.
///
/// The bank's state is `f32`; the graph's blocks are too, and another sample format is
/// converted at the edges.
#[derive(Clone, Debug)]
pub struct FdnKernel {
    layout: FdnLayout,
    /// `None` until [`prepare`](Kernel::prepare).
    running: Option<Running>,
    /// Whether the first retarget after `prepare` has landed.
    landed: bool,
}

impl FdnKernel {
    /// A network of `layout`. It has no lines until `prepare` gives it a rate.
    fn new(layout: FdnLayout) -> Self {
        Self {
            layout,
            running: None,
            landed: false,
        }
    }
}

impl Kernel for FdnKernel {
    type Set = FdnSet;

    /// A cell that moves reaches the render at a chunk's start, wherever the blocks fall.
    const GRID: usize = FdnBank::CHUNK;

    fn shape(&self) -> Shape {
        let output = Bus::banded(BusTopology::STEREO, BusRole::Main, 1).expect("one band is a bus");
        Shape::buses(
            Buses::main(BusTopology::MONO),
            Buses::NONE.with(output).expect("one bus"),
        )
        .with_cost(Cost::Moderate)
    }

    /// Sizes the lines, the pre-delay and the scratch of a design at `rate`: a re-prepare
    /// empties them.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        let bank = FdnBank::new(&self.layout, rate);
        let glides = bank.glides();
        self.running = Some(Running { bank, glides });
        self.landed = false;
    }

    fn reset(&mut self) {
        if let Some(r) = &mut self.running {
            r.bank.reset();
            r.glides.settle();
        }
    }

    fn retarget(&mut self, controls: &FdnControls) {
        let Some(r) = &mut self.running else { return };
        r.bank.aim(&mut r.glides, controls);
        if !self.landed {
            r.glides.settle();
            self.landed = true;
        }
    }

    fn render<S: Float>(
        &mut self,
        input: Planes<'_, S>,
        _: ControlLanes<'_, S>,
        output: PlanesMut<'_, '_, S>,
    ) {
        if let Some(r) = &mut self.running {
            r.bank.process(&mut r.glides, input, output);
        }
    }

    /// The bank's ceiling, or [`Tail::Unbounded`] when that is past what a graph takes as a
    /// finite figure ([`MAX_NODE_TAIL`]): a longer ring-out is declared unbounded, not as a
    /// figure the graph would refuse or count as never decaying. The node still narrows it
    /// each block to what rings.
    fn tail(&self) -> Tail {
        match self.running.as_ref().map(|r| r.bank.tail()) {
            None => Tail::Unknown,
            Some(Tail::Finite(n)) if n > MAX_NODE_TAIL => Tail::Unbounded,
            Some(tail) => tail,
        }
    }

    fn tail_of(&mut self, controls: &FdnControls) -> Tail {
        self.running
            .as_mut()
            .map_or(Tail::Unknown, |r| r.bank.tail_of(&r.glides, controls))
    }
}
