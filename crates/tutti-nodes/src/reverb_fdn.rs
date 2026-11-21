//! The feedback delay network, [`FdnNode`].
//!
//! [`FdnBank`] is the network over buffers views (the lines, the absorption, the mix, the
//! ramps); this file holds the kernel that retargets it, the cells and how the graph reaches
//! them.

use tutti_buffers::{BusTopology, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, FdnBank, FdnControls, FdnLayout, FdnRamps, FftFloat, FreezeMode, Lerp,
};
use tutti_graph::{Bus, BusRole, Buses, Cost, MAX_NODE_TAIL, Shape};
use tutti_units::{Db, Depth, Hz, Mix, SampleRate, Seconds, StereoWidth, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// The feedback delay network as a graph node: mono in, stereo out, wet only.
///
/// A [`KernelNode`] over [`FdnKernel`], the network being [`FdnBank`]'s. Its blend with its
/// input, for use as an insert, is the graph's mix (`Editor::set_mix`); the node declares no
/// latency (its pre-delay is part of the wet sound). Made from an [`FdnConfig`]
/// (`FdnNode::from`).
///
/// # The controls
///
/// The ports are the three decays (`Decay[0]` the mid band, `[1]` the low, `[2]` the high),
/// the room size (a move lands over seconds by design: half a frame per 32-frame chunk,
/// about 6 s for the longest line going from 0 to 1, to avoid pitch artefacts), the
/// diffusion, the pre-delay, the modulation's depth and rate, the width, the wet gain and,
/// if the layout can freeze, the freeze: a cell at or above 0.5. Each lands whole on the
/// 32-frame grid and ramps in the bank.
///
/// # The tail
///
/// The declared tail is the ceiling: the layout's longest decay, or unbounded if it can
/// freeze. The node narrows it each block to what the controls in force ring, and once the
/// input has been silent for that long it cuts the room off and parks ([`KernelNode`]).
///
/// `F` is the float of the graph it runs in, and of the lines, the pre-delay and the ramps;
/// the lines are the layout's worst case (its most lines at the longest length the size and the
/// modulation reach), so an `f32` graph halves the memory of an `f64` one.
pub type FdnNode<F> = KernelNode<FdnKernel<F>>;

/// What an [`FdnNode`] is made from: the network's layout and the controls its cells start at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FdnConfig {
    layout: FdnLayout,
    controls: FdnControls,
}

impl FdnConfig {
    /// A network of [`FdnLayout::default`] with its cells at [`FdnControls::DEFAULT`]. It has
    /// no rate until the graph's `prepare` gives it one: it declares no latency and an unknown
    /// tail before, and renders silence.
    #[must_use]
    pub fn new() -> Self {
        Self {
            layout: FdnLayout::default(),
            controls: FdnControls::DEFAULT,
        }
    }

    /// The network's lines, their longest decay and whether it can freeze.
    #[must_use]
    pub const fn layout(mut self, layout: FdnLayout) -> Self {
        self.layout = layout;
        self
    }

