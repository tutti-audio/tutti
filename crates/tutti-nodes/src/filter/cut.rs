//! [`CutFilterNode`]: a Butterworth, Linkwitz-Riley, Chebyshev or elliptic high-pass or
//! low-pass, run as a cascade of state-variable sections.

use tutti_buffers::Float;
use tutti_buffers::{BusTopology, ChannelLayout, InputOutput, MaxBlock};
use tutti_dsp::{
    BlockMismatch, Butterworth, ButterworthOrder, ChebyshevI, ChebyshevII, CutResponse, DecayTime,
    Elliptic, EllipticTolerances, LinkwitzRiley, PassbandRipple, SimdLanes, SosCoefficients,
    StopbandAttenuation, System, TAIL_FLOOR,
};
use tutti_graph::Shape;
use tutti_units::{Hz, SampleRate, Samples, Tail};

use super::filter_bus;
use super::ramped::{RAMP_STEPS, RampedSos, filter_shape};
use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// The lowest cutoff a filter takes: 10 Hz.
pub const MIN_CUT_CUTOFF: Hz = Hz::new(10.0);

/// The octaves from [`MIN_CUT_CUTOFF`] a cutoff below 192 kHz's Nyquist can sit in.
const LADDER: usize = 14;

/// The control of a [`CutFilterNode`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutFilterControls {
    /// The cutoff: `-3 dB` for a Butterworth filter, `-6 dB` for a Linkwitz-Riley one, the
    /// passband edge of a Chebyshev I or elliptic one and the stopband edge of a Chebyshev II
    /// one. Read
    /// within [`MIN_CUT_CUTOFF`] and 0.99 of the Nyquist.
    pub cutoff: Hz,
}

impl CutFilterControls {
    /// 1 kHz.
    pub const DEFAULT: Self = Self {
        cutoff: Hz::new(1_000.0),
    };
}

/// The family and order of a [`CutFilterNode`]'s design.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum CutAlignment {
    /// A [`Butterworth`] filter of this order: `6 * order` dB per octave, `-3 dB` at the cutoff.
    Butterworth(ButterworthOrder),
    /// A [`LinkwitzRiley`] filter of twice this order (the Butterworth order it squares):
    /// `12 * order` dB per octave, `-6 dB` at the cutoff.
    LinkwitzRiley(ButterworthOrder),
    /// A [`ChebyshevI`] filter: equiripple in the passband, `-ripple` dB at the cutoff.
    ChebyshevI {
        /// The order.
        order: ButterworthOrder,
        /// The passband ripple.
        ripple: PassbandRipple,
    },
    /// A [`ChebyshevII`] filter: equiripple in the stopband, `-attenuation` dB at the cutoff,
    /// the stopband edge.
    ChebyshevII {
        /// The order.
        order: ButterworthOrder,
        /// The stopband attenuation.
        attenuation: StopbandAttenuation,
    },
    /// An [`Elliptic`] filter: equiripple in both bands, `-ripple` dB at the cutoff, the
    /// passband edge.
    Elliptic {
        /// The order.
        order: ButterworthOrder,
        /// The passband ripple and the stopband attenuation.
        tolerances: EllipticTolerances,
    },
}

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

/// A high-pass or low-pass of any width, a [`Butterworth`], [`LinkwitzRiley`], [`ChebyshevI`],
/// [`ChebyshevII`] or [`Elliptic`] design run
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
    input: BusTopology,
    response: CutResponse,
    alignment: CutAlignment,
    sos: RampedSos<F>,
    rate: SampleRate,
    /// The tail at each octave from [`MIN_CUT_CUTOFF`] up (`LADDER` rungs at most), found
    /// in `prepare`; the rungs past the Nyquist's are unused.
    ladder: [Tail; LADDER],
}

impl<F: Float + SimdLanes> CutFilterKernel<F> {
    /// A filter on the bus `input`, its output where it is; at a placeholder 48 kHz until
    /// `prepare`, which an unprepared node renders nothing before.
    fn new(input: BusTopology, config: &CutFilterConfig) -> Self {
        let mut kernel = Self {
            input,
            response: config.response,
            alignment: config.alignment,
            sos: RampedSos::new(input),
            rate: SampleRate::SR_48K,
            ladder: [Tail::Unknown; LADDER],
        };
        let first = kernel.design(CutFilterControls::DEFAULT.cutoff);
        kernel.sos.start(first);
        kernel
    }

