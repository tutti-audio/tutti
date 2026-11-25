//! The shimmer reverb, [`ShimmerNode`]: a feedback delay network whose feedback is shifted in
//! pitch.
//!
//! The network is [`FdnBank`](tutti_dsp::FdnBank)'s, run by an [`FdnKernel`] on a
//! [`ShimmerLayout`]; this file holds the controls it adds to the
//! [`FdnSet`](crate::FdnSet)'s and how the graph reaches them.

use tutti_dsp::{FdnControls, FftFloat, Lerp, MaxDecay, ShimmerControls, ShimmerLayout};
use tutti_units::{Bipolar, Cents, Db, Feedback, Hz, Mix, Seconds, StereoWidth};

use crate::kernel::KernelNode;
use crate::reverb_fdn::{FdnKernel, FdnNodeControls};

/// A feedback delay network with a pitch shifter in its feedback: mono in, stereo out, wet only.
///
/// A [`KernelNode`] over an [`FdnKernel`] of a [`ShimmerLayout`]. Each trip round the loop, the share of the feedback
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
pub type ShimmerNode<F> = KernelNode<FdnKernel<F, ShimmerLayout>>;

/// What a [`ShimmerNode`] is made from: the network's layout and the controls its cells start
/// at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShimmerConfig {
    layout: ShimmerLayout,
    controls: FdnNodeControls<ShimmerLayout>,
}

impl ShimmerConfig {
    /// A [`ShimmerLayout`] of eight lines, RT60s up to [`MaxDecay::DEFAULT`], the cells at
    /// [`FdnControls::DEFAULT`]: an octave up (`1200` cents), at a send of `0.5`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            layout: ShimmerLayout::default(),
            controls: FdnNodeControls {
                network: FdnControls::DEFAULT,
                kind: ShimmerControls {
                    send: Feedback::new_clamped(0.5),
                    ..ShimmerControls::DEFAULT
                },
            },
        }
    }

    /// The network's lines and their longest decay.
    #[must_use]
    pub const fn layout(mut self, layout: ShimmerLayout) -> Self {
        self.layout = layout;
        self
    }

    /// The controls the cells start at.
    #[must_use]
    pub const fn controls(mut self, controls: FdnNodeControls<ShimmerLayout>) -> Self {
        self.controls = controls;
        self
    }

    /// The shift the cell starts at.
    #[must_use]
    pub const fn pitch(mut self, pitch: Cents) -> Self {
        self.controls.kind.pitch = pitch;
        self
    }

    /// The share of the feedback the cell starts at sending through the shifter.
    #[must_use]
    pub const fn send(mut self, send: Feedback) -> Self {
        self.controls.kind.send = send;
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
        Self::with_kernel(FdnKernel::new(config.layout), ShimmerSet, config.controls)
    }
}

/// The shimmer's controls: the [`FdnSet`](crate::FdnSet)'s (the three decays, the size, the
/// diffusion, the pre-delay, the modulation's depth and rate, the width and the wet gain), the
/// shift and the send.
#[derive(Clone, Copy, Debug)]
pub struct ShimmerSet;

