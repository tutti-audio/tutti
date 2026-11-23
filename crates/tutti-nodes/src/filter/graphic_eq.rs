//! [`GraphicEqNode`]: a graphic equalizer, a bank of cookbook peaking bells at the ISO 266
//! octave or third-octave centers run as one [`SosBank`](tutti_dsp::SosBank) cascade.

use tutti_buffers::{ChannelTopology, Float, InputOutput, MaxBlock};
use tutti_dsp::{BlockMismatch, DecayTime, GraphicBands, SimdLanes, SosCoefficients, TAIL_FLOOR};
use tutti_graph::{Cost, Shape};
use tutti_units::{Db, SampleRate, Samples, Tail};

use super::ramped::{RAMP_STEPS, RampedSos, filter_shape};
use super::{MAX_EQ_GAIN, eq_gain};
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// The controls of a [`GraphicEqNode`]: the gain of each band, lowest first. The bands past
/// the spacing's [`count`](GraphicBands::count) are not read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphicEqControls {
    /// The gain at each band's center, a boost above 0 dB and a cut below. Read within 24 dB.
    pub gains: [Db; GraphicBands::MAX_COUNT],
}

impl GraphicEqControls {
    /// Every band at 0 dB: a flat response.
    pub const FLAT: Self = Self {
        gains: [Db::UNITY; GraphicBands::MAX_COUNT],
    };
}

/// What a [`GraphicEqNode`] is made from: its input bus and its band spacing. It starts flat.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphicEqConfig {
    input: ChannelTopology,
    bands: GraphicBands,
}

impl GraphicEqConfig {
    /// A graphic equalizer on the bus `input` with the bands `bands`, every gain 0 dB.
    #[must_use]
    pub fn new(input: ChannelTopology, bands: GraphicBands) -> Self {
        Self { input, bands }
    }
}

/// A graphic equalizer of any width: one [`Cookbook::Peaking`](tutti_dsp::Cookbook::Peaking)
/// bell per ISO 266 band ([`GraphicBands`]), each of the band spacing's quality factor
/// ([`GraphicBands::q`]), cascaded and run as state-variable sections
/// ([`SosBank`](tutti_dsp::SosBank)).
///
/// The gain of band `i` is the live cell `UnitAddr::new(UnitParam::GainDb, i)`, read within
/// 24 dB. Every band is always in the cascade, a band at 0 dB being exactly a wire, so a gain
/// that moves ramps the coefficients along a line over the next 16 frames, designed on a
/// 16-frame grid, with the state kept. Neighbouring bands overlap, so a band's gain at its
/// center is its own only when the others are at 0 dB.
///
/// The tail is a finite ceiling, found in `prepare` from the slowest pole of any band over the 24 dB range
/// ([`GraphicBands::decay_ceiling`]). No latency. A [`KernelNode`]. Made from a [`GraphicEqConfig`]
/// (`GraphicEqNode::from`).
pub type GraphicEqNode<F> = KernelNode<GraphicEqKernel<F>>;

/// The graphic equalizer as a [`Kernel`].
#[derive(Clone, Debug)]
pub struct GraphicEqKernel<F: Float + SimdLanes> {
    input: ChannelTopology,
    bands: GraphicBands,
    sos: RampedSos<F>,
    rate: SampleRate,
    /// The ring-out over every gain at `rate`.
    ceiling: Tail,
}

impl<F: Float + SimdLanes> GraphicEqKernel<F> {
    /// A flat kernel on the bus `input`, at a placeholder 48 kHz until `prepare`.
    fn new(input: ChannelTopology, bands: GraphicBands) -> Self {
        let mut kernel = Self {
            input,
            bands,
            sos: RampedSos::new(input),
            rate: SampleRate::SR_48K,
            ceiling: Tail::Unknown,
        };
        let first = kernel.design(&GraphicEqControls::FLAT);
        kernel.sos.start(first);
        kernel.ceiling = kernel.ceiling_at();
        kernel
    }

    /// The ring-out over every gain: [`GraphicBands::decay_ceiling`] at [`MAX_EQ_GAIN`].
    fn ceiling_at(&self) -> Tail {
        match self.bands.decay_ceiling(MAX_EQ_GAIN, self.rate, TAIL_FLOOR) {
            Ok(DecayTime::Finite(t)) => Tail::Finite(Samples::new(
                (t.get() * self.rate.get()).ceil() as usize + 1,
            )),
            _ => Tail::Unbounded,
        }
    }

