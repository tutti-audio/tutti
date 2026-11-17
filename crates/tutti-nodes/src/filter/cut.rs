//! [`CutFilterNode`]: a Butterworth or Linkwitz-Riley high-pass or low-pass of 6 to 96 dB per
//! octave, run as a cascade of state-variable sections.

use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_dsp::{
    Butterworth, Glide, LinkwitzRiley, MAX_ORDER, MAX_SECTIONS, Ring, Sos, SosBank,
    SosCoefficients, SosResponse, System, TAIL_FLOOR,
};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_types::{BusTopology, ChannelLayout, Hz, MaxBlock, SampleRate, Samples, Tail};

use super::filter_bus;
use crate::kernel::{ControlLanes, Kernel, KernelNode};

/// The lowest cutoff a filter takes: 10 Hz, which bounds the ring-out of the slowest design
/// (LR96) at a few seconds.
pub const MIN_CUT_CUTOFF: Hz = Hz(10.0);

/// The steps the coefficients take to a new design: one grid period.
const GLIDE_STEPS: u32 = 16;

/// The octaves from [`MIN_CUT_CUTOFF`] a cutoff below 192 kHz's Nyquist can sit in.
const LADDER: usize = 14;

/// The control of a [`CutFilterNode`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutFilterControls {
    /// The cutoff: `-3 dB` for a Butterworth filter, `-6 dB` for a Linkwitz-Riley one. Read
    /// within [`MIN_CUT_CUTOFF`] and 0.99 of the Nyquist.
    pub cutoff: Hz,
}

impl CutFilterControls {
    /// 1 kHz.
    pub const DEFAULT: Self = Self {
        cutoff: Hz(1_000.0),
    };
}

/// What a [`CutFilterNode`] is made from: its width, response and order, and optionally its
/// alignment and cutoff.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutFilterConfig {
    width: ChannelLayout,
    response: SosResponse,
    order: u8,
    linkwitz_riley: bool,
    controls: CutFilterControls,
}

