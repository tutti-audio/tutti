//! [`CutFilterNode`]: a Butterworth, Linkwitz-Riley, Chebyshev or elliptic high-pass or
//! low-pass, run as a cascade of state-variable sections.

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, ChannelTopology, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, CutAlignment, CutDecayLadder, CutDesign, CutFilterControls, CutResponse,
    DecayTime, SimdLanes, SosCoefficients, TAIL_FLOOR,
};
use tutti_graph::{Cost, Shape};
use tutti_units::{Hz, SampleRate, Samples, Tail};

use super::filter_bus;
use super::ramped::{RAMP_STEPS, RampedSos};
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// What a [`CutFilterNode`] is made from: its width, response and alignment, and optionally
/// its cutoff.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutFilterConfig {
    width: ChannelLayout,
    response: CutResponse,
    alignment: CutAlignment,
    controls: CutFilterControls,
}

impl CutFilterConfig {
    /// A filter of `width` channels, `response` and `alignment`, at 1 kHz.
    #[must_use]
    pub fn new(
        width: impl Into<ChannelLayout>,
        response: CutResponse,
        alignment: CutAlignment,
    ) -> Self {
        Self {
            width: width.into(),
            response,
            alignment,
            controls: CutFilterControls::DEFAULT,
        }
    }

    /// The cutoff it starts at.
    #[must_use]
    pub fn cutoff(self, cutoff: Hz) -> Self {
        Self {
            controls: CutFilterControls { cutoff },
            ..self
        }
    }
}

/// A high-pass or low-pass of any width, a [`Butterworth`](tutti_dsp::Butterworth), [`LinkwitzRiley`](tutti_dsp::LinkwitzRiley), [`ChebyshevI`](tutti_dsp::ChebyshevI),
/// [`ChebyshevII`](tutti_dsp::ChebyshevII) or [`Elliptic`](tutti_dsp::Elliptic) design run
/// as a cascade of state-variable sections ([`SosBank`](tutti_dsp::SosBank)) in `f64`.
///
/// The cutoff is a live [`Hz`] cell (`UnitParam::Cutoff`), designed on a 16-frame grid
/// and reached along a line in the sections' coefficients over the next 16 frames with the
/// state kept, so an automated cutoff ramps and the cascade stays stable. The response and
/// the order are the node's build: another is another node.
///
/// The tail is the ring-out of the whole cascade at the lowest cutoff ([`Sos`](tutti_dsp::Sos)'s tail,
/// every pole together), the ceiling over every control. No latency. A [`KernelNode`]. Made
/// from a [`CutFilterConfig`] (`CutFilterNode::from`).
///
/// # Panics
///
/// `CutFilterNode::from` panics past 16 channels: a filter is one bus.
pub type CutFilterNode<F> = KernelNode<CutFilterKernel<F>>;

impl<F: Float + SimdLanes> From<CutFilterConfig> for CutFilterNode<F> {
    fn from(config: CutFilterConfig) -> Self {
        Self::with_kernel(
            CutFilterKernel::new(filter_bus(config.width), &config),
            CutFilterSet,
            config.controls,
        )
    }
}

/// The filter's control: the cutoff.
#[derive(Clone, Copy, Debug)]
pub struct CutFilterSet;

control_set! {
    CutFilterSet(_s): CutFilterControls = CutFilterControls::DEFAULT;
    live: (all);
    Cutoff => Hz = cutoff;

    fn defaults(&self) -> CutFilterControls {
        CutFilterControls::DEFAULT
    }
}

/// The filter as a [`Kernel`]: a [`SosBank`](tutti_dsp::SosBank) run with the ramp of the
/// coefficients every channel shares.
#[derive(Clone, Debug)]
pub struct CutFilterKernel<F: Float + SimdLanes> {
    input: ChannelTopology,
    design: CutDesign,
    sos: RampedSos<F>,
    rate: SampleRate,
    /// The ring-out at each octave of cutoff, found in `prepare`; `None` before it.
    ladder: Option<CutDecayLadder>,
}

impl<F: Float + SimdLanes> CutFilterKernel<F> {
    /// A filter on the bus `input`, its output where it is; at a placeholder 48 kHz until
    /// `prepare`, which an unprepared node renders nothing before.
    fn new(input: ChannelTopology, config: &CutFilterConfig) -> Self {
        let mut kernel = Self {
            input,
            design: CutDesign::new(config.response, config.alignment),
            sos: RampedSos::new(input),
            rate: SampleRate::SR_48K,
            ladder: None,
        };
        let first = kernel.design(&CutFilterControls::DEFAULT);
        kernel.sos.start(first);
        kernel
    }

