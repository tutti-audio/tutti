//! The Dattorro plate, [`PlateReverbNode`].
//!
//! [`PlateBank`] is the plate over buffers views (the lattices, the delays, the output taps,
//! the glides); this file holds the kernel that aims it, the cells and how the graph reaches
//! them.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{PlateBank, PlateControls, PlateGlides};
use tutti_graph::{Bus, BusRole, Buses, Cost, MAX_NODE_TAIL, Shape};
use tutti_types::{BusTopology, Depth, Hz, MaxBlock, Mix, SampleRate, Seconds, Tail};

use crate::kernel::{ControlLanes, Kernel, KernelNode};

/// What a [`PlateReverbNode`] is made from: the controls it starts at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlateReverbConfig {
    controls: PlateControls,
}

impl PlateReverbConfig {
    /// A plate at the controls of [`PlateControls::DEFAULT`]: a decay of 1.8 s (the RT60 of
    /// Dattorro's decay of 0.5), the paper's damping, a 20 kHz bandwidth, no pre-delay.
    #[must_use]
    pub fn new() -> Self {
        Self {
            controls: PlateControls::DEFAULT,
        }
    }

    /// The RT60 of the tank's loop, from 50 ms to 8 s. The rendered decay is exactly this only
    /// at diffusion 0 with no damping and no modulation; damping adds a little to each half's
    /// loop and diffusion makes the decay a mixture over frequency that a T30 fit reads longer
    /// (8 to 10% at the paper's diffusion): see [`PlateControls::decay`].
    #[must_use]
    pub fn decay(mut self, decay: Seconds) -> Self {
        self.controls.decay = decay;
        self
    }

    /// The tank's damping, the pole of its one-pole low-pass: `0` is none, `0.999` the darkest.
    #[must_use]
    pub fn damping(mut self, damping: Mix) -> Self {
        self.controls.damping = damping;
        self
    }

    /// The delay before the first sound, up to half a second.
    #[must_use]
    pub fn predelay(mut self, predelay: Seconds) -> Self {
        self.controls.pre_delay = predelay;
        self
    }

    /// The input bandwidth: the cutoff of the low-pass the input goes through.
    #[must_use]
    pub fn bandwidth(mut self, bandwidth: Hz) -> Self {
        self.controls.bandwidth = bandwidth;
        self
    }

    /// How diffuse the lattices are, `0..1`: `1` is the paper's coefficients and `0` makes every
    /// lattice a plain delay.
    #[must_use]
    pub fn diffusion(mut self, diffusion: Mix) -> Self {
        self.controls.diffusion = diffusion;
        self
    }

    /// The depth of the tank's delay modulation, `0..1`.
    #[must_use]
    pub fn modulation(mut self, modulation: Depth) -> Self {
        self.controls.modulation = modulation;
        self
    }
}

impl Default for PlateReverbConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// J. Dattorro's plate reverberator as a graph node: stereo in (the channels are averaged),
/// stereo out, wet only.
///
/// A [`KernelNode`] over [`PlateKernel`], the plate being [`PlateBank`]'s. Its blend with its
/// input, for use as an insert, is the graph's mix (`Editor::set_mix`); it declares no latency
/// (its pre-delay is part of the wet sound). Made from a [`PlateReverbConfig`]
/// (`PlateReverbNode::from`).
///
/// # The controls
///
/// The ports are the decay (an RT60 in seconds), the damping, the pre-delay, the input
/// bandwidth (as [`UnitParam::Cutoff`](tutti_types::UnitParam::Cutoff)), the modulation
/// depth and the diffusion. Each lands whole on the 32-frame grid and glides in the bank.
///
/// # The tail
///
/// The declared tail is the ceiling: the longest decay and pre-delay. The node narrows it
/// each block to what the controls in force ring, and once the input has been silent for that
/// long it cuts the room off and parks ([`KernelNode`]).
pub type PlateReverbNode = KernelNode<PlateKernel>;

impl From<PlateReverbConfig> for PlateReverbNode {
    fn from(config: PlateReverbConfig) -> Self {
        Self::with_kernel(PlateKernel::new(), PlateSet, config.controls)
    }
}

/// The plate's controls: the decay, the damping, the pre-delay, the bandwidth, the
/// modulation depth and the diffusion.
#[derive(Clone, Copy, Debug)]
pub struct PlateSet;

// Each lands whole: the bank's glides are the ramp.
control_set! {
    PlateSet(_s): PlateControls = PlateControls::DEFAULT;
    live: (all);
    Decay => Seconds = decay { whole },
    Damping => Mix = damping { whole },
    DelayTime => Seconds = pre_delay { whole },
    Cutoff => Hz = bandwidth { whole },
    Depth => Depth = modulation { whole },
    Diffusion => Mix = diffusion { whole };

    fn defaults(&self) -> PlateControls {
        PlateControls::DEFAULT
    }
}

/// A [`PlateBank`] and its glides, once the rate is known.
#[derive(Clone, Debug)]
struct Running {
    /// The rate the bank was sized at: what a tail bound is converted at.
    rate: SampleRate,
    bank: PlateBank,
    glides: PlateGlides,
}

/// The plate as a [`Kernel`]: a [`PlateBank`] sized at the prepared rate and the glides its
/// design is aimed through.
///
/// The bank's state is `f32`; the graph's blocks are too, and another sample format is
/// converted at the edges.
#[derive(Clone, Debug)]
pub struct PlateKernel {
    /// `None` until [`prepare`](Kernel::prepare).
    running: Option<Running>,
    /// Whether the first retarget after `prepare` has landed.
    landed: bool,
}

impl PlateKernel {
    /// A plate with no lines until `prepare` gives it a rate.
    fn new() -> Self {
        Self {
            running: None,
            landed: false,
        }
    }
}

impl Kernel for PlateKernel {
    type Set = PlateSet;

    /// A cell that moves reaches the render at a chunk's start, wherever the blocks fall.
    const GRID: usize = PlateBank::CHUNK;

    fn shape(&self) -> Shape {
        let stereo = |role| Bus::banded(BusTopology::STEREO, role, 1).expect("one band is a bus");
        Shape::buses(
            Buses::NONE.with(stereo(BusRole::Main)).expect("one bus"),
            Buses::NONE.with(stereo(BusRole::Main)).expect("one bus"),
        )
        .with_cost(Cost::Moderate)
    }

    /// Sizes the lines at `rate` (the paper's lengths scaled to it): a re-prepare empties them.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        let bank = PlateBank::new(rate);
        let glides = bank.glides();
        self.running = Some(Running { rate, bank, glides });
        self.landed = false;
    }

    /// Silence, and the next design lands at once, as a fresh node's first does.
    fn reset(&mut self) {
        if let Some(r) = &mut self.running {
            r.bank.reset();
            r.glides.settle();
        }
    }

    fn retarget(&mut self, controls: &PlateControls) {
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
    /// finite figure ([`MAX_NODE_TAIL`] at the prepared rate).
    fn tail(&self) -> Tail {
        match self.running.as_ref().map(|r| (r.rate, r.bank.tail())) {
            None => Tail::Unknown,
            Some((rate, Tail::Finite(n))) if n > MAX_NODE_TAIL.to_samples_ceil(rate) => {
                Tail::Unbounded
            }
            Some((_, tail)) => tail,
        }
    }

    fn tail_of(&mut self, controls: &PlateControls) -> Tail {
        self.running
            .as_mut()
            .map_or(Tail::Unknown, |r| r.bank.tail_of(&r.glides, controls))
    }
}
