//! [`GraphicEqNode`]: a graphic equalizer, a bank of cookbook peaking bells at the ISO 266
//! octave or third-octave centers run as one [`SosBank`](tutti_dsp::SosBank) cascade.

use tutti_buffers::{BusTopology, Float, MaxBlock, Planes, PlanesMut};
use tutti_dsp::{GraphicBands, SimdLanes, SosCoefficients};
use tutti_graph::{Cost, Shape};
use tutti_units::{Db, SampleRate, Tail};

use super::eq_gain;
use super::ramped::{RAMP_STEPS, RampedSos, filter_shape};
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
    input: BusTopology,
    bands: GraphicBands,
}

impl GraphicEqConfig {
    /// A graphic equalizer on the bus `input` with the bands `bands`, every gain 0 dB.
    #[must_use]
    pub fn new(input: BusTopology, bands: GraphicBands) -> Self {
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
/// The tail is unbounded: no closed-form bound on the ring-out of the cascade over every
/// gain is known. No latency. A [`KernelNode`]. Made from a [`GraphicEqConfig`]
/// (`GraphicEqNode::from`).
pub type GraphicEqNode<F> = KernelNode<GraphicEqKernel<F>>;

/// The graphic equalizer as a [`Kernel`].
#[derive(Clone, Debug)]
pub struct GraphicEqKernel<F: Float + SimdLanes> {
    input: BusTopology,
    bands: GraphicBands,
    sos: RampedSos<F>,
    rate: SampleRate,
}

impl<F: Float + SimdLanes> GraphicEqKernel<F> {
    /// A flat kernel on the bus `input`, at a placeholder 48 kHz until `prepare`.
    fn new(input: BusTopology, bands: GraphicBands) -> Self {
        let mut kernel = Self {
            input,
            bands,
            sos: RampedSos::new(input),
            rate: SampleRate::SR_48K,
        };
        let first = kernel.design(&GraphicEqControls::FLAT);
        kernel.sos.start(first);
        kernel
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
        filter_shape(self.input).with_cost(cost)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.sos.at_rate(rate);
        let first = self.design(&GraphicEqControls::FLAT);
        self.sos.restart(first);
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
        input: Planes<'_, F>,
        _: ControlSignals<'_, F>,
        output: PlanesMut<'_, '_, F>,
    ) {
        self.sos.render(input, output);
    }

    fn tail(&self) -> Tail {
        Tail::Unbounded
    }
}

#[cfg(test)]
mod tests {
    // These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.
    use super::*;
    use crate::filter::test_utils::{bin, gain, generate_sine, render};
    use crate::test_support::{RATE, assert_cell_ramps, prepared};
    use tutti_buffers::ChannelLayout;
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_units::{Hz, UnitAddr, UnitParam};

    fn mono() -> BusTopology {
        BusTopology::default_for(ChannelLayout::MONO).expect("mono is a bus")
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

    /// The shape is the width in and out with one gain cell per band, the cost of the bank, and
    /// an unbounded tail.
    #[test]
    fn the_shape_declares_a_gain_per_band_and_an_unbounded_tail() {
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
            assert_eq!(shape.tail, Tail::Unbounded);
            let want: Vec<UnitAddr> = (0..count).map(band).collect();
            assert_eq!(shape.params.as_slice(), &want[..]);
            assert_eq!(n.param_base(1), Some(-6.0));
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
