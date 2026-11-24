//! [`TiltFilterNode`]: a first-order tilt equalizer, run as one state-variable section.

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_dsp::{DecayTime, SimdLanes, Sos, SosError, TiltFilterControls};
use tutti_units::{Db, Hz, SampleRate};

use super::eq::sealed::Sealed;
use super::eq::{EqControls, EqKernel};
use super::filter_bus;
use crate::kernel::KernelNode;

/// What a [`TiltFilterNode`] is made from: its width and its controls' start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TiltFilterConfig {
    width: ChannelLayout,
    controls: TiltFilterControls,
}

impl TiltFilterConfig {
    /// A tilt of `width` channels pivoting at `frequency`, `gain` between DC and Nyquist.
    #[must_use]
    pub fn new(width: impl Into<ChannelLayout>, frequency: Hz, gain: Db) -> Self {
        Self {
            width: width.into(),
            controls: TiltFilterControls { frequency, gain },
        }
    }
}

/// A tilt equalizer of any width, a [`Tilt`](tutti_dsp::Tilt) run as one state-variable section
/// ([`SosBank`](tutti_dsp::SosBank)) in `f64`: the low end down by half the gain and the high
/// end up by half, 0 dB at the pivot.
///
/// The pivot and the gain are live cells (`UnitParam::Cutoff` and `GainDb`), designed in
/// closed form on a 16-frame grid and reached along a line in the section's coefficients
/// over the next 16 frames with the state kept, so an automated control ramps.
///
/// The tail is the tail-sum bound over every pivot and gain ([`Tilt::decay_time_over`](tutti_dsp::Tilt::decay_time_over)), the
/// ceiling; each block narrows it to the bound of the controls in force ([`Tilt::decay_time`](tutti_dsp::Tilt::decay_time)). No latency. A [`KernelNode`]. Made from a
/// [`TiltFilterConfig`] (`TiltFilterNode::from`).
///
/// # Panics
///
/// `TiltFilterNode::from` panics past 16 channels: a filter is one bus.
pub type TiltFilterNode<F> = KernelNode<TiltFilterKernel<F>>;

/// The tilt as a [`Kernel`](crate::Kernel).
pub type TiltFilterKernel<F> = EqKernel<TiltFilterControls, F>;

impl<F: Float + SimdLanes> From<TiltFilterConfig> for TiltFilterNode<F> {
    fn from(config: TiltFilterConfig) -> Self {
        Self::with_kernel(
            TiltFilterKernel::new(filter_bus(config.width)),
            TiltFilterSet,
            config.controls,
        )
    }
}

/// The tilt's controls: the pivot and the gain.
#[derive(Clone, Copy, Debug)]
pub struct TiltFilterSet;

control_set! {
    TiltFilterSet(_s): TiltFilterControls = TiltFilterControls::DEFAULT;
    live: (all);
    Cutoff => Hz = frequency,
    GainDb => Db = gain;

    fn defaults(&self) -> TiltFilterControls {
        TiltFilterControls::DEFAULT
    }
}

impl EqControls for TiltFilterControls {}

impl Sealed for TiltFilterControls {
    type Set = TiltFilterSet;
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
    use super::*;
    use crate::filter::test_utils::{bin, gain, generate_sine, level, render, ring_at};
    use crate::kernel::Kernel;
    use crate::test_support::{RATE, assert_cell_ramps};
    use core::f64::consts::PI;
    use tutti_buffers::MaxBlock;
    use tutti_dsp::Tilt;
    use tutti_graph::ParamNode;
    use tutti_units::UnitParam;
    use tutti_units::{Samples, Tail};

    fn config(frequency: Hz, gain: Db) -> TiltFilterConfig {
        TiltFilterConfig::new(1u16, frequency, gain)
    }

    fn gain_of(config: TiltFilterConfig, f: f64) -> f64 {
        gain(TiltFilterNode::from(config), f)
    }

