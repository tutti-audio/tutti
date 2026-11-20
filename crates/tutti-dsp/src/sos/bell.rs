//! The matched bell: a peaking equalizer whose digital magnitude is the analog one at DC,
//! at the center and at Nyquist.

use tutti_units::{Amplitude, Db, Hz, Q, SampleRate, Seconds};

use super::MAX_TAIL_FRAMES;
use super::butterworth::{check_cutoff, check_gain, check_q, gain_root};
use super::{Sos, SosError};
use crate::{Biquad, DecayTime};

/// A peaking equalizer, described without a sample rate: the analog bell
/// `H(s) = (s^2 + s A / Q + 1) / (s^2 + s / (A Q) + 1)` in `s / w0`, for the center `w0`,
/// the quality factor `Q` and `A = 10^(gain / 40)`, a gain of `A^2` at the center and 1
/// far from it.
///
/// The digital filter is M. Vicanek's "Matched Second Order Digital Filters" (2016): the
/// poles are the analog poles by impulse invariance, and the numerator is the one whose
/// magnitude equals the analog one at DC, at the center and at Nyquist. A bilinear bell
/// (the cookbook's) is cramped near Nyquist; this one keeps the analog gain there, and a
/// center at a large fraction of the rate keeps its shape. A cut is the exact inverse of the
/// boost of the same gain, which matches the analog cut at all three points because the
/// analog bell is reciprocal under `A -> 1 / A`; the two cancel exactly.
///
/// Built as `MatchedBell::new(q, gain)`; [`design`](Self::design) binds a center and the rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchedBell {
    q: Q,
    gain: Db,
}

impl MatchedBell {
    /// A bell of quality factor `q` and `gain` at the center: a boost for a positive gain,
    /// a cut for a negative one, a wire at 0 dB.
    #[must_use]
    pub fn new(q: Q, gain: Db) -> Self {
        Self { q, gain }
    }

    /// Designs the bell at `center` and `rate`: one section.
    ///
    /// # Errors
    ///
    /// [`SosError::CutoffOutOfRange`] for a center outside `(0, Nyquist)`,
    /// [`SosError::QOutOfRange`] for a quality factor that is not finite and above 0, and
    /// [`SosError::GainOutOfRange`] for a gain beyond [`MAX_GAIN_MAGNITUDE`](super::MAX_GAIN_MAGNITUDE).
    pub fn design(&self, center: Hz, rate: SampleRate) -> Result<Sos, SosError> {
        let mut sos = Sos::new(Vec::with_capacity(1), rate);
        self.design_into(center, rate, &mut sos)?;
        Ok(sos)
    }

    /// [`design`](Self::design) into `into`, reusing its sections' storage: no allocation
    /// when it holds room for one section.
    ///
    /// # Errors
    ///
    /// As [`design`](Self::design); `into` is then as it was.
    pub fn design_into(
        &self,
        center: Hz,
        rate: SampleRate,
        into: &mut Sos,
    ) -> Result<(), SosError> {
        let section = self.section(center, rate)?;
        into.rate = rate;
        into.sections.clear();
        into.sections.push(section);
        Ok(())
    }

    /// The time after which the bell's output to any input in `[-1, 1]` that stops stays
    /// under `floor` (an amplitude, for full-scale input), a bound in closed form from the
    /// designed section: the first frame `n` with `sum_{m >= n} |h[m]| <= floor`, the
    /// smaller of two bounds on the tail sum.
    ///
    /// The residue bound writes `h[m] = sum A_k p_k^(m-1)` for `m >= 1` over the distinct
    /// poles `p_k`, with `A_k = (c1 p_k + c0) / (p_k - p_other)`, `c1 = b1 - b0 a1` and
    /// `c0 = b2 - b0 a2`, so the tail sum is at most `(|A_1| + |A_2|) r^(n-1) / (1 - r)` for
    /// the larger pole modulus `r`. The repeated-root bound holds for any poles: `|h[m]| <=
    /// K (m + 1) r^(m-2)` for `m >= 2` with `K = |b0| + |b1| + |b2|`, from the all-pole
    /// response `sum p1^k p2^(m-k)`, whose tail sum is `K r^(n-2) ((n + 1) / (1 - r) + r /
    /// (1 - r)^2)`, solved for `n` by doubling and bisection. [`DecayTime::Unbounded`] for a pole
    /// that is not inside the unit circle, a ring-out past `2^24` frames or a floor that is not a
    /// number or is infinite. A cut rings as
    /// the boost's zeros do.
    ///
    /// # Errors
    ///
    /// As [`design`](Self::design).
    pub fn decay_time(
        &self,
        center: Hz,
        rate: SampleRate,
        floor: Db,
    ) -> Result<DecayTime, SosError> {
        let b = self.section(center, rate)?;
        let eps = floor.to_amplitude();
        Ok(match ring_frames(&b, eps) {
            Some(frames)
                if eps.get().is_finite() && eps.get() > 0.0 && frames <= MAX_TAIL_FRAMES as f64 =>
            {
                DecayTime::Finite(Seconds::new(frames / rate.get()))
            }
            _ => DecayTime::Unbounded,
        })
    }

    /// The matched section at `center` and `rate`.
    fn section(&self, center: Hz, rate: SampleRate) -> Result<Biquad, SosError> {
        let fc = check_cutoff(center, rate)?;
        let q = check_q(self.q)?;
        let db = check_gain(self.gain)?;
        let boost = matched(fc, q, Db::new(db.get().abs()), rate);
        Ok(if db.get() < 0.0 {
            inverse(boost)
        } else {
            boost
        })
    }
}