impl CutFilterConfig {
    /// A Butterworth filter of `width` channels, `response` and `order` (read within 1 and
    /// [`MAX_ORDER`]: `6 * order` dB per octave) at 1 kHz.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, response: SosResponse, order: u8) -> Self {
        Self {
            width: width.into(),
            response,
            order,
            linkwitz_riley: false,
            controls: CutFilterControls::DEFAULT,
        }
    }

    /// A Linkwitz-Riley filter instead: the order is read as even, within 2 and [`MAX_ORDER`].
    #[must_use]
    pub fn linkwitz_riley(self) -> Self {
        Self {
            linkwitz_riley: true,
            ..self
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

/// A high-pass or low-pass of any width, a [`Butterworth`] or [`LinkwitzRiley`] design run
/// as a cascade of state-variable sections ([`SosBank`]) in `f64`.
///
/// The cutoff is a live [`Hz`] cell (`UnitParam::Cutoff`), designed on a 16-frame grid
/// and reached along a line in the sections' coefficients over the next 16 frames with the
/// state kept, so an automated cutoff glides and the cascade stays stable. The response and
/// the order are the node's build: another is another node.
///
/// The tail is the ring-out of the whole cascade at the lowest cutoff ([`Sos`]'s tail,
/// every pole together), the ceiling over every control. No latency. A [`KernelNode`]. Made
/// from a [`CutFilterConfig`] (`CutFilterNode::from`).
///
/// # Panics
///
/// `CutFilterNode::from` panics past 16 channels: a filter is one bus.
pub type CutFilterNode = KernelNode<CutFilterKernel>;

impl From<CutFilterConfig> for CutFilterNode {
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

/// The filter as a [`Kernel`]: a [`SosBank`] and the glide of the coefficients every channel
/// shares.
#[derive(Clone, Debug)]
pub struct CutFilterKernel {
    input: BusTopology,
    response: SosResponse,
    order: u8,
    linkwitz_riley: bool,
    bank: SosBank<f64>,
    coefficients: Glide<SosCoefficients<f64>>,
    /// The design in progress, its room for [`MAX_SECTIONS`] sections held.
    scratch: Sos,
    rate: SampleRate,
    landed: bool,
    /// The tail at each octave from [`MIN_CUT_CUTOFF`] up (`LADDER` rungs at most), found
    /// in `prepare`; the rungs past the Nyquist's are unused.
    ladder: [Tail; LADDER],
}

impl CutFilterKernel {
    /// A filter on the bus `input`, its output where it is; at a placeholder 48 kHz until
    /// `prepare`, which an unprepared node renders nothing before.
    fn new(input: BusTopology, config: &CutFilterConfig) -> Self {
        let rate = SampleRate::SR_48K;
        let order = config.order.clamp(1, MAX_ORDER);
        let mut kernel = Self {
            input,
            response: config.response,
            order: if config.linkwitz_riley {
                (order & !1).max(2)
            } else {
                order
            },
            linkwitz_riley: config.linkwitz_riley,
            bank: SosBank::new(input.layout()),
            coefficients: Glide::landed(SosCoefficients::THROUGH),
            scratch: Sos::new(Vec::with_capacity(MAX_SECTIONS), rate),
            rate,
            landed: false,
            ladder: [Tail::Unknown; LADDER],
        };
        kernel.coefficients = Glide::landed(kernel.design(CutFilterControls::DEFAULT.cutoff));
        kernel
    }

    /// The coefficients of the design at `cutoff`, read within [`MIN_CUT_CUTOFF`] and the
    /// Nyquist; the cascade as it is for a cutoff that is not a number. No allocation.
    fn design(&mut self, cutoff: Hz) -> SosCoefficients<f64> {
        let top = self.rate.nyquist_scaled(0.99).get();
        let cutoff = Hz(cutoff.get().clamp(MIN_CUT_CUTOFF.get(), top));
        let done = if self.linkwitz_riley {
            LinkwitzRiley::new(self.response, self.order).design_into(
                cutoff,
                self.rate,
                &mut self.scratch,
            )
        } else {
            Butterworth::new(self.response, self.order).design_into(
                cutoff,
                self.rate,
                &mut self.scratch,
            )
        };
        done.ok()
            .and_then(|()| SosCoefficients::new(&self.scratch))
            .unwrap_or_else(|| self.coefficients.target())
    }
}

impl Kernel for CutFilterKernel {
    type Set = CutFilterSet;

    const GRID: usize = GLIDE_STEPS as usize;

    fn shape(&self) -> Shape {
        let output = Bus::banded(self.input, BusRole::Main, 1).expect("one band is a bus");
        Shape::buses(
            Buses::main(self.input),
            Buses::NONE.with(output).expect("one bus"),
        )
        .with_cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.rate = rate;
        self.coefficients =
            Glide::landed(self.design(CutFilterControls::DEFAULT.cutoff)).by(GLIDE_STEPS);
        self.landed = false;
        let top = self.rate.nyquist_scaled(0.99).get();
        for rung in 0..LADDER {
            let cutoff = MIN_CUT_CUTOFF.get() * (1u32 << rung) as f32;
            let _ = self.design(Hz(cutoff.min(top)));
            self.ladder[rung] = match self.scratch.tail(TAIL_FLOOR) {
                Ring::Finite(s) => Tail::Finite(Samples((s.get() * rate.get()).ceil() as usize)),
                Ring::Unbounded => Tail::Unbounded,
            };
        }
    }

    fn reset(&mut self) {
        self.coefficients.settle();
        self.bank.reset();
    }

    fn retarget(&mut self, controls: &CutFilterControls) {
        let design = self.design(controls.cutoff);
        self.coefficients.aim(design);
        if !self.landed {
            self.coefficients.settle();
            self.landed = true;
        }
    }

    fn render<S: Float>(
        &mut self,
        input: Planes<'_, S>,
        _: ControlLanes<'_, S>,
        output: PlanesMut<'_, '_, S>,
    ) {
        self.bank.process(&mut self.coefficients, input, output);
    }

    /// The ring-out at the lowest cutoff: the longest over every control.
    fn tail(&self) -> Tail {
        self.ladder[0]
    }

    /// The tail found for the highest octave from [`MIN_CUT_CUTOFF`] at or below the
    /// cutoff in force: a lower cutoff rings longer, so it errs long. No simulation.
    fn tail_of(&mut self, controls: &CutFilterControls) -> Tail {
        let top = self.rate.nyquist_scaled(0.99).get();
        let cutoff = controls.cutoff.get().clamp(MIN_CUT_CUTOFF.get(), top);
        let octave = (1..LADDER)
            .take_while(|&rung| MIN_CUT_CUTOFF.get() * (1u32 << rung) as f32 <= cutoff)
            .count();
        self.ladder[octave]
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::PI;

    use super::*;
    use crate::filter::test_utils::{generate_sine, rms};
    use crate::test_support::{RATE, assert_cell_glides, drive_block, prepared};
    use tutti_dsp::TAIL_FLOOR;
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_types::UnitParam;

    const LP: SosResponse = SosResponse::LowPass;
    const HP: SosResponse = SosResponse::HighPass;

    fn node(config: CutFilterConfig) -> CutFilterNode {
        prepared(CutFilterNode::from(config))
    }

    /// The frequency of bin `m` of the 8192-frame window the levels are read over: a sine
    /// there has whole cycles in it, so its RMS is exact.
    fn bin(m: u32) -> f32 {
        (RATE.get() * f64::from(m) / 8192.0) as f32
    }

    /// The amplitude of a sine whose samples are `x`.
    fn level(x: &[f32]) -> f64 {
        f64::from(rms(x)) * 2f64.sqrt()
    }

    /// `input` through `node` in blocks of 1024.
    fn render(node: &mut CutFilterNode, input: &[f32]) -> Vec<f32> {
        input
            .chunks(1024)
            .flat_map(|c| drive_block(node, &[c]).remove(0))
            .collect()
    }

    /// The RMS of the settled half of a unit sine at `f` through the filter.
    fn gain(config: CutFilterConfig, f: f32) -> f64 {
        let out = render(&mut node(config), &generate_sine(f, 48_000.0, 16_384));
        level(&out[8_192..])
    }

    /// The analog Butterworth magnitude at the bilinear image of `f`.
    fn analog(response: SosResponse, order: u8, fc: f64, f: f64) -> f64 {
        let t = libm::tan(PI * f / RATE.get()) / libm::tan(PI * fc / RATE.get());
        let ratio = if response == LP { t } else { 1.0 / t };
        1.0 / (1.0 + ratio.powi(2 * i32::from(order))).sqrt()
    }

    /// A fork starts from the cutoff last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(CutFilterNode::from(CutFilterConfig::new(2u16, LP, 4)));
    }

    /// The shape is the width in and out, `Moderate`, declares the cutoff and a finite
    /// tail once prepared; the cutoff starts at the configured one.
    #[test]
    fn the_shape_declares_the_cutoff_and_a_finite_tail() {
        let n = node(CutFilterConfig::new(3u16, HP, 8).cutoff(Hz(120.0)));
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
                    let want = analog(response, order, 1000.0, f64::from(f));
                    let got = gain(CutFilterConfig::new(1u16, response, order), f);
                    assert!(
                        (got - want).abs() < 2e-5 + 1e-4 * want,
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
                CutFilterConfig::new(1u16, response, order)
                    .linkwitz_riley()
                    .cutoff(Hz(bin(171)))
            };
            assert!((gain(lr(LP), bin(171)) - 0.5).abs() < 1e-4, "LR{order}");
            for f in [34, 120, 171, 512].map(bin) {
                let want = analog(LP, order / 2, f64::from(bin(171)), f64::from(f)).powi(2);
                assert!((gain(lr(LP), f) - want).abs() < 2e-5 + 1e-4 * want);
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
    fn a_cutoff_glide_stays_finite_and_settles_on_the_design() {
        let make = || CutFilterNode::from(CutFilterConfig::new(1u16, LP, 16).linkwitz_riley());
        let sine = generate_sine(bin(256), 48_000.0, 8_192);
        let (mut node, out) = {
            let n = assert_cell_glides(
                make,
                |n: &CutFilterNode| assert!(n.param_set().set(UnitParam::Cutoff, 4_000.0)),
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
        let want = analog(LP, 8, 4_000.0, f64::from(bin(256))).powi(2);
        assert!((level(&settled[16_384..]) - want).abs() < 1e-4);
    }

    /// The order and the cutoff are read within their range: an order past the table, an
    /// odd Linkwitz-Riley order and a cutoff past Nyquist build a stable filter.
    #[test]
    fn an_order_and_cutoff_out_of_range_are_read_within_it() {
        for config in [
            CutFilterConfig::new(1u16, LP, 40),
            CutFilterConfig::new(1u16, HP, 0),
            CutFilterConfig::new(1u16, LP, 5).linkwitz_riley(),
            CutFilterConfig::new(1u16, HP, 4).cutoff(Hz(90_000.0)),
            CutFilterConfig::new(1u16, HP, 4).cutoff(Hz(1.0)),
        ] {
            let out = render(&mut node(config), &generate_sine(500.0, 48_000.0, 4_096));
            assert!(
                out.iter().all(|v| v.is_finite() && v.abs() < 2.0),
                "{config:?}"
            );
        }
    }

    /// The frames an `f64` impulse response (transposed direct form II, never the kernel's
    /// realization) of LR16 at `cutoff` last carries the tail floor of its peak.
    fn ring_at(response: SosResponse, cutoff: f32) -> usize {
        let sos = LinkwitzRiley::new(response, 16)
            .design(Hz(cutoff), RATE)
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
        let eps = f64::from(TAIL_FLOOR.to_amplitude().get());
        h.iter().rposition(|v| v.abs() >= eps * peak).unwrap() + 1
    }

    /// Between two octave rungs the tail `tail_of` reports covers the impulse response's
    /// at that exact cutoff and is under the next rung down's twice over, and the ceiling
    /// is the lowest rung's. Reporting the rung above fails the cover.
    #[test]
    fn the_tail_narrows_to_the_octave_below_the_cutoff() {
        let config = CutFilterConfig::new(1u16, LP, 16).linkwitz_riley();
        let frames = |t: Tail| match t {
            Tail::Finite(s) => s.get(),
            other => panic!("{other:?}"),
        };
        for response in [LP, HP] {
            let mut kernel = CutFilterKernel::new(filter_bus(1u16.into()), &config);
            kernel.response = response;
            kernel.prepare(RATE, MaxBlock::of(Samples(64)));
            assert_eq!(
                kernel.tail(),
                kernel.tail_of(&CutFilterControls { cutoff: Hz(10.0) })
            );
            let mut last = usize::MAX;
            for cutoff in [14.0f32, 27.0, 55.0, 110.0, 230.0, 470.0, 1500.0, 3000.0] {
                let reported = frames(kernel.tail_of(&CutFilterControls { cutoff: Hz(cutoff) }));
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