    /// The coefficients at `controls`, each gain read within 24 dB; the design in force when
    /// the design refuses. No allocation.
    fn design(&mut self, controls: &GraphicEqControls) -> SosCoefficients<F> {
        let (bands, rate) = (self.bands, self.rate);
        let gains = controls.gains.map(eq_gain);
        self.sos
            .design(|into| bands.design_into(&gains, rate, into))
    }
}

impl<F: Float + SimdLanes> From<GraphicEqConfig> for GraphicEqNode<F> {
    fn from(config: GraphicEqConfig) -> Self {
        Self::with_kernel(
            GraphicEqKernel::new(config.input, config.bands),
            GraphicEqSet {
                bands: config.bands,
            },
            GraphicEqControls::FLAT,
        )
    }
}

/// The graphic equalizer's controls: one gain per band of its spacing.
#[derive(Clone, Copy, Debug)]
pub struct GraphicEqSet {
    bands: GraphicBands,
}

control_set! {
    GraphicEqSet(s): GraphicEqControls = GraphicEqControls::FLAT;
    live: (s.bands.count());
    GainDb[0] => Db = gains[0] { whole },
    GainDb[1] => Db = gains[1] { whole },
    GainDb[2] => Db = gains[2] { whole },
    GainDb[3] => Db = gains[3] { whole },
    GainDb[4] => Db = gains[4] { whole },
    GainDb[5] => Db = gains[5] { whole },
    GainDb[6] => Db = gains[6] { whole },
    GainDb[7] => Db = gains[7] { whole },
    GainDb[8] => Db = gains[8] { whole },
    GainDb[9] => Db = gains[9] { whole },
    GainDb[10] => Db = gains[10] { whole },
    GainDb[11] => Db = gains[11] { whole },
    GainDb[12] => Db = gains[12] { whole },
    GainDb[13] => Db = gains[13] { whole },
    GainDb[14] => Db = gains[14] { whole },
    GainDb[15] => Db = gains[15] { whole },
    GainDb[16] => Db = gains[16] { whole },
    GainDb[17] => Db = gains[17] { whole },
    GainDb[18] => Db = gains[18] { whole },
    GainDb[19] => Db = gains[19] { whole },
    GainDb[20] => Db = gains[20] { whole },
    GainDb[21] => Db = gains[21] { whole },
    GainDb[22] => Db = gains[22] { whole },
    GainDb[23] => Db = gains[23] { whole },
    GainDb[24] => Db = gains[24] { whole },
    GainDb[25] => Db = gains[25] { whole },
    GainDb[26] => Db = gains[26] { whole },
    GainDb[27] => Db = gains[27] { whole },
    GainDb[28] => Db = gains[28] { whole },
    GainDb[29] => Db = gains[29] { whole },
    GainDb[30] => Db = gains[30] { whole };

    fn defaults(&self) -> GraphicEqControls {
        GraphicEqControls::FLAT
    }
}

impl<F: Float + SimdLanes> Kernel for GraphicEqKernel<F> {
    type Float = F;
    type Set = GraphicEqSet;

    const GRID: usize = RAMP_STEPS as usize;

    /// The input bus in and out, `Moderate` for the ten octave bands and `Heavy` for the
    /// thirty-one third-octave ones.
    fn shape(&self) -> Shape {
        let cost = match self.bands {
            GraphicBands::Octave => Cost::Moderate,
            _ => Cost::Heavy,
        };
        filter_shape(self.input).cost(cost)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.sos.at_rate(rate);
        let first = self.design(&GraphicEqControls::FLAT);
        self.sos.restart(first);
        self.ceiling = self.ceiling_at();
    }

    fn reset(&mut self) {
        self.sos.reset();
    }

    fn retarget(&mut self, controls: &GraphicEqControls) {
        let design = self.design(controls);
        self.sos.retarget(design);
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        _: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        self.sos.render(block)
    }

    /// The longest ring-out over the gains, found in `prepare`.
    fn tail(&self) -> Tail {
        self.ceiling
    }
}

