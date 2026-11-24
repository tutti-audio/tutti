//! Freeverb, [`FreeverbNode`].
//!
//! [`FreeverbBank`] is the combs and all-passes over buffers views (the lines, the ramps);
//! this file holds the kernel that retargets it, the cells and how the graph reaches them.

use tutti_buffers::Float;
use tutti_buffers::{ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, FreeverbBank, FreeverbControls, FreeverbRamps, Lerp};
use tutti_graph::{Buses, Cost, MAX_NODE_TAIL, Shape};
use tutti_units::{Mix, SampleRate, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// Freeverb as a graph node: mono in, stereo out, wet only.
///
/// A [`KernelNode`] over [`FreeverbKernel`], the combs and all-passes being
/// [`FreeverbBank`]'s. Its blend with its input, for use as an insert, is the graph's mix
/// (`Editor::set_mix`); it declares no latency. Made from a [`FreeverbConfig`]
/// (`FreeverbNode::from`); [`FreeverbConfig::new`] is the General MIDI reverb send of the
/// SoundFont player.
///
/// # The controls
///
/// The ports are the room size and the damping, each a position on `0..1`
/// ([`FreeverbControls`]). A cell that moves lands whole on the 32-frame grid and ramps in
/// the bank over 20 ms.
///
/// # The tail
///
/// The declared tail is the ceiling: the room at its largest, whatever the damping (the
/// loop's DC gain is the comb's feedback alone). The node narrows it each block to what the controls in force ring, and once the
/// input has been silent for that long it cuts the room off and parks ([`KernelNode`]).
///
/// `F` is the float of the graph it runs in, and of the combs and all-passes; the lines hold
/// Freeverb's tunings scaled to the rate (about 24,000 frames a channel at 44.1 kHz in all), so
/// an `f32` graph halves the memory of an `f64` one. The General MIDI send is an `f32` node in
/// the session.
pub type FreeverbNode<F> = KernelNode<FreeverbKernel<F>>;

/// What a [`FreeverbNode`] is made from: the room size and damping it starts at, which are its
/// two cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FreeverbConfig {
    controls: FreeverbControls,
}

impl FreeverbConfig {
    /// A reverb whose room size and damping are cells, starting at
    /// [`FreeverbControls::GENERAL_MIDI`] (room 0.5 and damping 0.5: the General MIDI reverb
    /// send), mono in and stereo out, wet only. It has no rate until the graph's `prepare`
    /// gives it one: it declares no latency and an unknown tail before, and renders silence.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            controls: FreeverbControls::GENERAL_MIDI,
        }
    }

    /// The room size and damping the reverb starts at.
    #[must_use]
    pub const fn controls(mut self, controls: FreeverbControls) -> Self {
        self.controls = controls;
        self
    }
}

impl Default for FreeverbConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Float + Lerp> From<FreeverbConfig> for FreeverbNode<F> {
    /// Allocates: build it before the node goes live.
    fn from(config: FreeverbConfig) -> Self {
        Self::with_kernel(FreeverbKernel::new(), FreeverbSet, config.controls)
    }
}

/// Freeverb's controls: the room size and the damping, as positions on `0..1`.
#[derive(Clone, Copy, Debug)]
pub struct FreeverbSet;

// Each lands whole: the bank's ramps are the ramp.
control_set! {
    FreeverbSet(_s): FreeverbControls = FreeverbControls::GENERAL_MIDI;
    live: (all);
    RoomSize => Mix = room { whole },
    Damping => Mix = damp { whole };

    fn defaults(&self) -> FreeverbControls {
        FreeverbControls::GENERAL_MIDI
    }
}

/// A [`FreeverbBank`] and its ramps, once the rate is known.
#[derive(Clone, Debug)]
struct Running<F: Float> {
    /// The rate the bank was sized at: what a tail bound is converted at.
    rate: SampleRate,
    bank: FreeverbBank<F>,
    ramps: FreeverbRamps<F>,
}

/// Freeverb as a [`Kernel`]: a [`FreeverbBank`] sized at the prepared rate and the ramps its
/// controls are set through.
///
/// The bank's state is `F`, the float of the graph.
#[derive(Clone, Debug)]
pub struct FreeverbKernel<F: Float> {
    /// `None` until [`prepare`](Kernel::prepare).
    running: Option<Running<F>>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
}

impl<F: Float + Lerp> FreeverbKernel<F> {
    /// A reverb with no lines until `prepare` gives it a rate.
    fn new() -> Self {
        Self {
            running: None,
            applied: false,
        }
    }
}

impl<F: Float + Lerp> Kernel for FreeverbKernel<F> {
    type Float = F;
    type Set = FreeverbSet;

    /// A cell that moves reaches the render at a chunk's start, wherever the blocks fall.
    const GRID: usize = FreeverbBank::<F>::CHUNK;

    fn shape(&self) -> Shape {
        Shape::buses(
            Buses::main(ChannelTopology::MONO),
            Buses::main(ChannelTopology::STEREO),
        )
        .cost(Cost::Moderate)
    }

    /// Sizes the lines at `rate` (Freeverb's tunings scaled to it): a re-prepare empties them.
    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        let bank = FreeverbBank::new(rate);
        let ramps = bank.ramps();
        self.running = Some(Running { rate, bank, ramps });
        self.applied = false;
    }

    /// Silence, and the next design lands at once, as a fresh node's first does.
    fn reset(&mut self) {
        if let Some(r) = &mut self.running {
            r.bank.reset();
            r.ramps.jump_to_target();
        }
        self.applied = false;
    }

    fn retarget(&mut self, controls: &FreeverbControls) {
        let Some(r) = &mut self.running else { return };
        r.bank.retarget(&mut r.ramps, controls);
        if !self.applied {
            r.ramps.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let rendered = match &mut self.running {
            Some(r) => r.bank.process(&mut r.ramps, block),
            None => Ok(()),
        };
        // A design after a render is a move, which ramps.
        self.applied = true;
        rendered
    }

    /// The bank's ceiling (the room at its largest), or [`Tail::Unbounded`] when that is past what a graph takes as a finite figure
    /// ([`MAX_NODE_TAIL`] at the prepared rate).
    fn tail(&self) -> Tail {
        let Some(r) = &self.running else {
            return Tail::Unknown;
        };
        match r.bank.tail() {
            Tail::Finite(n) if n > MAX_NODE_TAIL.to_samples_ceil(r.rate) => Tail::Unbounded,
            tail => tail,
        }
    }

    fn tail_of(&mut self, controls: &FreeverbControls) -> Tail {
        self.running
            .as_ref()
            .map_or(Tail::Unknown, |r| r.bank.tail_of(controls))
    }
}