/// The first frame `n` from which `sum |h[m]|` of `b` is at most `eps` for good, by the smaller
/// of the residue and the repeated-root bounds ([`MatchedBell::decay_time`]); `None` for a pole
/// not inside the unit circle.
fn ring_frames(b: &Biquad, eps: Amplitude) -> Option<f64> {
    let eps = eps.get();
    let [p1, p2] = b.poles();
    let r = p1.norm().max(p2.norm());
    if r.is_nan() || r >= 1.0 {
        return None;
    }
    let slack = 1.0 - r;
    let decay = -libm::log(r.max(1e-300));
    // Repeated roots, `n >= 2`: `(n - 2) decay - ln((n + 1) / (1 - r) + r / (1 - r)^2)` rises
    // from `n >= 1 / decay` and must reach `ln(K / eps)`.
    let need = libm::log((b.b0.abs() + b.b1.abs() + b.b2.abs()) / eps);
    let rise = |n: f64| (n - 2.0) * decay - libm::log((n + 1.0) / slack + r / (slack * slack));
    let (mut lo, mut hi) = (0.0, libm::ceil(1.0 / decay).max(2.0));
    while rise(hi) < need && hi < 1e12 {
        lo = hi;
        hi *= 2.0;
    }
    for _ in 0..64 {
        let mid = libm::floor((lo + hi) / 2.0);
        if mid <= lo {
            break;
        }
        if rise(mid) >= need {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let repeated = hi;
    // Distinct roots, `n >= 1`: `h[m] = sum A_k p_k^(m-1)`.
    let gap = (p1 - p2).norm();
    let (c1, c0) = (b.b1 - b.b0 * b.a1, b.b2 - b.b0 * b.a2);
    let residues = ((p1 * c1 + c0).norm() + (p2 * c1 + c0).norm()) / gap / slack;
    let distinct = if gap > 1e-6 && residues.is_finite() {
        if residues <= eps {
            1.0
        } else {
            1.0 + libm::ceil(libm::log(residues / eps) / decay)
        }
    } else {
        f64::INFINITY
    };
    Some(repeated.min(distinct))
}

/// The inverse of the minimum-phase section `b`: numerator and denominator swapped. The analog
/// cut is the reciprocal of the analog boost of the same gain, so the matched cut is the
/// matched boost inverted; a cut and a boost of the same magnitude cancel exactly.
fn inverse(b: Biquad) -> Biquad {
    Biquad {
        b0: 1.0 / b.b0,
        b1: b.a1 / b.b0,
        b2: b.a2 / b.b0,
        a1: b.b1 / b.b0,
        a2: b.b2 / b.b0,
    }
}

/// The matched boost section for the center `fc`, quality factor `q` and `db >= 0` of gain
/// at `fs`.
///
/// The denominator is the impulse-invariant image of the analog poles: damping `zeta = 1 /
/// (2 A q)`, radius `exp(-zeta w0)`, angle `w0 sqrt(1 - zeta^2)`, and two real poles for
/// `zeta > 1`.
///
/// Let `p0 = cos^2(w/2)`, `p1 = sin^2(w/2)` and `p2 = 4 p0 p1`. The squared magnitude of
/// `c0 + c1 z^-1 + c2 z^-2` is `C0 p0 + C1 p1 + C2 p2` with `C0 = (c0 + c1 + c2)^2`,
/// `C1 = (c0 + c2 - c1)^2` and `C2 = -4 c0 c2`. The numerator's `C` follow from three
/// conditions: `C0` is the denominator's `B0` (unit gain at DC), `C1` is the analog squared
/// gain at Nyquist times `B1`, and `C2` makes the response at `w0` the squared center gain
/// `A^4`. Then `c0 + c2` is half the sum of `sqrt(C0)` and `sqrt(C1)`, `c1` is half their
/// difference and `c0 c2 = -C2 / 4`; `c0` is the larger root, which puts the zeros inside
/// the circle.
fn matched(fc: Hz, q: Q, db: Db, rate: SampleRate) -> Biquad {
    let w0 = fc.angle_per_frame(rate).get();
    let (fc, q, fs) = (fc.get(), q.get(), rate.get());
    let a = gain_root(db);
    let zeta = 1.0 / (2.0 * a * q);
    let decay = libm::exp(-zeta * w0);
    let shape = if zeta < 1.0 {
        libm::cos(w0 * libm::sqrt(1.0 - zeta * zeta))
    } else {
        libm::cosh(w0 * libm::sqrt(zeta * zeta - 1.0))
    };
    let (a1, a2) = (-2.0 * decay * shape, decay * decay);
    // The squared magnitudes `B` of the denominator and the analog ones to match.
    let (u, d) = (1.0 + a1 + a2, 1.0 - a1 + a2);
    let (b0, b1, b2) = (u * u, d * d, -4.0 * a2);
    let omega = fs / (2.0 * fc);
    let off = 1.0 - omega * omega;
    let nyquist =
        (off * off + libm::pow(a * omega / q, 2.0)) / (off * off + libm::pow(omega / (a * q), 2.0));
    let center = libm::pow(a, 4.0);
    let (p1, p0) = (
        libm::pow(libm::sin(w0 / 2.0), 2.0),
        libm::pow(libm::cos(w0 / 2.0), 2.0),
    );
    let p2 = 4.0 * p0 * p1;
    let c2 = (center * (b0 * p0 + b1 * p1 + b2 * p2) - b0 * p0 - nyquist * b1 * p1) / p2;
    let v = libm::sqrt(nyquist) * d;
    let sum = (u + v) / 2.0;
    let product = -c2 / 4.0;
    let lead = (sum + libm::sqrt((sum * sum - 4.0 * product).max(0.0))) / 2.0;
    Biquad {
        b0: lead,
        b1: (u - v) / 2.0,
        b2: product / lead,
        a1,
        a2,
    }
}