    /// The analog tilt `(A^2 x^2 + 1) / (x^2 + A^2)` (squared amplitudes) at the bilinear
    /// image of `f` for a pivot `fc`.
    fn analog(fc: f64, db: f64, f: f64) -> f64 {
        let x = libm::tan(PI * f / RATE.get()) / libm::tan(PI * fc / RATE.get());
        let a2 = 10f64.powf(db / 20.0);
        ((a2 * x * x + 1.0) / (x * x + a2)).sqrt()
    }

    /// A settled rendered sine has the bilinear image of the analog tilt as its gain
    /// (to 1e-4), and 0 dB at the pivot.
    #[test]
    fn a_rendered_sine_has_the_analog_tilt_magnitude() {
        let pivot = Hz::new(bin(171));
        for db in [-18.0f64, -6.0, 6.0, 18.0] {
            for f in [10, 34, 120, 240, 683, 2_400, 3_900].map(bin) {
                let want = analog(pivot.get(), db, f);
                let got = gain_of(config(pivot, Db::new(db)), f);
                assert!(
                    (got - want).abs() < 1e-4 * want,
                    "gain {db} at {f}: {got} vs {want}"
                );
            }
            assert!((gain_of(config(pivot, Db::new(db)), pivot.get()) - 1.0).abs() < 1e-4);
        }
    }

    /// The pivot and the gain moved mid-sine stay finite and bounded, render the same
    /// however the host cuts its blocks, and settle on the design.
    #[test]
    fn a_ramp_of_the_controls_stays_finite_and_settles_on_the_design() {
        let make = || TiltFilterNode::from(config(Hz::new(200.0), Db::UNITY));
        let sine = generate_sine(bin(256), 48_000.0, 8_192);
        let mut node = assert_cell_ramps(
            make,
            |n: &TiltFilterNode<f32>| {
                assert!(n.param_set().set(UnitParam::Cutoff, 4_000.0));
                assert!(n.param_set().set(UnitParam::GainDb, 12.0));
            },
            &[&sine[..2_048]],
            &[&sine],
            "pivot and gain",
        );
        for (hz, db) in [
            (30.0, 24.0),
            (21_000.0, -24.0),
            (60.0, -18.0),
            (20_000.0, 12.0),
        ] {
            assert!(node.param_set().set(UnitParam::Cutoff, hz));
            assert!(node.param_set().set(UnitParam::GainDb, db));
            let out = render(&mut node, &sine);
            assert!(out.iter().all(|v| v.is_finite() && v.abs() < 40.0));
        }
        assert!(node.param_set().set(UnitParam::Cutoff, 4_000.0));
        assert!(node.param_set().set(UnitParam::GainDb, 12.0));
        let settled = render(&mut node, &generate_sine(bin(256), 48_000.0, 24_576));
        let want = analog(4_000.0, 12.0, bin(256));
        assert!((level(&settled[16_384..]) - want).abs() < 1e-4 * want);
    }

    /// Across pivots and gains, from a pole near 1 (a low pivot) to one near -1 (a high
    /// pivot and a boost), the reported tail covers the first frame from which the
    /// impulse response's tail sum is under the floor, and the ceiling covers every one.
    #[test]
    fn the_tail_covers_the_tail_sum_and_the_ceiling_covers_it_all() {
        let mut kernel = TiltFilterKernel::<f64>::new(filter_bus(1u16.into()));
        kernel.prepare(RATE, MaxBlock::of(Samples::new(64)));
        let frames = |t: Tail| match t {
            Tail::Finite(s) => s.get(),
            other => panic!("{other:?}"),
        };
        let ceiling = frames(kernel.tail());
        for fc in [20.0f64, 90.0, 700.0, 4_000.0, 15_000.0, 23_000.0] {
            for db in [-24.0f64, -6.0, 6.0, 24.0] {
                let controls = TiltFilterControls {
                    frequency: Hz::new(fc),
                    gain: Db::new(db),
                };
                let reported = frames(kernel.tail_of(&controls));
                let section = Tilt::new(Db::new(db)).design(Hz::new(fc), RATE).unwrap();
                let ring = ring_at(&section.sections()[0]);
                let tag = format!("fc {fc} gain {db}: reported {reported}, ring {ring}");
                assert!(ring <= reported, "{tag}");
                assert!(reported <= ceiling, "{tag}");
            }
        }
    }
}