control_set! {
    ShimmerSet(_s): FdnNodeControls<ShimmerLayout> = FdnNodeControls {
        network: FdnControls::DEFAULT,
        kind: ShimmerControls::DEFAULT,
    };
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
    Pitch => Cents = kind.pitch { whole, no_ring },
    Feedback => Feedback = kind.send { whole, longest = Feedback::MAX_STABLE.get() };

    fn defaults(&self) -> FdnNodeControls<ShimmerLayout> {
        FdnNodeControls {
            network: FdnControls::DEFAULT,
            kind: ShimmerControls::DEFAULT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::Kernel;
    use crate::test_support::non_ringing;
    use tutti_graph::ParamNode as _;
    use tutti_units::{SampleRate, UnitAddr, UnitParam};

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

    /// The rows that state a longest value (the three decays and the send) never ring shorter
    /// as they move to it, whatever the other controls are.
    #[test]
    fn the_tail_is_longest_at_the_stated_decays_and_send() {
        use tutti_buffers::MaxBlock;
        let mut kernel = FdnKernel::<f64, ShimmerLayout>::new(ShimmerLayout::default());
        kernel.prepare(
            SampleRate::SR_48K,
            MaxBlock::of(tutti_units::Samples::new(64)),
        );
        // The layout clamps a decay at its longest (the default 8 s), so the sweep spans the
        // decays that differ; the stated value past it reads as that longest.
        let decay = (0.05, MaxDecay::DEFAULT.get().get());
        crate::test_support::assert_tail_longest_at_stated(
            &mut kernel,
            &ShimmerSet,
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
                (0.0, 0.0),
                (0.0, 0.99),
            ],
        );
    }

    /// The controls the last `retarget` of the wrapped kernel took.
    type Seen = tutti_fixtures::Shared<Option<FdnNodeControls<ShimmerLayout>>>;

    /// A shimmer kernel that records what its `retarget` is given.
    #[derive(Clone, Debug)]
    struct Spy {
        fdn: FdnKernel<f32, ShimmerLayout>,
        seen: Seen,
    }

    impl Kernel for Spy {
        type Float = f32;
        type Set = ShimmerSet;

        const GRID: usize = <FdnKernel<f32, ShimmerLayout> as Kernel>::GRID;

        fn shape(&self) -> tutti_graph::Shape {
            self.fdn.shape()
        }

        fn prepare(&mut self, rate: SampleRate, max_block: tutti_buffers::MaxBlock) {
            self.fdn.prepare(rate, max_block);
        }

        fn reset(&mut self) {
            self.fdn.reset();
        }

        fn retarget(&mut self, controls: &FdnNodeControls<ShimmerLayout>) {
            *self.seen.lock().unwrap() = Some(*controls);
            self.fdn.retarget(controls);
        }

        fn jump_to_target(&mut self) {
            self.fdn.jump_to_target();
        }

        fn render(
            &mut self,
            block: &mut tutti_buffers::InputOutput<'_, f32>,
            signals: crate::ControlSignals<'_, f32>,
        ) -> Result<(), tutti_dsp::BlockMismatch> {
            self.fdn.render(block, signals)
        }

        fn tail(&self) -> tutti_units::Tail {
            self.fdn.tail()
        }
    }

    /// A control a row names through a field path (`kind.pitch`, `kind.send`,
    /// `network.decay[1]`) is written where it names: the cells a port holds reach `retarget`
    /// as the shift, the send and the low band's decay, and no other field moves.
    #[test]
    fn a_driven_nested_control_reaches_retarget_in_its_field() {
        let seen = Seen::default();
        let spy = Spy {
            fdn: FdnKernel::new(ShimmerLayout::default()),
            seen: seen.clone(),
        };
        let controls = FdnNodeControls::<ShimmerLayout> {
            network: FdnControls::DEFAULT,
            kind: ShimmerControls::DEFAULT,
        };
        let mut node = tutti_graph::contract::prepared(
            KernelNode::with_kernel(spy, ShimmerSet, controls),
            SampleRate::SR_48K,
            64,
        );
        let cells = node.param_set();
        assert!(cells.set(UnitParam::Pitch, 700.0));
        assert!(cells.set(UnitParam::Feedback, 0.25));
        assert!(cells.set(UnitAddr::new(UnitParam::Decay, 1), 3.0));
        let input = vec![0.25f32; 64];
        for _ in 0..2 {
            tutti_graph::contract::drive(
                &mut node,
                SampleRate::SR_48K,
                tutti_buffers::Planes::new(&[&input[..]]).unwrap(),
                &[],
            );
        }
        let got = seen.lock().unwrap().expect("the kernel was retargeted");
        let mut want = controls;
        want.kind.pitch = Cents::new(700.0);
        want.kind.send = Feedback::new(0.25);
        want.network.decay[1] = Seconds::new(3.0);
        assert_eq!(got, want);
    }

    /// A fresh node, a reset one, a re-prepared one and a fork land their first design whole.
    /// A diagonal two-line shimmer at one decay in every band, a send `k` and no shift, takes
    /// an impulse `c_0` into line 0, whose first echo is `ear c_0` at `t0` and whose second is
    /// `ear g_0 c_0 (1 - k / 2)` at `2 t0` (`g_0` the loop gain of the line): the first needs
    /// the input gain on its target at the first frame, the second the absorption and the send
    /// ramp as well, none of which a ramp from the idle coefficients reaches in a trip.
    #[test]
    fn the_first_block_after_a_prepare_a_reset_or_a_fork_lands_whole() {
        use crate::test_support::{RATE, render_chunked};
        use tutti_dsp::{Band, FdnLayout, NetworkStage};
        use tutti_graph::ParamNode as _;
        use tutti_graph::{Node as _, Prepare};
        use tutti_units::Bipolar;

        let k = 0.5;
        let controls = FdnNodeControls::<ShimmerLayout> {
            network: FdnControls {
                decay: [Seconds::new(2.0); 3],
                diffusion: Mix::new(0.0),
                modulation: Bipolar::of(0.0),
                ..FdnControls::DEFAULT
            },
            kind: ShimmerControls {
                pitch: Cents::new(0.0),
                send: Feedback::new_clamped(k),
            },
        };
        let layout = ShimmerLayout::new(FdnLayout::new(2, MaxDecay::DEFAULT).unwrap());
        let mut network = Box::new(tutti_dsp::Network::new(RATE));
        layout.design_into(&controls.network, RATE, &mut network);
        let Some(NetworkStage::Fdn(f)) = network.stages.first() else {
            panic!("one FDN stage")
        };
        let t0 = f.lines[0].length.get() as usize;
        let ear = f.output.rows[0][0].get();
        let g0 = f.lines[0].absorption.gain(Band::Low).get();
        let want = [
            (t0, ear * f.input[0].get()),
            (2 * t0, ear * g0 * f.input[0].get() * (1.0 - k / 2.0)),
        ];
        let mut impulse = vec![0.0f32; 3_072];
        impulse[0] = 1.0;
        let landed = |n: &mut dyn tutti_graph::Node<f32>, what: &str| {
            let left = &render_chunked(n, std::slice::from_ref(&impulse), 1024)[0];
            for (at, amplitude) in want {
                assert!(
                    (f64::from(left[at]) - amplitude).abs() < 1e-4,
                    "{what}: frame {at} holds {} for {amplitude}",
                    left[at]
                );
            }
        };
        let silence = vec![vec![0.0f32; 1_024]];
        let mut n = crate::test_support::prepared(ShimmerNode::<f32>::from(
            ShimmerConfig::new().layout(layout).controls(controls),
        ));
        let cells = n.param_set();
        landed(&mut n, "fresh");
        assert!(cells.set(UnitParam::Feedback, 0.9));
        render_chunked(&mut n, &[vec![0.25f32; 2_048]], 1024);
        assert!(cells.set(UnitParam::Feedback, k));
        n.reset();
        landed(&mut n, "reset");
        render_chunked(&mut n, &[vec![0.25f32; 2_048]], 1024);
        n.prepare(&Prepare::new(
            RATE,
            tutti_buffers::MaxBlock::of(tutti_units::Samples::new(1024)),
        ));
        landed(&mut n, "re-prepare");
        n.reset();
        render_chunked(&mut n, &silence, 1024);
        let mut fork = n.fork_fresh();
        landed(&mut fork, "fork");
    }
}
