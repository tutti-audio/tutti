//! Parametric EQ band: an SVF with a mono layout and an EQ band's constructor.

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_graph::{
    Cx, HasParam, HostValues, Initials, IntoNode, Io, Node, NodeConfig, NodeParts, ParamNode,
    ParamSet, Prepare, Shape, Status,
};
use tutti_units::{Db, Hz, Q};

use super::svf::{SvfFilterConfig, SvfFilterNode};
use tutti_dsp::{SimdLanes, SvfType};

/// Parametric EQ band: an [`SvfFilterNode`] at one channel. 1 input, 1
/// output.
///
/// Cutoff, [`Q`] and gain are the inner SVF's live params, reached by
/// [`UnitParam`](tutti_units::UnitParam) through the [`ParamSet`] it is
/// inserted with. Stack several to build a parametric EQ, one band
/// per [`SvfType`]. A band's bypass is not here: it is `NodeMode::Bypassed`
/// (`Graph::set_mode`), the graph's for every node: a crossfade of
/// [`DECLICK`](tutti_graph::DECLICK), bit-exact once it is over.
///
/// Made from an [`EqBandConfig`] (`EqBandNode::from`). `F` is the float of the graph it runs in.
#[derive(Clone, Debug)]
pub struct EqBandNode<F: Float + SimdLanes> {
    svf: SvfFilterNode<F>,
}

/// What an [`EqBandNode`] is made from: the response of its band and its controls' start.
///
/// A config starts as an [`SvfFilterConfig`] of one channel does: 1 kHz, Butterworth, 0 dB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqBandConfig {
    svf: SvfFilterConfig,
}

impl EqBandConfig {
    /// A band of the response `filter_type`.
    #[must_use]
    pub fn new(filter_type: SvfType) -> Self {
        Self {
            svf: SvfFilterConfig::new(ChannelLayout::MONO, filter_type),
        }
    }

    /// The cutoff, centre or corner at the start.
    #[must_use]
    pub fn freq(mut self, frequency: Hz) -> Self {
        self.svf = self.svf.freq(frequency);
        self
    }

    /// The resonance at the start.
    #[must_use]
    pub fn q(mut self, q: Q) -> Self {
        self.svf = self.svf.q(q);
        self
    }

    /// The gain at the start, read only by [`Bell`](SvfType::Bell),
    /// [`LowShelf`](SvfType::LowShelf) and [`HighShelf`](SvfType::HighShelf), the usual
    /// EQ-band types. On the others it is stored and ignored.
    #[must_use]
    pub fn gain(mut self, gain: Db) -> Self {
        self.svf = self.svf.gain(gain);
        self
    }
}

impl<F: Float + SimdLanes> From<EqBandConfig> for EqBandNode<F> {
    fn from(config: EqBandConfig) -> Self {
        Self {
            svf: SvfFilterNode::from(config.svf),
        }
    }
}

/// The band's values and structure are its filter's.
impl<F: Float + SimdLanes> NodeConfig<F> for EqBandConfig {
    type Node = EqBandNode<F>;

    fn build(&self, _: &HostValues) -> EqBandNode<F> {
        EqBandNode::from(*self)
    }

    fn same_structure(&self, other: &Self) -> bool {
        NodeConfig::<F>::same_structure(&self.svf, &other.svf)
    }

    fn initials(&self) -> Initials {
        NodeConfig::<F>::initials(&self.svf)
    }
}

impl<F: Float + SimdLanes> Node<F> for EqBandNode<F> {
    /// Mono in and out. Unlike the bare filter it declares no modulatable
    /// params: a band is set, not swept.
    /// Its tail is the inner filter's: [`Tail::Unknown`](tutti_units::Tail::Unknown).
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .cost(tutti_graph::Cost::Moderate)
            .tail(self.svf.shape().tail)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.svf.prepare(p);
    }

    fn process(&mut self, cx: &Cx<'_>, io: Io<'_, F>) -> Status {
        self.svf.process(cx, io)
    }

    fn reset(&mut self) {
        Node::<F>::reset(&mut self.svf);
    }
}

impl<F: Float + SimdLanes> ParamNode<F> for EqBandNode<F> {
    /// The inner filter's: cutoff, Q and gain.
    fn param_set(&self) -> ParamSet {
        self.svf.param_set()
    }