#[cfg(test)]
mod tests {
    // These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.
    use super::*;
    use crate::filter::MAX_EQ_GAIN;
    use crate::filter::test_utils::{bin, gain, generate_sine, render};
    use crate::test_support::{RATE, assert_cell_ramps, prepared};
    use tutti_buffers::ChannelLayout;
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_units::{Hz, UnitAddr, UnitParam};

    fn mono() -> ChannelTopology {
        ChannelTopology::default_for(ChannelLayout::MONO).expect("mono is a bus")
    }

    fn band(i: usize) -> UnitAddr {
        UnitAddr::new(UnitParam::GainDb, u8::try_from(i).expect("a band index"))
    }

    /// A mono node of `bands` with the gains `gains` set before it is prepared.
    fn node_with(bands: GraphicBands, gains: &[(usize, f64)]) -> GraphicEqNode<f32> {
        let node = GraphicEqNode::from(GraphicEqConfig::new(mono(), bands));
        for &(i, db) in gains {
            assert!(node.param_set().set(band(i), db));
        }
        node
    }

    /// The design's magnitude at `f` for `gains`, read from the sections.
    fn designed(bands: GraphicBands, gains: &[(usize, f64)], f: f64) -> f64 {
        let mut all = [Db::UNITY; GraphicBands::MAX_COUNT];
        for &(i, db) in gains {
            all[i] = Db::new(db);
        }
        let sos = bands.design(&all, RATE).unwrap();
        let mut h = [tutti_dsp::Complex64::new(0.0, 0.0)];
        tutti_dsp::Lti::frequency_response(&sos, 0, &[Hz::new(f)], &mut h);
        h[0].norm()
    }

