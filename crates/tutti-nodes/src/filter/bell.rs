//! [`BellFilterNode`]: a matched peaking equalizer, run as one state-variable section.

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_dsp::{BellFilterControls, DecayTime, SimdLanes, Sos, SosError};
use tutti_units::{Db, Hz, Q, SampleRate};

use super::eq::sealed::Sealed;
use super::eq::{EqControls, EqKernel};
use super::filter_bus;
use crate::kernel::KernelNode;

/// What a [`BellFilterNode`] is made from: its width and its controls' start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BellFilterConfig {
    width: ChannelLayout,
    controls: BellFilterControls,
}

impl BellFilterConfig {
    /// A bell of `width` channels at `frequency`, quality factor `q` and `gain`.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, frequency: Hz, q: Q, gain: Db) -> Self {
        Self {
            width: width.into(),
            controls: BellFilterControls { frequency, q, gain },
        }
    }
}

/// A peaking equalizer of any width, a [`MatchedBell`](tutti_dsp::MatchedBell) run as one state-variable section
/// ([`SosBank`](tutti_dsp::SosBank)) in `f64`.
///
/// The analog bell's gain is kept at DC, at the center and at Nyquist, so a center high in
/// the band keeps its shape where a bilinear bell is cramped. The center, [`Q`] and gain
/// are live cells (`UnitParam::Cutoff`, `Q` and `GainDb`), designed in closed form on a
/// 16-frame grid and reached along a line in the section's coefficients over the next 16
/// frames with the state kept, so an automated control ramps and the section stays stable.
///
/// The declared tail is unbounded: the matched cut's poles have no closed-form bound over the
/// ranges. Each block narrows it to [`MatchedBell::decay_time`](tutti_dsp::MatchedBell::decay_time) at the controls in force, the
/// first frame from which the tail sum of the impulse response is under the floor, so any
/// input in `[-1, 1]` that stops is under the floor by then at the controls in force. No latency. A [`KernelNode`]. Made from a
/// [`BellFilterConfig`] (`BellFilterNode::from`).
///
/// # Panics
///
/// `BellFilterNode::from` panics past 16 channels: a filter is one bus.
pub type BellFilterNode<F> = KernelNode<BellFilterKernel<F>>;

/// The bell as a [`Kernel`](crate::Kernel).
pub type BellFilterKernel<F> = EqKernel<BellFilterControls, F>;

impl<F: Float + SimdLanes> From<BellFilterConfig> for BellFilterNode<F> {
    fn from(config: BellFilterConfig) -> Self {
        Self::with_kernel(
            BellFilterKernel::new(filter_bus(config.width)),
            BellFilterSet,
            config.controls,
        )
    }
}

/// The bell's controls: the center, the quality factor and the gain.
#[derive(Clone, Copy, Debug)]
pub struct BellFilterSet;

control_set! {
    BellFilterSet(_s): BellFilterControls = BellFilterControls::DEFAULT;
    live: (all);
    Cutoff => Hz = frequency,
    Q => Q = q,
    GainDb => Db = gain;

    fn defaults(&self) -> BellFilterControls {
        BellFilterControls::DEFAULT
    }
}

impl EqControls for BellFilterControls {}

impl Sealed for BellFilterControls {
    type Set = BellFilterSet;
    const DEFAULT: Self = Self::DEFAULT;

    fn held(&self, rate: SampleRate) -> Self {
        self.held(rate)
    }

    fn design_into(&self, rate: SampleRate, into: &mut Sos) -> Result<(), SosError> {
        self.design_into(rate, into)
    }

    fn decay_time(&self, rate: SampleRate, floor: Db) -> Result<DecayTime, SosError> {
        self.decay_time(rate, floor)
    }

    fn decay_ceiling(rate: SampleRate, floor: Db) -> Result<DecayTime, SosError> {
        Self::decay_ceiling(rate, floor)
    }
}

#[cfg(test)]
mod tests {
    // These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.
    use core::f64::consts::PI;

    use super::*;
    use crate::filter::test_utils::{bin, gain, generate_sine, level, render, ring_at};
    use crate::kernel::Kernel;
    use crate::test_support::{RATE, assert_cell_ramps};
    use tutti_buffers::MaxBlock;
    use tutti_dsp::MatchedBell;

    use tutti_graph::ParamNode;
    use tutti_units::UnitParam;
    use tutti_units::{Samples, Tail};

    fn config(frequency: Hz, q: Q, gain: Db) -> BellFilterConfig {
        BellFilterConfig::new(1u16, frequency, q, gain)
    }

    fn gain_of(config: BellFilterConfig, f: f64) -> f64 {
        gain(BellFilterNode::from(config), f)
    }

    /// The design's magnitude at `f`, read from the section's coefficients.
    fn designed(fc: Hz, q: Q, db: Db, f: f64) -> f64 {
        let sos = MatchedBell::new(q, db).design(fc, RATE).unwrap();
        sos.sections()[0].response(2.0 * PI * f / RATE.get()).norm()
    }

