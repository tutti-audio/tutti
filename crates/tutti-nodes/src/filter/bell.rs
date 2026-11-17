//! [`BellFilterNode`]: a matched peaking equalizer, run as one state-variable section.

use tutti_dsp::{MatchedBell, Ring, Sos, SosError};
use tutti_types::{ChannelLayout, Db, Hz, Q, SampleRate};

use super::eq::{EqControls, EqKernel};
use super::{eq_center, eq_gain, filter_bus};
use crate::kernel::KernelNode;

/// The quality factors a bell takes: 0.1 to 30.
const Q_RANGE: (f32, f32) = (0.1, 30.0);

/// The controls of a [`BellFilterNode`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BellFilterControls {
    /// The center, where the gain is reached. Read within 20 Hz and 0.99 of the Nyquist.
    pub frequency: Hz,
    /// The quality factor: the narrower, the higher. Read within 0.1 and 30.
    pub q: Q,
    /// The gain at the center, a boost above 0 dB and a cut below. Read within 24 dB.
    pub gain: Db,
}

impl BellFilterControls {
    /// 1 kHz, [`Q::BUTTERWORTH`] and 0 dB.
    pub const DEFAULT: Self = Self {
        frequency: Hz(1_000.0),
        q: Q::BUTTERWORTH,
        gain: Db::UNITY,
    };
}

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

/// A peaking equalizer of any width, a [`MatchedBell`] run as one state-variable section
/// ([`SosBank`](tutti_dsp::SosBank)) in `f64`.
///
/// The analog bell's gain is kept at DC, at the center and at Nyquist, so a center high in
/// the band keeps its shape where a bilinear bell is cramped. The center, [`Q`] and gain
/// are live cells (`UnitParam::Cutoff`, `Q` and `GainDb`), designed in closed form on a
/// 16-frame grid and reached along a line in the section's coefficients over the next 16
/// frames with the state kept, so an automated control glides and the section stays stable.
///
/// The declared tail is unbounded: the matched cut's poles have no closed-form bound over the
/// ranges. Each block narrows it to [`MatchedBell::ring_out`] at the controls in force, the
/// first frame from which the tail sum of the impulse response is under the floor, so any
/// input in `[-1, 1]` that stops is under the floor by then. No latency. A [`KernelNode`]. Made from a
/// [`BellFilterConfig`] (`BellFilterNode::from`).
///
/// # Panics
///
/// `BellFilterNode::from` panics past 16 channels: a filter is one bus.
pub type BellFilterNode = KernelNode<BellFilterKernel>;

/// The bell as a [`Kernel`](crate::Kernel).
pub type BellFilterKernel = EqKernel<BellFilterControls>;

impl From<BellFilterConfig> for BellFilterNode {
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

impl EqControls for BellFilterControls {
    type Set = BellFilterSet;
    const DEFAULT: Self = Self::DEFAULT;

    fn held(self, rate: SampleRate) -> Self {
        Self {
            frequency: eq_center(self.frequency, rate),
            q: Q(self.q.get().clamp(Q_RANGE.0, Q_RANGE.1)),
            gain: eq_gain(self.gain),
        }
    }

    fn design_into(&self, rate: SampleRate, into: &mut Sos) -> Result<(), SosError> {
        MatchedBell::new(self.q, self.gain).design_into(self.frequency, rate, into)
    }

    fn ring_out(&self, rate: SampleRate, floor: Db) -> Result<Ring, SosError> {
        MatchedBell::new(self.q, self.gain).ring_out(self.frequency, rate, floor)
    }