    /// A fork starts from the controls last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(node_with(GraphicBands::Octave, &[(2, 6.0)]));
    }

    /// The shape is the width in and out with one gain cell per band, and the cost of the bank.
    #[test]
    fn the_shape_declares_a_gain_per_band_and_the_banks_cost() {
        for (bands, cost, count) in [
            (GraphicBands::Octave, Cost::Moderate, 10),
            (GraphicBands::ThirdOctave, Cost::Heavy, 31),
        ] {
            let n = prepared(node_with(bands, &[(1, -6.0)]));
            let shape = n.shape();
            assert_eq!(
                (shape.audio_in().count(), shape.audio_out().count()),
                (1, 1)
            );
            assert_eq!(shape.cost, cost);
            let want: Vec<UnitAddr> = (0..count).map(band).collect();
            assert_eq!(shape.params.as_slice(), &want[..]);
            assert_eq!(n.param_base(1), Some(-6.0));
        }
    }

    /// The declared tail is finite and holds the measured ring-out of every gain set tried: the
    /// last frame of the `f64` impulse response (the sections in transposed direct form II, never
    /// the kernel's realization) at or above the floor under its own peak is inside it. The
    /// ceiling is within six times the largest measured, so it is a ring-out and not a bare cap
    /// (the pole alone ignores that a narrow bell's modes carry little of the response). The
    /// sets are the extremes, each band alone at the largest boost, and 16 pseudo-random gains.
    #[test]
    fn the_tail_ceiling_holds_the_measured_decay_of_every_gain_set() {
        let floor = tutti_dsp::TAIL_FLOOR.to_amplitude().get();
        let measured = |bands: GraphicBands, gains: &[Db; GraphicBands::MAX_COUNT], len: usize| {
            let sos = bands.design(gains, RATE).unwrap();
            let mut z = vec![[0.0f64; 2]; sos.sections().len()];
            let h: Vec<f64> = (0..len)
                .map(|n| {
                    let mut y = if n == 0 { 1.0 } else { 0.0 };
                    for (b, z) in sos.sections().iter().zip(&mut z) {
                        let x = y;
                        y = b.b0 * x + z[0];
                        z[0] = b.b1 * x - b.a1 * y + z[1];
                        z[1] = b.b2 * x - b.a2 * y;
                    }
                    y
                })
                .collect();
            let peak = h.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            h.iter().rposition(|v| v.abs() >= floor * peak).unwrap() + 1
        };
        for bands in [GraphicBands::Octave, GraphicBands::ThirdOctave] {
            let n = bands.count();
            let node = prepared(node_with(bands, &[]));
            let Tail::Finite(ceiling) = node.shape().tail else {
                panic!("{bands:?}: {:?}", node.shape().tail);
            };
            // Past the ceiling, so a response still above the floor there is seen.
            let len = ceiling.get() + 4_096;
            let mut sets: Vec<[Db; GraphicBands::MAX_COUNT]> = vec![
                [MAX_EQ_GAIN; GraphicBands::MAX_COUNT],
                [-MAX_EQ_GAIN; GraphicBands::MAX_COUNT],
                [Db::UNITY; GraphicBands::MAX_COUNT],
            ];
            sets.push(core::array::from_fn(|i| {
                Db::new(if i % 2 == 0 { 24.0 } else { -24.0 })
            }));
            for i in 0..n {
                let mut one = [Db::UNITY; GraphicBands::MAX_COUNT];
                one[i] = MAX_EQ_GAIN;
                sets.push(one);
            }
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            for _ in 0..16 {
                sets.push(core::array::from_fn(|_| {
                    seed = seed
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    Db::new(f64::from((seed >> 40) as u32 % 49) - 24.0)
                }));
            }
            let mut worst = 0;
            for set in &sets {
                let got = measured(bands, set, len);
                worst = worst.max(got);
                assert!(
                    got <= ceiling.get(),
                    "{bands:?}: {got} > {ceiling:?} at {:?}",
                    set.iter().take(n).map(|d| d.get()).collect::<Vec<_>>()
                );
            }
            assert!(
                ceiling.get() <= 6 * worst,
                "{bands:?}: {ceiling:?} against a measured {worst}"
            );
        }
    }

    /// A rendered sine through one raised band has the design's magnitude (to 1e-3) at the
    /// bins around the band's center, so the node runs the designed cascade.
    #[test]
    fn a_rendered_sine_has_the_designed_magnitude_around_a_raised_band() {
        for bands in [GraphicBands::Octave, GraphicBands::ThirdOctave] {
            let at = bands.count() / 2;
            let center = bands.centers()[at].get();
            let m = (center / bin(1)).round() as u32;
            for db in [12.0, -9.0] {
                for f in [m / 2, m - 3, m, m + 3, 2 * m].map(bin) {
                    let want = designed(bands, &[(at, db)], f);
                    let got = gain(node_with(bands, &[(at, db)]), f);
                    assert!(
                        (got - want).abs() < 1e-3 * want,
                        "{bands:?} {db} dB at {f}: {got} vs {want}"
                    );
                }
            }
        }
    }

    /// With every gain at 0 dB the render is the input to the `f32` rounding.
    #[test]
    fn zero_gains_pass_the_input_through() {
        for bands in [GraphicBands::Octave, GraphicBands::ThirdOctave] {
            for f in [bin(8), bin(171), bin(1_500), bin(3_900)] {
                let got = gain(node_with(bands, &[]), f);
                assert!((got - 1.0).abs() < 1e-4, "{bands:?} at {f}: {got}");
            }
        }
    }

    /// A gain past 24 dB reads as 24 dB, bit for bit.
    #[test]
    fn a_gain_out_of_range_is_read_as_the_end_of_it() {
        let input = generate_sine(bin(171), 48_000.0, 4_096);
        let at = |db: f64| {
            render(
                &mut prepared(node_with(GraphicBands::Octave, &[(5, db)])),
                &input,
            )
        };
        assert_eq!(at(90.0), at(24.0));
        assert_eq!(at(-90.0), at(-24.0));
        assert_ne!(at(24.0), at(-24.0));
    }

    /// A gain moved mid-sine ramps the same however the host cuts its blocks, and settles on
    /// the design.
    #[test]
    fn a_moved_gain_ramps_independently_of_the_blocks_and_settles_on_the_design() {
        let sine = generate_sine(bin(171), 48_000.0, 8_192);
        let make = || node_with(GraphicBands::Octave, &[]);
        let mut node = assert_cell_ramps(
            make,
            |n: &GraphicEqNode<f32>| assert!(n.param_set().set(band(5), 12.0)),
            &[&sine[..2_048]],
            &[&sine],
            "a band's gain",
        );
        let settled = render(&mut node, &generate_sine(bin(171), 48_000.0, 24_576));
        let want = designed(GraphicBands::Octave, &[(5, 12.0)], bin(171));
        let got = crate::filter::test_utils::level(&settled[16_384..]);
        assert!((got - want).abs() < 1e-3 * want, "{got} vs {want}");
    }
}