    /// The coefficients of the design at `cutoff`, read within [`MIN_CUT_CUTOFF`] and the
    /// Nyquist; the cascade as it is for a cutoff that is not a number. No allocation.
    fn design(&mut self, cutoff: Hz) -> SosCoefficients<F> {
        let top = self.rate.nyquist_scaled(0.99).get();
        let cutoff = Hz::new(cutoff.get().clamp(MIN_CUT_CUTOFF.get(), top));
        let (response, alignment, rate) = (self.response, self.alignment, self.rate);
        self.sos.design(|into| match alignment {
            CutAlignment::Butterworth(order) => {
                Butterworth::new(response, order).design_into(cutoff, rate, into)
            }
            CutAlignment::LinkwitzRiley(half) => {
                LinkwitzRiley::new(response, half).design_into(cutoff, rate, into)
            }
            CutAlignment::ChebyshevI { order, ripple } => {
                ChebyshevI::new(response, order, ripple).design_into(cutoff, rate, into)
            }
            CutAlignment::ChebyshevII { order, attenuation } => {
                ChebyshevII::new(response, order, attenuation).design_into(cutoff, rate, into)
            }
            CutAlignment::Elliptic { order, tolerances } => {
                Elliptic::new(response, order, tolerances).design_into(cutoff, rate, into)
            }
        })
    }
}

/// The cutoff of ladder rung `rung`: `rung` octaves above [`MIN_CUT_CUTOFF`].
fn rung_cutoff(rung: usize) -> Hz {
    MIN_CUT_CUTOFF * f64::from(1u32 << rung)
}

impl<F: Float + SimdLanes> Kernel for CutFilterKernel<F> {
    type Float = F;
    type Set = CutFilterSet;

    const GRID: usize = RAMP_STEPS as usize;

    fn shape(&self) -> Shape {
        filter_shape(self.input)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.sos.at_rate(rate);
        let first = self.design(CutFilterControls::DEFAULT.cutoff);
        self.sos.restart(first);
        let top = self.rate.nyquist_scaled(0.99);
        for rung in 0..LADDER {
            let _ = self.design(rung_cutoff(rung).min(top));
            self.ladder[rung] = match self.sos.scratch().tail(TAIL_FLOOR) {
                DecayTime::Finite(s) => {
                    Tail::Finite(Samples::new((s.get() * rate.get()).ceil() as usize))
                }
                DecayTime::Unbounded => Tail::Unbounded,
            };
        }
    }

    fn reset(&mut self) {
        self.sos.reset();
    }

    fn retarget(&mut self, controls: &CutFilterControls) {
        let design = self.design(controls.cutoff);
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
        self.ladder[0]
    }

    /// The tail found for the highest octave from [`MIN_CUT_CUTOFF`] at or below the
    /// cutoff in force: a lower cutoff rings longer, so it errs long. No simulation.
    fn tail_of(&mut self, controls: &CutFilterControls) -> Tail {
        let top = self.rate.nyquist_scaled(0.99);
        let cutoff = controls.cutoff.clamp(MIN_CUT_CUTOFF, top);
        let octave = (1..LADDER)
            .take_while(|&rung| rung_cutoff(rung) <= cutoff)
            .count();
        self.ladder[octave]
    }
}

#[cfg(test)]
mod tests {
    // These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.
    use core::f64::consts::PI;