    /// The controls the cells start at.
    #[must_use]
    pub const fn controls(mut self, controls: FdnControls) -> Self {
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
        let set = FdnSet {
            freezable: config.layout.freeze_mode() == FreezeMode::Freezable,
        };
        Self::with_kernel(FdnKernel::new(config.layout), set, config.controls)
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
// lands whole: the bank's ramps are the ramp.
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

/// A [`FdnBank`] and its ramps, once the rate is known.
#[derive(Clone, Debug)]
struct Running<F: FftFloat> {
    /// The rate the bank was sized at: what a tail bound is converted at.
    rate: SampleRate,
    bank: FdnBank<F>,
    ramps: FdnRamps<F>,
}

/// The feedback delay network as a [`Kernel`]: a [`FdnBank`] sized at the prepared rate and
/// the ramps its design is set through.
///
/// The bank's state is `F`, the float of the graph.
#[derive(Clone, Debug)]
pub struct FdnKernel<F: FftFloat> {
    layout: FdnLayout,
    /// `None` until [`prepare`](Kernel::prepare).
    running: Option<Running<F>>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
}

impl<F: FftFloat + Lerp> FdnKernel<F> {
    /// A network of `layout`. It has no lines until `prepare` gives it a rate.
    pub(crate) fn new(layout: FdnLayout) -> Self {
        Self {
            layout,
            running: None,
            applied: false,
        }
    }
}

impl<F: FftFloat + Lerp> Kernel for FdnKernel<F> {
    type Float = F;
    type Set = FdnSet;

    /// A cell that moves reaches the render at a chunk's start, wherever the blocks fall.
    const GRID: usize = FdnBank::<F>::CHUNK;

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
        let ramps = bank.ramps();
        self.running = Some(Running { rate, bank, ramps });
        self.applied = false;
    }

    fn reset(&mut self) {
        if let Some(r) = &mut self.running {
            r.bank.reset();
            r.ramps.jump_to_target();
        }
    }

    fn retarget(&mut self, controls: &FdnControls) {
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
        let (input, output) = block.split();
        if let Some(r) = &mut self.running {
            r.bank.process(&mut r.ramps, input, output);
        }
        Ok(())
    }

    /// The bank's ceiling, or [`Tail::Unbounded`] when that is past what a graph takes as a
    /// finite figure ([`MAX_NODE_TAIL`] at the prepared rate): a longer ring-out is declared
    /// unbounded, not as a figure the graph would refuse or count as never decaying. The node
    /// still narrows it each block to what rings.
    fn tail(&self) -> Tail {
        match self.running.as_ref().map(|r| (r.rate, r.bank.tail())) {
            None => Tail::Unknown,
            Some((rate, Tail::Finite(n))) if n > MAX_NODE_TAIL.to_samples_ceil(rate) => {
                Tail::Unbounded
            }
            Some((_, tail)) => tail,
        }
    }

    fn tail_of(&mut self, controls: &FdnControls) -> Tail {
        self.running
            .as_mut()
            .map_or(Tail::Unknown, |r| r.bank.tail_of(&r.ramps, controls))
    }
}

#[cfg(test)]
mod tests {
    // The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
    // format, so their tolerances are `f32`'s.
    use super::*;
    use crate::test_support::drive_kernel;
    use tutti_dsp::MaxDecay;
    use tutti_fixtures::signal::wide_signal;
    use tutti_units::Samples;

    /// An `f64` block through a `FdnKernel::<f64>` with modulation off is linear: `R(a x + b y)
    /// = a R(x) + b R(y)` to 2^-40 of the peak, with `a`, `b` and two wide signals that an `f32`
    /// state cannot carry (it is off by 2^-24).
    #[test]
    fn an_f64_kernel_is_linear() {
        let len = 6_000;
        let x = wide_signal(len);
        let y: Vec<f64> = x.iter().rev().map(|v| v - 0.3).collect();
        let (a, b) = (1.0 / 3.0, -std::f64::consts::PI / 7.0);
        let mix: Vec<f64> = x.iter().zip(&y).map(|(x, y)| a * x + b * y).collect();
        let moves = [(
            0,
            FdnControls {
                modulation: Depth::of(0.0),
                ..FdnControls::DEFAULT
            },
        )];
        // Two identical input channels: the bank averages them, and the block has the stereo
        // width of the output the harness reads back.
        let render = |input: &[f64]| {
            let mut k = FdnKernel::<f64>::new(
                FdnLayout::new(8, MaxDecay::DEFAULT, FreezeMode::Decaying).unwrap(),
            );
            k.prepare(SampleRate::SR_48K, MaxBlock::of(Samples::new(len)));
            drive_kernel(&mut k, &moves, &[input.to_vec(), input.to_vec()])
        };
        let (rx, ry, rm) = (render(&x), render(&y), render(&mix));
        for c in 0..2 {
            let peak = rm[c].iter().fold(0.0f64, |p, v| p.max(v.abs()));
            assert!(peak > 1e-4, "the reverb sounds");
            for n in 0..len {
                let want = a * rx[c][n] + b * ry[c][n];
                assert!(
                    (rm[c][n] - want).abs() <= peak * 2.0f64.powi(-40),
                    "channel {c}, frame {n}: {} against {want}",
                    rm[c][n]
                );
            }
        }
    }
}