    /// The coefficients of the design at `controls`; the cascade as it is for a cutoff that
    /// is not a number. No allocation.
    fn design(&mut self, controls: &CutFilterControls) -> SosCoefficients<F> {
        let (design, rate) = (self.design, self.rate);
        self.sos
            .design(|into| design.design_into(controls, rate, into))
    }

    /// `time` in frames at the prepared rate.
    fn tail_in(&self, time: DecayTime) -> Tail {
        match time {
            DecayTime::Finite(s) => {
                Tail::Finite(Samples::new((s.get() * self.rate.get()).ceil() as usize))
            }
            DecayTime::Unbounded => Tail::Unbounded,
        }
    }
}

impl<F: Float + SimdLanes> Kernel for CutFilterKernel<F> {
    type Float = F;
    type Set = CutFilterSet;

    const GRID: usize = RAMP_STEPS as usize;

    fn shape(&self) -> Shape {
        Shape::through(self.input).cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.sos.at_rate(rate);
        let first = self.design(&CutFilterControls::DEFAULT);
        self.sos.restart(first);
        self.ladder = Some(self.design.decay_ladder(rate, TAIL_FLOOR));
    }

    fn reset(&mut self) {
        self.sos.reset();
    }

    fn retarget(&mut self, controls: &CutFilterControls) {
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

    /// The ring-out at the lowest cutoff: the longest over every control.
    fn tail(&self) -> Tail {
        self.ladder
            .map_or(Tail::Unknown, |ladder| self.tail_in(ladder.ceiling()))
    }

    /// The ring-out found for the highest octave at or below the cutoff in force
    /// ([`CutDecayLadder::at`]): a lower cutoff rings longer, so it errs long. No simulation.
    fn tail_of(&mut self, controls: &CutFilterControls) -> Tail {
        self.ladder.map_or(Tail::Unknown, |ladder| {
            self.tail_in(ladder.at(controls, self.rate))
        })
    }
}

#[cfg(test)]
mod tests {
    // These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.
    use core::f64::consts::PI;

    use super::*;
    use crate::filter::test_utils::{generate_sine, rms};
    use crate::test_support::{RATE, assert_cell_ramps, drive_block, prepared};
    use tutti_dsp::{
        ButterworthOrder, Elliptic, EllipticTolerances, PassbandRipple, StopbandAttenuation,
    };

    use tutti_graph::ParamNode;
    use tutti_units::{Db, UnitParam};

    const LP: CutResponse = CutResponse::LowPass;
    const HP: CutResponse = CutResponse::HighPass;

    fn bw(order: u8) -> CutAlignment {
        CutAlignment::Butterworth(ButterworthOrder::of(order))
    }

    /// A Linkwitz-Riley alignment of twice `half` the order.
    fn lr(half: u8) -> CutAlignment {
        CutAlignment::LinkwitzRiley(ButterworthOrder::of(half))
    }

    fn node(config: CutFilterConfig) -> CutFilterNode<f32> {
        prepared(CutFilterNode::from(config))
    }

    /// The frequency of bin `m` of the 8192-frame window the levels are read over: a sine
    /// there has whole cycles in it, so its RMS is exact.
    fn bin(m: u32) -> f64 {
        RATE.get() * f64::from(m) / 8192.0
    }

    /// The amplitude of a sine whose samples are `x`.
    fn level(x: &[f32]) -> f64 {
        f64::from(rms(x)) * 2f64.sqrt()
    }

    /// `input` through `node` in blocks of 1024.
    fn render(node: &mut CutFilterNode<f32>, input: &[f32]) -> Vec<f32> {
        input
            .chunks(1024)
            .flat_map(|c| drive_block(node, &[c]).remove(0))
            .collect()
    }

    /// The RMS of the settled half of a unit sine at `f` through the filter.
    fn gain(config: CutFilterConfig, f: f64) -> f64 {
        let out = render(&mut node(config), &generate_sine(f, 48_000.0, 16_384));
        level(&out[8_192..])
    }

    /// The analog Butterworth magnitude at the bilinear image of `f`.
    fn analog(response: CutResponse, order: u8, fc: f64, f: f64) -> f64 {
        let t = libm::tan(PI * f / RATE.get()) / libm::tan(PI * fc / RATE.get());
        let ratio = if response == LP { t } else { 1.0 / t };
        1.0 / (1.0 + ratio.powi(2 * i32::from(order))).sqrt()
    }

    /// The analog Chebyshev I magnitude at the bilinear image of `f`: `1 / sqrt(1 + e^2
    /// T_N(w)^2)` with `w` the prewarped frequency over the prewarped cutoff and `T_N` from
    /// its `cos` and `cosh` forms.
    fn chebyshev_one(response: CutResponse, order: u8, ripple: f64, fc: f64, f: f64) -> f64 {
        let t = libm::tan(PI * f / RATE.get()) / libm::tan(PI * fc / RATE.get());
        let w = if response == LP { t } else { 1.0 / t };
        let n = f64::from(order);
        let poly = if w <= 1.0 {
            libm::cos(n * libm::acos(w))
        } else {
            libm::cosh(n * libm::acosh(w))
        };
        let e2 = libm::pow(10.0, ripple / 10.0) - 1.0;
        1.0 / libm::sqrt(1.0 + e2 * poly * poly)
    }