    use super::*;
    use crate::filter::test_utils::{generate_sine, rms};
    use crate::test_support::{RATE, assert_cell_ramps, drive_block, prepared};
    use tutti_dsp::TAIL_FLOOR;
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Cost, Node, ParamNode};
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

    /// A fork starts from the cutoff last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(CutFilterNode::from(CutFilterConfig::new(2u16, LP, bw(4))));
    }

    /// The shape is the width in and out, `Moderate`, declares the cutoff and a finite
    /// tail once prepared; the cutoff starts at the configured one.
    #[test]
    fn the_shape_declares_the_cutoff_and_a_finite_tail() {
        let n = node(CutFilterConfig::new(3u16, HP, bw(8)).cutoff(Hz::new(120.0)));
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.cost, Cost::Moderate);
        assert_eq!(shape.params.as_slice(), &[UnitParam::Cutoff][..]);
        assert!(matches!(shape.tail, Tail::Finite(_)));
        assert_eq!(n.param_base(0), Some(120.0));
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

    /// A cutoff past the range is read as its end, bit for bit: 1 Hz as [`MIN_CUT_CUTOFF`],
    /// and one past the Nyquist as 0.99 of it.
    #[test]
    fn a_cutoff_out_of_range_is_read_as_the_end_of_it() {
        let input = generate_sine(500.0, 48_000.0, 4_096);
        let at = |hz: f64| {
            let config = CutFilterConfig::new(1u16, HP, bw(4)).cutoff(Hz::new(hz));
            render(&mut node(config), &input)
        };
        assert_eq!(at(1.0), at(MIN_CUT_CUTOFF.get()));
        assert_eq!(at(90_000.0), at(RATE.nyquist_scaled(0.99).get()));
        assert_ne!(at(10.0), at(100.0));
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

    /// The frames an `f64` impulse response (transposed direct form II, never the kernel's
    /// realization) of LR16 at `cutoff` last carries the tail floor of its peak.
    fn ring_at(response: CutResponse, cutoff: f64) -> usize {
        let sos = LinkwitzRiley::new(response, ButterworthOrder::of(8))
            .design(Hz::new(cutoff), RATE)
            .unwrap();
        let mut z = vec![[0.0f64; 2]; sos.sections().len()];
        let h: Vec<f64> = (0..1 << 20)
            .map(|n| {
                let mut x = if n == 0 { 1.0 } else { 0.0 };
                for (b, z) in sos.sections().iter().zip(&mut z) {
                    let y = b.b0 * x + z[0];
                    z[0] = b.b1 * x - b.a1 * y + z[1];
                    z[1] = b.b2 * x - b.a2 * y;
                    x = y;
                }
                x
            })
            .collect();
        let peak = h.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let eps = TAIL_FLOOR.to_amplitude().get();
        h.iter().rposition(|v| v.abs() >= eps * peak).unwrap() + 1
    }

    /// Between two octave rungs the tail `tail_of` reports covers the impulse response's
    /// at that exact cutoff and is under the next rung down's twice over, and the ceiling
    /// is the lowest rung's. Reporting the rung above fails the cover.
    #[test]
    fn the_tail_narrows_to_the_octave_below_the_cutoff() {
        let config = CutFilterConfig::new(1u16, LP, lr(8));
        let frames = |t: Tail| match t {
            Tail::Finite(s) => s.get(),
            other => panic!("{other:?}"),
        };
        for response in [LP, HP] {
            let mut kernel = CutFilterKernel::<f64>::new(filter_bus(1u16.into()), &config);
            kernel.response = response;
            kernel.prepare(RATE, MaxBlock::of(Samples::new(64)));
            assert_eq!(
                kernel.tail(),
                kernel.tail_of(&CutFilterControls {
                    cutoff: Hz::new(10.0)
                })
            );
            let mut last = usize::MAX;
            for cutoff in [14.0f64, 27.0, 55.0, 110.0, 230.0, 470.0, 1500.0, 3000.0] {
                let reported = frames(kernel.tail_of(&CutFilterControls {
                    cutoff: Hz::new(cutoff),
                }));
                let ring = ring_at(response, cutoff);
                assert!(
                    ring <= reported,
                    "{response:?} {cutoff}: {reported} < {ring}"
                );
                assert!(
                    reported <= 3 * ring + 64,
                    "{response:?} {cutoff}: {reported} vs {ring}"
                );
                assert!(reported <= last, "{response:?} {cutoff} does not narrow");
                last = reported;
            }
            assert!(frames(kernel.tail()) > 10 * last);
        }
    }
}