    /// Unbounded. The boost's poles are `exp(-zeta w0)` with `zeta = 1 / (2 A Q)`, slowest
    /// at the lowest center, highest Q and largest boost, but the cut is the boost's
    /// inverse, whose poles are the boost's zeros, and the matched zeros have no
    /// closed-form bound on their radius (the numerator is fixed by three magnitudes, not
    /// by its zeros). A ceiling is a bound over every control, so the shape declares none;
    /// each block's [`ring_out`](Self::ring_out) is a bound at its controls and is what the
    /// node reports.
    fn ring_ceiling(_: SampleRate, _: Db) -> Result<Ring, SosError> {
        Ok(Ring::Unbounded)
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::PI;

    use super::*;
    use crate::filter::test_utils::{bin, gain, generate_sine, level, render};
    use crate::kernel::Kernel;
    use crate::test_support::{RATE, assert_cell_glides, prepared};
    use tutti_dsp::{Biquad, TAIL_FLOOR};
    use tutti_graph::Cost;
    use tutti_graph::contract::assert_param_fork;
    use tutti_graph::{Node, ParamNode};
    use tutti_types::UnitParam;
    use tutti_types::{MaxBlock, Samples, Tail};

    fn config(frequency: Hz, q: f32, gain: f32) -> BellFilterConfig {
        BellFilterConfig::new(1u16, frequency, Q(q), Db(gain))
    }

    fn node(config: BellFilterConfig) -> BellFilterNode {
        prepared(BellFilterNode::from(config))
    }

    fn gain_of(config: BellFilterConfig, f: f32) -> f64 {
        gain(BellFilterNode::from(config), f)
    }

    /// The design's magnitude at `f`, read from the section's coefficients.
    fn designed(fc: Hz, q: f32, db: f32, f: f32) -> f64 {
        let sos = MatchedBell::new(Q(q), Db(db)).design(fc, RATE).unwrap();
        sos.sections()[0]
            .response(2.0 * PI * f64::from(f) / RATE.get())
            .norm()
    }

    /// A fork starts from the controls last set and shares no cell.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(BellFilterNode::from(BellFilterConfig::new(
            2u16,
            Hz(300.0),
            Q(2.0),
            Db(-6.0),
        )));
    }

    /// The shape is the width in and out, `Moderate`, declares the center, Q and gain and a
    /// finite tail once prepared; the center starts at the configured one.
    #[test]
    fn the_shape_declares_its_controls_and_a_finite_tail() {
        let n = node(BellFilterConfig::new(3u16, Hz(120.0), Q(4.0), Db(9.0)));
        let shape = n.shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (3, 3)
        );
        assert_eq!(shape.cost, Cost::Moderate);
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Cutoff, UnitParam::Q, UnitParam::GainDb][..]
        );
        assert_eq!(shape.tail, Tail::Unbounded);
        assert_eq!(n.param_base(0), Some(120.0));
        assert_eq!(n.param_base(2), Some(9.0));
    }

    /// A settled rendered sine has the design's magnitude (to 1e-4) at several bins, and
    /// exactly the gain at the center: the analog bell's value, the point the design matches.
    #[test]
    fn a_rendered_sine_has_the_designed_magnitude_and_the_gain_at_the_center() {
        let center = Hz(bin(171));
        for (q, db) in [(0.7, 12.0), (3.0, -9.0), (12.0, 18.0), (0.4, -18.0)] {
            for f in [34, 120, 171, 240, 683, 2_400, 3_900].map(bin) {
                let want = designed(center, q, db, f);
                let got = gain_of(config(center, q, db), f);
                assert!(
                    (got - want).abs() < 1e-4 * want,
                    "q {q} gain {db} at {f}: {got} vs {want}"
                );
            }
            let at = gain_of(config(center, q, db), center.get());
            let want = 10f64.powf(f64::from(db) / 20.0);
            assert!((at - want).abs() < 1e-4 * want, "q {q} gain {db}: {at}");
        }
    }

    /// The controls moved across their ranges mid-sine stay finite and bounded, render the
    /// same however the host cuts its blocks, and settle on the design.
    #[test]
    fn a_glide_of_the_controls_stays_finite_and_settles_on_the_design() {
        let make = || BellFilterNode::from(config(Hz(200.0), 1.0, 0.0));
        let sine = generate_sine(bin(256), 48_000.0, 8_192);
        let mut node = assert_cell_glides(
            make,
            |n: &BellFilterNode| {
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
        let want = designed(Hz(4_000.0), 2.0, 12.0, bin(256));
        assert!((level(&settled[16_384..]) - want).abs() < 1e-4 * want);
    }

    /// A center, Q and gain past their ranges read as the end of them, bit for bit.
    #[test]
    fn controls_out_of_range_are_read_as_the_end_of_it() {
        let input = generate_sine(500.0, 48_000.0, 4_096);
        let at = |hz: f32, q: f32, db: f32| render(&mut node(config(Hz(hz), q, db)), &input);
        assert_eq!(at(1.0, 1.0, 6.0), at(20.0, 1.0, 6.0));
        assert_eq!(
            at(90_000.0, 1.0, 6.0),
            at(RATE.nyquist_scaled(0.99).get(), 1.0, 6.0)
        );
        assert_eq!(at(500.0, 0.0, 6.0), at(500.0, 0.1, 6.0));
        assert_eq!(at(500.0, 900.0, 6.0), at(500.0, 30.0, 6.0));
        assert_eq!(at(500.0, 1.0, 90.0), at(500.0, 1.0, 24.0));
        assert_ne!(at(500.0, 1.0, 6.0), at(500.0, 1.0, -6.0));
    }

    /// The first frame from which the tail sum of the bell's `f64` impulse response
    /// (transposed direct form II, never the kernel's realization) is at most the floor.
    fn ring_at(fc: f32, q: f32, db: f32) -> usize {
        let b: Biquad = MatchedBell::new(Q(q), Db(db))
            .design(Hz(fc), RATE)
            .unwrap()
            .sections()[0];
        let mut z = [0.0f64; 2];
        let h: Vec<f64> = (0..1 << 21)
            .map(|n| {
                let x = if n == 0 { 1.0 } else { 0.0 };
                let y = b.b0 * x + z[0];
                z[0] = b.b1 * x - b.a1 * y + z[1];
                z[1] = b.b2 * x - b.a2 * y;
                y
            })
            .collect();
        let eps = f64::from(TAIL_FLOOR.to_amplitude().get());
        let mut sum = 0.0;
        for n in (0..h.len()).rev() {
            sum += h[n].abs();
            if sum > eps {
                return n + 1;
            }
        }
        0
    }

    /// For centers, Q and gains across the ranges the reported tail covers the first frame
    /// from which the impulse response's tail sum is under the floor.
    #[test]
    fn the_tail_covers_the_tail_sum_of_the_impulse_response() {
        let mut kernel = BellFilterKernel::new(filter_bus(1u16.into()));
        kernel.prepare(RATE, MaxBlock::of(Samples(64)));
        let frames = |t: Tail| match t {
            Tail::Finite(s) => s.get(),
            other => panic!("{other:?}"),
        };
        assert_eq!(kernel.tail(), Tail::Unbounded);
        for fc in [20.0f32, 90.0, 700.0, 4_000.0, 15_000.0, 22_000.0] {
            for q in [0.1f32, 0.7, 4.0, 30.0] {
                for db in [-24.0f32, -6.0, 6.0, 24.0] {
                    let controls = BellFilterControls {
                        frequency: Hz(fc),
                        q: Q(q),
                        gain: Db(db),
                    };
                    let reported = frames(kernel.tail_of(&controls));
                    let ring = ring_at(fc, q, db);
                    let tag = format!("fc {fc} q {q} gain {db}: reported {reported}, ring {ring}");
                    assert!(ring <= reported, "{tag}");
                }
            }
        }
    }

    /// The residue bound's figure for a bell, in frames, read from the design's poles:
    /// `1 + ceil(ln(S / eps) / -ln r)` for `S = (|A1| + |A2|) / (1 - r)` over
    /// `h[n] = sum A_k p_k^(n-1)`, or 1 when `S <= eps`.
    fn residue_frames(fc: f32, q: f32, db: f32) -> f64 {
        let b: Biquad = MatchedBell::new(Q(q), Db(db))
            .design(Hz(fc), RATE)
            .unwrap()
            .sections()[0];
        let [p1, p2] = b.poles();
        let r = p1.norm().max(p2.norm());
        let (c1, c0) = (b.b1 - b.b0 * b.a1, b.b2 - b.b0 * b.a2);
        let sum = ((p1 * c1 + c0).norm() + (p2 * c1 + c0).norm()) / (p1 - p2).norm() / (1.0 - r);
        let eps = f64::from(TAIL_FLOOR.to_amplitude().get());
        if sum <= eps {
            return 1.0;
        }
        1.0 + ((sum / eps).ln() / -r.ln()).ceil()
    }

    /// A narrow bell of low gain (Q 30, +6 dB at 90 Hz) and a wider one (Q 4, +24 dB at 90 Hz)
    /// report at most the residue bound's figure (the smaller bound of the two) and cover the
    /// tail sum of the f64 impulse response.
    #[test]
    fn the_reported_tail_is_at_most_the_residue_bound() {
        let mut kernel = BellFilterKernel::new(filter_bus(1u16.into()));
        kernel.prepare(RATE, MaxBlock::of(Samples(64)));
        for (fc, q, db) in [(90.0f32, 30.0f32, 6.0f32), (90.0, 4.0, 24.0)] {
            let Tail::Finite(reported) = kernel.tail_of(&BellFilterControls {
                frequency: Hz(fc),
                q: Q(q),
                gain: Db(db),
            }) else {
                panic!("not finite");
            };
            let bound = residue_frames(fc, q, db);
            assert!(ring_at(fc, q, db) <= reported.get(), "{fc} {q} {db}");
            assert!(
                reported.get() as f64 <= bound + 1.0,
                "{reported:?} vs {bound}"
            );
        }
    }
}