    /// A settled rendered sine has the analog Chebyshev I magnitude at the prewarped
    /// frequency, in both responses and across the passband edge.
    #[test]
    fn a_rendered_sine_has_the_chebyshev_one_magnitude() {
        for response in [LP, HP] {
            for order in [3u8, 4, 7] {
                let ripple = 1.5;
                let alignment = CutAlignment::ChebyshevI {
                    order: ButterworthOrder::of(order),
                    ripple: PassbandRipple::new(Db::new(ripple)).unwrap(),
                };
                for f in [51, 137, 171, 256, 683].map(bin) {
                    let want = chebyshev_one(response, order, ripple, 1000.0, f);
                    let got = gain(CutFilterConfig::new(1u16, response, alignment), f);
                    assert!(
                        (got - want).abs() < 5e-7 + 1e-4 * want,
                        "{response:?} {order} at {f}: {got} vs {want}"
                    );
                }
            }
        }
    }

    /// A Chebyshev II low-pass stays under `-attenuation` dB from its cutoff up, and an
    /// elliptic one from its stopband edge up and within `ripple` of unity below the cutoff.
    #[test]
    fn rendered_sines_meet_the_stopband_and_passband_bounds() {
        let (ripple, attenuation) = (1.0, 50.0);
        let order = ButterworthOrder::of(6);
        let (ripple_db, attenuation_db) = (
            PassbandRipple::new(Db::new(ripple)).unwrap(),
            StopbandAttenuation::new(Db::new(attenuation)).unwrap(),
        );
        let limit = libm::pow(10.0, -attenuation / 20.0);
        let two = CutAlignment::ChebyshevII {
            order,
            attenuation: attenuation_db,
        };
        let tolerances = EllipticTolerances::new(ripple_db, attenuation_db).unwrap();
        let elliptic = CutAlignment::Elliptic { order, tolerances };
        let edge = Elliptic::new(LP, order, tolerances)
            .stopband_edge(Hz::new(1000.0), RATE)
            .unwrap()
            .get();
        for f in [bin(171), bin(300), bin(600)] {
            let got = gain(CutFilterConfig::new(1u16, LP, two), f);
            assert!(got <= limit * 1.01, "Chebyshev II at {f}: {got}");
        }
        for f in [edge * 1.0001, edge * 1.5, edge * 3.0] {
            let got = gain(CutFilterConfig::new(1u16, LP, elliptic), f);
            assert!(got <= limit * 1.01, "elliptic at {f}: {got}");
        }
        let floor = libm::pow(10.0, -ripple / 20.0);
        for f in [bin(17), bin(51), bin(120), bin(160)] {
            let got = gain(CutFilterConfig::new(1u16, LP, elliptic), f);
            assert!(
                got <= 1.0 + 1e-4 && got >= floor - 1e-4,
                "elliptic at {f}: {got}"
            );
        }
    }

    /// A settled rendered sine has the analog Butterworth magnitude at the prewarped
    /// frequency, for several orders and both responses.
    #[test]
    fn a_rendered_sine_has_the_butterworth_magnitude() {
        for response in [LP, HP] {
            for order in [1u8, 2, 3, 5, 8, 16] {
                for f in [51, 137, 171, 256, 683].map(bin) {
                    let want = analog(response, order, 1000.0, f);
                    let got = gain(CutFilterConfig::new(1u16, response, bw(order)), f);
                    assert!(
                        (got - want).abs() < 5e-7 + 1e-4 * want,
                        "{response:?} {order} at {f}: {got} vs {want}"
                    );
                }
            }
        }
    }

    /// Linkwitz-Riley is `-6 dB` at the cutoff and Butterworth squared elsewhere, and its
    /// low-pass and high-pass sum to unit magnitude (their difference at an order of 2 mod 4).
    #[test]
    fn linkwitz_riley_is_6_db_down_and_the_pair_sums_to_an_allpass() {
        for order in [2u8, 4, 6, 8, 16] {
            let lr = |response| {
                CutFilterConfig::new(1u16, response, lr(order / 2)).cutoff(Hz::new(bin(171)))
            };
            assert!((gain(lr(LP), bin(171)) - 0.5).abs() < 1e-4, "LR{order}");
            for f in [34, 120, 171, 512].map(bin) {
                let want = analog(LP, order / 2, bin(171), f).powi(2);
                assert!((gain(lr(LP), f) - want).abs() < 5e-7 + 1e-4 * want);
                let input = generate_sine(f, 48_000.0, 16_384);
                let lows = render(&mut node(lr(LP)), &input);
                let highs = render(&mut node(lr(HP)), &input);
                let sign = if order % 4 == 0 { 1.0 } else { -1.0 };
                let sum: Vec<f32> = lows.iter().zip(&highs).map(|(l, h)| l + sign * h).collect();
                assert!(
                    (level(&sum[8_192..]) - 1.0).abs() < 1e-4,
                    "LR{order} at {f}"
                );
            }
        }
    }