    fn fork_fresh(&self) -> Self {
        Self {
            svf: self.svf.fork_fresh(),
        }
    }
}

/// A band has the params of the filter inside it, in its units.
impl<F: Float + SimdLanes, const P: u16> HasParam<P> for EqBandNode<F>
where
    SvfFilterNode<F>: HasParam<P>,
{
    type Unit = <SvfFilterNode<F> as HasParam<P>>::Unit;
}

/// Inserted as the inner filter is: its [`ParamSet`] as its controls, and a
/// fork from the values last set through it.
impl<F: Float + SimdLanes> IntoNode<F> for EqBandNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::test_utils::{generate_sine, rms};
    use tutti_buffers::Planes;
    use tutti_graph::contract::{drive, prepared};
    use tutti_units::SampleRate;

    /// `eq`, prepared at 44.1 kHz, over `input` in one block.
    fn render(eq: EqBandNode<f32>, input: &[f32]) -> Vec<f32> {
        let rate = SampleRate::SR_44K1;
        let mut eq = prepared(eq, rate, input.len());
        drive(&mut eq, rate, Planes::new(&[input]).unwrap(), &[]).remove(0)
    }

    /// The band declares the inner filter's tail, not the shape default.
    #[test]
    fn the_band_rings_on_as_its_filter_does() {
        let band = EqBandNode::<f32>::from(
            EqBandConfig::new(SvfType::Bell)
                .freq(Hz::new(1000.0))
                .q(Q::new(8.0))
                .gain(Db::new(12.0)),
        );
        assert_eq!(band.shape().tail, tutti_units::Tail::Unknown);
    }

    #[test]
    fn a_bell_boost_raises_the_level_at_its_centre() {
        let sine = generate_sine(1000.0, 44100.0, 4096);
        let out = render(
            EqBandNode::<f32>::from(
                EqBandConfig::new(SvfType::Bell)
                    .freq(Hz::new(1000.0))
                    .q(Q::new(1.0))
                    .gain(Db::new(12.0)),
            ),
            &sine,
        );
        let rms_in = rms(&sine[512..]);
        let rms_out = rms(&out[512..]);
        assert!(
            rms_out > rms_in,
            "Bell boost should increase level: in={rms_in}, out={rms_out}"
        );
    }

    #[test]
    fn the_filter_type_selects_the_response() {
        let high = generate_sine(5000.0, 44100.0, 2048);
        let out_lp = render(
            EqBandNode::<f32>::from(
                EqBandConfig::new(SvfType::LowPass)
                    .freq(Hz::new(500.0))
                    .q(Q::new(0.707))
                    .gain(Db::new(0.0)),
            ),
            &high,
        );
        let out_hp = render(
            EqBandNode::<f32>::from(
                EqBandConfig::new(SvfType::HighPass)
                    .freq(Hz::new(500.0))
                    .q(Q::new(0.707))
                    .gain(Db::new(0.0)),
            ),
            &high,
        );
        let rms_lp = rms(&out_lp[512..]);
        let rms_hp = rms(&out_hp[512..]);
        assert!(
            rms_hp > rms_lp * 2.0,
            "HP should pass more high freq than LP: lp={rms_lp}, hp={rms_hp}"
        );
    }

    /// In an `f64` graph a low-pass band's steady response to a 29-bit constant is the constant
    /// (unity at DC) to `2^-45`.
    #[test]
    fn in_an_f64_graph_a_low_pass_band_passes_a_wide_constant_at_unity() {
        use crate::test_support::{F64_TOLERANCE, RATE, render_f64};
        const DC: f64 = 0x2000_0001 as f64 / 2_147_483_648.0;
        let x = vec![DC; 4_096];
        let node = EqBandNode::<f64>::from(
            EqBandConfig::new(SvfType::LowPass)
                .freq(Hz::new(1_000.0))
                .q(Q::new(0.707))
                .gain(Db::new(0.0)),
        );
        let settled = render_f64(node, RATE, &[&x], 4_096)[0][4_095];
        assert!(
            (settled - DC).abs() <= F64_TOLERANCE,
            "{settled} against {DC}"
        );
    }
}
