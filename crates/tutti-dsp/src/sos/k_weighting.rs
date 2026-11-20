//! The K-weighting filter of ITU-R BS.1770: a high shelf and a high-pass in cascade,
//! designed at any rate.

use tutti_units::{Db, Hz, Q, SampleRate};

use super::butterworth::check_cutoff;
use super::{Sos, SosError};
use crate::Biquad;
use crate::design::prewarp;

/// The K-weighting of ITU-R BS.1770-5 (Annex 1): the pre-filter that models the head, a high
/// shelf of about +4 dB above 2 kHz, followed by the revised low-frequency B-curve, a
/// second-order high-pass near 38 Hz. Loudness is the mean square of a signal through it.
///
/// BS.1770 publishes the coefficients at 48 kHz only. They are the bilinear transform, with
/// the frequency prewarped at the corner, of two analog prototypes in `p = s / wc`:
///
/// ```text
///   shelf      H1(p) = (Vh p^2 + (Vb / Q) p + 1) / (p^2 + p / Q + 1)    Vh = 10^(G / 20)
///   high-pass  H2(p) = p^2 / (p^2 + p / Q + 1)
/// ```
///
/// so [`design`](Self::design) derives them at any rate from the corners, qualities and gain
/// below, and at 48 kHz they reproduce the published coefficients to 1e-13. The shelf is 0 dB
/// at DC and `G` dB well above its corner; `Vb = Vh^0.4996667741545416` sets its slope. The parameters are those of Brecht De Man's "Evaluation of
/// Implementations of the EBU R128 Loudness Measurement" (AES 145th Convention, 2018), who
/// recovered them from the published 48 kHz coefficients.
///
/// The shelf has a gain of 1 at DC, as its prototype does. The high-pass is the published
/// `b = (1, -2, 1)` over the normalized denominator, which is the prototype times `a0 = 1 + K /
/// Q + K^2` (`K = tan(pi f / fs)`), the exact figure BS.1770 prints at 48 kHz: its gain well above
/// the corner is `a0`, 0.04 dB at 48 kHz.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KWeighting;

impl KWeighting {
    /// The shelf's corner: 1681.974450955533 Hz.
    pub const SHELF_CORNER: Hz = Hz::new(1_681.974_450_955_533);
    /// The shelf's quality: 0.7071752369554196.
    pub const SHELF_Q: Q = Q::new(0.707_175_236_955_419_6);
    /// The shelf's gain above its corner: 3.999843853973347 dB.
    pub const SHELF_GAIN: Db = Db::new(3.999_843_853_973_347);
    /// The high-pass's corner: 38.13547087602444 Hz.
    pub const HIGH_PASS_CORNER: Hz = Hz::new(38.135_470_876_024_44);
    /// The high-pass's quality: 0.5003270373238773.
    pub const HIGH_PASS_Q: Q = Q::new(0.500_327_037_323_877_3);

    /// The exponent of `Vh` that gives the shelf's `Vb`.
    const SHELF_SLOPE: f64 = 0.499_666_774_154_541_6;

    /// The two sections, the shelf then the high-pass, at `rate`.
    ///
    /// # Errors
    ///
    /// [`SosError::CutoffOutOfRange`] for a rate whose Nyquist is at or under the shelf's
    /// corner (rates under 3.4 kHz).
    pub fn design(rate: SampleRate) -> Result<Sos, SosError> {
        let shelf = check_cutoff(Self::SHELF_CORNER, rate)?;
        let high_pass = check_cutoff(Self::HIGH_PASS_CORNER, rate)?;
        Ok(Sos::new(
            vec![
                shelf_section(shelf, rate),
                high_pass_section(high_pass, rate),
            ],
            rate,
        ))
    }
}

/// The shelf section: `H1` through the bilinear transform prewarped at `corner`.
fn shelf_section(corner: Hz, rate: SampleRate) -> Biquad {
    let k = prewarp(corner, rate);
    let q = KWeighting::SHELF_Q.get();
    let vh = KWeighting::SHELF_GAIN.to_amplitude().get();
    let vb = libm::pow(vh, KWeighting::SHELF_SLOPE);
    let a0 = 1.0 + k / q + k * k;
    Biquad {
        b0: (vh + vb * k / q + k * k) / a0,
        b1: 2.0 * (k * k - vh) / a0,
        b2: (vh - vb * k / q + k * k) / a0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / q + k * k) / a0,
    }
}

/// The high-pass section: `H2` through the bilinear transform prewarped at `corner`, with the
/// numerator `(1, -2, 1)` BS.1770 publishes.
fn high_pass_section(corner: Hz, rate: SampleRate) -> Biquad {
    let k = prewarp(corner, rate);
    let q = KWeighting::HIGH_PASS_Q.get();
    let a0 = 1.0 + k / q + k * k;
    Biquad {
        b0: 1.0,
        b1: -2.0,
        b2: 1.0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / q + k * k) / a0,
    }
}

#[cfg(test)]
mod tests;