    /// The analog bell `(s^2 + s A / Q + 1) / (s^2 + s / (A Q) + 1)` at `s = j f / fc`, as a
    /// magnitude, for `A = 10^(db / 40)`: the oracle the design is matched to.
    fn analog(fc: f64, q: f64, db: f64, f: f64) -> f64 {
        let a = 10f64.powf(db / 40.0);
        let x = f / fc;
        let off = 1.0 - x * x;
        ((off * off + (a * x / q).powi(2)) / (off * off + (x / (a * q)).powi(2))).sqrt()
    }

    /// A settled rendered sine has the design's magnitude (to 1e-4) at several bins, and
    /// exactly the gain at the center: the analog bell's value, the point the design matches.
    #[test]
    fn a_rendered_sine_has_the_designed_magnitude_and_the_gain_at_the_center() {
        let center = Hz::new(bin(171));
        for (q, db) in [(0.7, 12.0), (3.0, -9.0), (12.0, 18.0), (0.4, -18.0)] {
            for f in [34, 120, 171, 240, 683, 2_400, 3_900].map(bin) {
                let want = designed(center, Q::new(q), Db::new(db), f);
                let got = gain_of(config(center, Q::new(q), Db::new(db)), f);
                assert!(
                    (got - want).abs() < 1e-4 * want,
                    "q {q} gain {db} at {f}: {got} vs {want}"
                );
            }
            // The matched points against the analog bell itself: DC (bin 1), the center and
            // Nyquist (bin 4095), where the render is the analog magnitude.
            for f in [bin(1), center.get(), bin(4_095)] {
                let want = analog(center.get(), q, db, f);
                let got = gain_of(config(center, Q::new(q), Db::new(db)), f);
                assert!(
                    (got - want).abs() < 2e-3 * want,
                    "q {q} gain {db} at {f}: {got} vs analog {want}"
                );
            }
            let at = gain_of(config(center, Q::new(q), Db::new(db)), center.get());
            let want = 10f64.powf(db / 20.0);
            assert!((at - want).abs() < 1e-4 * want, "q {q} gain {db}: {at}");
        }
    }

    /// The controls moved across their ranges mid-sine stay finite and bounded, render the
    /// same however the host cuts its blocks, and settle on the design.
    #[test]
    fn a_ramp_of_the_controls_stays_finite_and_settles_on_the_design() {
        let make = || BellFilterNode::from(config(Hz::new(200.0), Q::new(1.0), Db::UNITY));
        let sine = generate_sine(bin(256), 48_000.0, 8_192);
        let mut node = assert_cell_ramps(
            make,
            |n: &BellFilterNode<f32>| {
                assert!(n.param_set().set(UnitParam::Cutoff, 4_000.0));
                assert!(n.param_set().set(UnitParam::GainDb, 12.0));
            },
            &[&sine[..2_048]],
            &[&sine],
            "center and gain",
        );
        for (hz, q, db) in [
            (30.0, 25.0, 24.0),
            (12_000.0, 0.2, -24.0),
            (60.0, 8.0, 18.0),
            (20_000.0, 3.0, -12.0),
        ] {
            assert!(node.param_set().set(UnitParam::Cutoff, hz));
            assert!(node.param_set().set(UnitParam::Q, q));
            assert!(node.param_set().set(UnitParam::GainDb, db));
            let out = render(&mut node, &sine);
            assert!(out.iter().all(|v| v.is_finite() && v.abs() < 40.0));
        }
        assert!(node.param_set().set(UnitParam::Cutoff, 4_000.0));
        assert!(node.param_set().set(UnitParam::Q, 2.0));
        assert!(node.param_set().set(UnitParam::GainDb, 12.0));
        let settled = render(&mut node, &generate_sine(bin(256), 48_000.0, 24_576));
        let want = designed(Hz::new(4_000.0), Q::new(2.0), Db::new(12.0), bin(256));
        assert!((level(&settled[16_384..]) - want).abs() < 1e-4 * want);
    }

    /// For centers, Q and gains across the ranges the reported tail covers the first frame
    /// from which the impulse response's tail sum is under the floor.
    #[test]
    fn the_tail_covers_the_tail_sum_of_the_impulse_response() {
        let mut kernel = BellFilterKernel::<f64>::new(filter_bus(1u16.into()));
        kernel.prepare(RATE, MaxBlock::of(Samples::new(64)));
        let frames = |t: Tail| match t {
            Tail::Finite(s) => s.get(),
            other => panic!("{other:?}"),
        };
        assert_eq!(kernel.tail(), Tail::Unbounded);
        for fc in [20.0f64, 90.0, 700.0, 4_000.0, 15_000.0, 22_000.0] {
            for q in [0.1f64, 0.7, 4.0, 30.0] {
                for db in [-24.0f64, -6.0, 6.0, 24.0] {
                    let controls = BellFilterControls {
                        frequency: Hz::new(fc),
                        q: Q::new(q),
                        gain: Db::new(db),
                    };
                    let reported = frames(kernel.tail_of(&controls));
                    let section = MatchedBell::new(Q::new(q), Db::new(db))
                        .design(Hz::new(fc), RATE)
                        .unwrap();
                    let ring = ring_at(&section.sections()[0]);
                    let tag = format!("fc {fc} q {q} gain {db}: reported {reported}, ring {ring}");
                    assert!(ring <= reported, "{tag}");
                }
            }
        }
    }
}