    /// A cutoff moved across the decades mid-sine stays finite and bounded, lands on the
    /// designed response, and renders the same however the host cuts its blocks.
    #[test]
    fn a_cutoff_ramp_stays_finite_and_settles_on_the_design() {
        let make = || CutFilterNode::from(CutFilterConfig::new(1u16, LP, lr(8)));
        let sine = generate_sine(bin(256), 48_000.0, 8_192);
        let (mut node, out) = {
            let n = assert_cell_ramps(
                make,
                |n: &CutFilterNode<f32>| assert!(n.param_set().set(UnitParam::Cutoff, 4_000.0)),
                &[&sine[..2_048]],
                &[&sine],
                "cutoff",
            );
            (
                n,
                crate::test_support::render_change(make, |_| {}, &[&sine], &[&sine], 64).1,
            )
        };
        assert!(out[0].iter().all(|v| v.is_finite() && v.abs() < 2.0));
        // Down again, then up: every block stays finite.
        for hz in [30.0, 12_000.0, 60.0, 9_000.0] {
            assert!(node.param_set().set(UnitParam::Cutoff, hz));
            assert!(
                render(&mut node, &sine)
                    .iter()
                    .all(|v| v.is_finite() && v.abs() < 2.0)
            );
        }
        assert!(node.param_set().set(UnitParam::Cutoff, 4_000.0));
        let settled = render(&mut node, &generate_sine(bin(256), 48_000.0, 24_576));
        let want = analog(LP, 8, 4_000.0, bin(256)).powi(2);
        assert!((level(&settled[16_384..]) - want).abs() < 1e-4);
    }

    /// An odd order has a first-order section: a cutoff swept across the decades stays
    /// bounded and the response settles on the design.
    #[test]
    fn an_odd_order_sweep_stays_bounded_and_settles() {
        let sine = generate_sine(bin(256), 48_000.0, 8_192);
        for response in [LP, HP] {
            for order in [3u8, 5] {
                let mut node = node(CutFilterConfig::new(1u16, response, bw(order)));
                for hz in [30.0, 12_000.0, 60.0, 9_000.0, 4_000.0] {
                    assert!(node.param_set().set(UnitParam::Cutoff, hz));
                    let out = render(&mut node, &sine);
                    let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                    // The state of one cutoff meets the next's: a transient of a few times the input.
                    assert!(peak < 4.0, "{response:?} {order} at {hz}: {peak}");
                }
                let settled = render(&mut node, &generate_sine(bin(256), 48_000.0, 24_576));
                let want = analog(response, order, 4_000.0, bin(256));
                assert!(
                    (level(&settled[16_384..]) - want).abs() < 1e-4,
                    "{response:?} {order}"
                );
            }
        }
    }

    /// The ceiling is the lowest cutoff's ring-out, a finite tail that narrows (never grows)
    /// as the cutoff in force rises; the ladder's figures are `tutti_dsp::CutDesign`'s.
    #[test]
    fn the_tail_narrows_with_the_cutoff_in_force() {
        let frames = |t: Tail| match t {
            Tail::Finite(s) => s.get(),
            other => panic!("{other:?}"),
        };
        for response in [LP, HP] {
            let config = CutFilterConfig::new(1u16, response, lr(8));
            let mut kernel = CutFilterKernel::<f64>::new(filter_bus(1u16.into()), &config);
            assert_eq!(kernel.tail(), Tail::Unknown);
            kernel.prepare(RATE, MaxBlock::of(Samples::new(64)));
            let at = |kernel: &mut CutFilterKernel<f64>, hz: f64| {
                kernel.tail_of(&CutFilterControls {
                    cutoff: Hz::new(hz),
                })
            };
            assert_eq!(kernel.tail(), at(&mut kernel, 10.0));
            let mut last = usize::MAX;
            for cutoff in [14.0f64, 110.0, 1500.0, 3000.0] {
                let reported = frames(at(&mut kernel, cutoff));
                assert!(reported <= last, "{response:?} {cutoff} does not narrow");
                last = reported;
            }
            assert!(frames(kernel.tail()) > 10 * last);
        }
    }
}
