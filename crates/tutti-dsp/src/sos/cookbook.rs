//! The Audio EQ Cookbook designs: the eight second-order sections of Robert Bristow-Johnson's
//! W3C note, each the bilinear transform of an analog prototype with the center prewarped
//! into the cookbook's `alpha = sin(w0) / (2 Q)` form.

use tutti_units::{Db, Hz, Q, SampleRate};

use super::butterworth::{check_cutoff, check_gain, check_q, gain_root};
use super::{Sos, SosError};
use crate::Biquad;

/// One of the Audio EQ Cookbook's second-order filters, described without a center or a
/// sample rate; [`design`](Self::design) binds both.
///
/// Every design takes the cookbook's `Q` (`alpha = sin(w0) / (2 Q)`) and the ones with a
/// gain take it as `A = 10^(gain / 40)`, the cookbook's `dBgain`. The numbers are the
/// cookbook's, normalized by `a0`.
///
/// [`Peaking`](Self::Peaking) is the cookbook's bilinear bell. It is a different filter from
/// [`MatchedBell`](super::MatchedBell): both have the gain at the center and 0 dB at DC, but
/// the bilinear one is warped toward Nyquist (its magnitude there is not the analog bell's)
/// and is exactly a wire at 0 dB.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Cookbook {
    /// Low-pass: a gain of 1 at DC, `Q` at the center (`-3.01 dB` for `Q = 1/sqrt(2)`) and
    /// 0 at Nyquist.
    LowPass {
        /// The quality factor.
        q: Q,
    },
    /// High-pass: 0 at DC, a gain of 1 at Nyquist and `Q` at the center.
    HighPass {
        /// The quality factor.
        q: Q,
    },
    /// Band-pass with a constant 0 dB peak gain: exactly 1 at the center, 0 at DC and at
    /// Nyquist.
    BandPass {
        /// The quality factor.
        q: Q,
    },
    /// Notch: a gain of 1 at DC and at Nyquist and a zero at the center.
    Notch {
        /// The quality factor.
        q: Q,
    },
    /// All-pass: a gain of 1 everywhere, the phase `-pi` at the center.
    AllPass {
        /// The quality factor.
        q: Q,
    },
    /// Peaking equalizer: `gain` at the center, 1 at DC and at Nyquist.
    Peaking {
        /// The quality factor.
        q: Q,
        /// The gain at the center, a boost above 0 dB and a cut below.
        gain: Db,
    },
    /// Low shelf: `gain` at DC and 1 at Nyquist, the shelf's midpoint at the center.
    LowShelf {
        /// The quality factor: `alpha = sin(w0) / (2 Q)` in the cookbook's shelf form.
        q: Q,
        /// The gain below the shelf.
        gain: Db,
    },
    /// High shelf: 1 at DC and `gain` at Nyquist, the shelf's midpoint at the center.
    HighShelf {
        /// The quality factor: `alpha = sin(w0) / (2 Q)` in the cookbook's shelf form.
        q: Q,
        /// The gain above the shelf.
        gain: Db,
    },
}

impl Cookbook {
    /// Designs the filter at `center` and `rate`: one section.
    ///
    /// # Errors
    ///
    /// [`SosError::CutoffOutOfRange`] for a center outside `(0, Nyquist)`,
    /// [`SosError::QOutOfRange`] for a quality factor that is not finite and above 0, and
    /// [`SosError::GainOutOfRange`] for a gain beyond
    /// [`MAX_GAIN_MAGNITUDE`](super::MAX_GAIN_MAGNITUDE).
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

    /// The cookbook section at `center` and `rate`.
    pub(super) fn section(&self, center: Hz, rate: SampleRate) -> Result<Biquad, SosError> {
        let fc = check_cutoff(center, rate)?;
        let (q, gain) = match *self {
            Self::LowPass { q }
            | Self::HighPass { q }
            | Self::BandPass { q }
            | Self::Notch { q }
            | Self::AllPass { q } => (q, Db::UNITY),
            Self::Peaking { q, gain }
            | Self::LowShelf { q, gain }
            | Self::HighShelf { q, gain } => (q, check_gain(gain)?),
        };
        let q = check_q(q)?;
        let w0 = fc.angle_per_frame(rate).get();
        let (sin, cos) = (libm::sin(w0), libm::cos(w0));
        let alpha = sin / (2.0 * q.get());
        let a = gain_root(gain);
        // `[b0, b1, b2]` and `[a0, a1, a2]` of the cookbook.
        let (b, d) = match self {
            Self::LowPass { .. } => (
                [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
                [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
            ),
            Self::HighPass { .. } => (
                [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
                [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
            ),
            Self::BandPass { .. } => ([alpha, 0.0, -alpha], [1.0 + alpha, -2.0 * cos, 1.0 - alpha]),
            Self::Notch { .. } => (
                [1.0, -2.0 * cos, 1.0],
                [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
            ),
            Self::AllPass { .. } => (
                [1.0 - alpha, -2.0 * cos, 1.0 + alpha],
                [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
            ),
            Self::Peaking { .. } => (
                [1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a],
                [1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a],
            ),
            Self::LowShelf { .. } => {
                let k = 2.0 * libm::sqrt(a) * alpha;
                (
                    [
                        a * ((a + 1.0) - (a - 1.0) * cos + k),
                        2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                        a * ((a + 1.0) - (a - 1.0) * cos - k),
                    ],
                    [
                        (a + 1.0) + (a - 1.0) * cos + k,
                        -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                        (a + 1.0) + (a - 1.0) * cos - k,
                    ],
                )
            }
            Self::HighShelf { .. } => {
                let k = 2.0 * libm::sqrt(a) * alpha;
                (
                    [
                        a * ((a + 1.0) + (a - 1.0) * cos + k),
                        -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                        a * ((a + 1.0) + (a - 1.0) * cos - k),
                    ],
                    [
                        (a + 1.0) - (a - 1.0) * cos + k,
                        2.0 * ((a - 1.0) - (a + 1.0) * cos),
                        (a + 1.0) - (a - 1.0) * cos - k,
                    ],
                )
            }
        };
        Ok(Biquad {
            b0: b[0] / d[0],
            b1: b[1] / d[0],
            b2: b[2] / d[0],
            a1: d[1] / d[0],
            a2: d[2] / d[0],
        })
    }
}
