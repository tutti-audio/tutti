//! Windowed-sinc FIR design by the Kaiser method.
//!
//! A [`FirDesign`] describes the filter (its [`FirResponse`], a transition width
//! and a stopband attenuation); [`design`](FirDesign::design) binds a sample
//! rate, checks the settings against it and returns the taps as a [`Fir`].
//! Everything here runs on the control side.
//!
//! The length and the window shape come from Kaiser's formulas (Kaiser 1974,
//! as scipy's `kaiserord` and `firwin` use them; Oppenheim & Schafer,
//! *Discrete-Time Signal Processing*, 3rd ed., eq. 7.62 for `β`): for a stopband attenuation `A` dB and a
//! transition `Δω` rad/sample, `β = kaiser_beta(A)` and
//! `N = (A − 7.95) / (2.285 · Δω) + 1`, rounded up to an odd length so the
//! filter is type I (symmetric, a whole-sample delay of `(N − 1) / 2`).

use tutti_units::{Db, Hz, SampleRate, Samples};

mod linear_phase;

pub use linear_phase::{LinearPhaseDesign, MAX_GRID};

use crate::stft::window::KaiserWindow;

/// The longest filter [`FirDesign::design`] builds, in taps.
const MAX_LEN: usize = 1 << 20;

/// The attenuation a design has until [`FirDesign::attenuation`] sets it.
const DEFAULT_ATTENUATION: Db = Db::new(60.0);

/// What a [`FirDesign`] passes. Cutoffs are the midpoints of their transition
/// bands, in [`Hz`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FirResponse {
    /// Passes below `cutoff`.
    LowPass {
        /// The edge, in `(0, Nyquist)`.
        cutoff: Hz,
    },
    /// Passes above `cutoff`.
    HighPass {
        /// The edge, in `(0, Nyquist)`.
        cutoff: Hz,
    },
    /// Passes from `low` to `high`.
    BandPass {
        /// The lower edge, in `(0, high)`.
        low: Hz,
        /// The upper edge, in `(low, Nyquist)`.
        high: Hz,
    },
    /// Stops from `low` to `high`.
    BandStop {
        /// The lower edge, in `(0, high)`.
        low: Hz,
        /// The upper edge, in `(low, Nyquist)`.
        high: Hz,
    },
    /// A low-pass at a quarter of the sample rate whose taps at even offsets
    /// from the centre, other than the centre, are exactly zero, so a
    /// convolution skips half its multiplies. The length is `4m + 3`.
    HalfBand,
}

/// The most stopband attenuation a design takes: `f64` taps resolve about
/// 313 dB below the passband (their epsilon), so a deeper stopband is not
/// representable.
pub const MAX_ATTENUATION: Db = Db::new(300.0);

/// A windowed-sinc FIR filter, described without a sample rate.
///
/// Built as `FirDesign::new(response, transition)` and the option
/// [`attenuation`](Self::attenuation); [`design`](Self::design) binds the rate.
///
/// Each response is scaled the way scipy's `firwin(scale=True)` scales it: a
/// low-pass, band-stop and the half-band to a DC gain of 1, a high-pass to a
/// gain of 1 at Nyquist, a band-pass to a gain of 1 at the middle of its
/// passband.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FirDesign {
    response: FirResponse,
    transition: Hz,
    attenuation: Db,
}

impl FirDesign {
    /// A filter with `response` whose transition bands are `transition` wide,
    /// each centred on its cutoff. The stopband attenuation is 60 dB until
    /// [`attenuation`](Self::attenuation) sets it.
    #[must_use]
    pub fn new(response: FirResponse, transition: Hz) -> Self {
        Self {
            response,
            transition,
            attenuation: DEFAULT_ATTENUATION,
        }
    }

    /// The stopband attenuation, in positive dB: the stopband sits at about
    /// `-attenuation` and the passband ripple is about `10^(-attenuation/20)`.
    /// Kaiser's formulas are estimates, so a design can miss either by a dB or
    /// two, most with a wide transition.
    #[must_use]
    pub fn attenuation(mut self, attenuation: Db) -> Self {
        self.attenuation = attenuation;
        self
    }

    /// Designs the filter at `rate`.
    ///
    /// # Errors
    ///
    /// - [`FirError::CutoffOutOfRange`] for a cutoff outside `(0, Nyquist)`.
    /// - [`FirError::CutoffsUnordered`] for a band whose `low` is not below its
    ///   `high`.
    /// - [`FirError::NonPositiveTransition`] or
    ///   [`FirError::NonPositiveAttenuation`] for a width or attenuation that is
    ///   not a positive, finite number.
    /// - [`FirError::TransitionTooWide`] when a transition band does not fit
    ///   between its edge and 0, Nyquist or the neighbouring edge's band.
    /// - [`FirError::AttenuationTooHigh`] past [`MAX_ATTENUATION`], which `f64`
    ///   taps cannot resolve.
    /// - [`FirError::TooLong`] when the filter would exceed `2^20` taps.
    pub fn design(&self, rate: SampleRate) -> Result<Fir, FirError> {
        let nyquist_hz = rate.nyquist();
        let nyquist = nyquist_hz.get();
        let transition = self.transition.get();
        let attenuation = self.attenuation.get();
        if !(transition.is_finite() && transition > 0.0) {
            return Err(FirError::NonPositiveTransition(self.transition));
        }
        if !(attenuation.is_finite() && attenuation > 0.0) {
            return Err(FirError::NonPositiveAttenuation(self.attenuation));
        }
        if attenuation > MAX_ATTENUATION.get() {
            return Err(FirError::AttenuationTooHigh {
                attenuation: self.attenuation,
                max: MAX_ATTENUATION,
            });
        }

        let inside = |f: Hz| {
            let f = f.get();
            f > 0.0 && f < nyquist
        };
        let edges: Vec<f64> = match self.response {
            FirResponse::LowPass { cutoff } | FirResponse::HighPass { cutoff } => {
                if !inside(cutoff) {
                    return Err(FirError::CutoffOutOfRange {
                        cutoff,
                        nyquist: nyquist_hz,
                    });
                }
                vec![cutoff.get()]
            }
            FirResponse::BandPass { low, high } | FirResponse::BandStop { low, high } => {
                for cutoff in [low, high] {
                    if !inside(cutoff) {
                        return Err(FirError::CutoffOutOfRange {
                            cutoff,
                            nyquist: nyquist_hz,
                        });
                    }
                }
                if low >= high {
                    return Err(FirError::CutoffsUnordered { low, high });
                }
                vec![low.get(), high.get()]
            }
            FirResponse::HalfBand => vec![nyquist / 2.0],
        };

        // The widest transition each neighbour allows: twice the distance to
        // 0 or Nyquist, and the gap between two edges.
        let mut widest = f64::INFINITY;
        for (i, &edge) in edges.iter().enumerate() {
            widest = widest.min(2.0 * edge).min(2.0 * (nyquist - edge));
            if let Some(&next) = edges.get(i + 1) {
                widest = widest.min(next - edge);
            }
        }
        if transition > widest {
            return Err(FirError::TransitionTooWide {
                transition: self.transition,
                widest: Hz::new(widest),
            });
        }

        let delta_omega = self.transition.angle_per_frame(rate).get();
        let mut len = odd_up(kaiser_order(attenuation, delta_omega));
        if matches!(self.response, FirResponse::HalfBand) {
            // The centre's offset (len - 1) / 2 is odd, so the outermost taps
            // are not the zeros.
            while len % 4 != 3 {
                len += 2;
            }
        }
        if len > MAX_LEN {
            return Err(FirError::TooLong { len, max: MAX_LEN });
        }

        let omega: Vec<f64> = edges
            .iter()
            .map(|f| Hz::new(*f).angle_per_frame(rate).get())
            .collect();
        Ok(Fir {
            taps: self.taps(len, &omega, kaiser_beta(self.attenuation)),
        })
    }

    /// The taps of a `len`-tap filter whose edges are `omega` rad/sample:
    /// the ideal response, the Kaiser window, then the scaling.
    fn taps(&self, len: usize, omega: &[f64], beta: f64) -> Vec<f64> {
        use std::f64::consts::PI;
        let m = (len - 1) / 2;
        let bands: Vec<(f64, f64)> = match self.response {
            FirResponse::LowPass { .. } | FirResponse::HalfBand => vec![(0.0, omega[0])],
            FirResponse::HighPass { .. } => vec![(omega[0], PI)],
            FirResponse::BandPass { .. } => vec![(omega[0], omega[1])],
            FirResponse::BandStop { .. } => vec![(0.0, omega[0]), (omega[1], PI)],
        };
        let half_band = matches!(self.response, FirResponse::HalfBand);
        let kaiser = KaiserWindow::new(beta);

        // `right[k]` is the tap k samples from the centre; the left half is its
        // mirror, so the taps are symmetric bit for bit.
        let mut right: Vec<f64> = (0..=m)
            .map(|k| {
                if half_band && k != 0 && k % 2 == 0 {
                    return 0.0;
                }
                let kf = k as f64;
                let ideal: f64 = bands
                    .iter()
                    .map(|&(a, b)| {
                        if k == 0 {
                            (b - a) / PI
                        } else {
                            (sin_edge(b, kf) - sin_edge(a, kf)) / (PI * kf)
                        }
                    })
                    .sum();
                ideal * kaiser.centred(kf / m as f64 / 2.0)
            })
            .collect();

        if half_band {
            // The odd taps carry half the DC gain, the centre the other half.
            let odd: f64 = right.iter().skip(1).step_by(2).sum();
            for tap in right.iter_mut().skip(1).step_by(2) {
                *tap *= 0.25 / odd;
            }
            right[0] = 0.5;
        } else {
            let reference = match self.response {
                FirResponse::HighPass { .. } => PI,
                FirResponse::BandPass { .. } => 0.5 * (omega[0] + omega[1]),
                _ => 0.0,
            };
            let gain = right[0]
                + 2.0
                    * right
                        .iter()
                        .enumerate()
                        .skip(1)
                        .map(|(k, tap)| tap * libm::cos(reference * k as f64))
                        .sum::<f64>();
            for tap in &mut right {
                *tap /= gain;
            }
        }

        let mut taps: Vec<f64> = right.iter().skip(1).rev().copied().collect();
        taps.extend(right);
        taps
    }
}

/// `sin(edge · k)`, exactly zero at the edges 0 and π (every integer `k`).
fn sin_edge(edge: f64, k: f64) -> f64 {
    if edge == 0.0 || edge == std::f64::consts::PI {
        0.0
    } else {
        libm::sin(edge * k)
    }
}

/// The designed taps of a windowed-sinc FIR filter.
///
/// The length is odd and the taps are symmetric, so the phase is linear and the
/// delay a whole number of samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Fir {
    taps: Vec<f64>,
}

impl Fir {
    /// The taps, in order from the first to reach the output: filter
    /// coefficients, designed in `f64`, which a caller converts to the format
    /// its signal runs in.
    pub fn taps(&self) -> impl ExactSizeIterator<Item = f64> + '_ {
        self.taps.iter().copied()
    }

    /// The number of taps; always odd.
    #[must_use]
    pub fn len(&self) -> Samples {
        Samples::new(self.taps.len())
    }

    /// The group delay, `(len - 1) / 2` samples at every frequency.
    #[must_use]
    pub fn delay(&self) -> Samples {
        Samples::new((self.taps.len() - 1) / 2)
    }
}

/// What [`FirDesign::design`] refuses.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum FirError {
    /// A cutoff at or below 0 Hz, at or above Nyquist, or not a number.
    #[error("cutoff {cutoff:?} is not inside (0, {nyquist:?})")]
    CutoffOutOfRange {
        /// The rejected cutoff.
        cutoff: Hz,
        /// The Nyquist frequency of the rate.
        nyquist: Hz,
    },
    /// A band whose lower edge is not below its upper edge.
    #[error("band edges {low:?} and {high:?} are not in increasing order")]
    CutoffsUnordered {
        /// The lower edge.
        low: Hz,
        /// The upper edge.
        high: Hz,
    },
    /// A transition width that is not positive and finite.
    #[error("transition width {0:?} is not positive")]
    NonPositiveTransition(Hz),
    /// A stopband attenuation that is not positive and finite.
    #[error("stopband attenuation {0:?} is not positive")]
    NonPositiveAttenuation(Db),
    /// A stopband attenuation past [`MAX_ATTENUATION`]: `f64` taps resolve
    /// about 313 dB below the passband, and past it the window's Bessel term
    /// overflows.
    #[error("stopband attenuation {attenuation:?} is past the {max:?} f64 taps resolve")]
    AttenuationTooHigh {
        /// The attenuation asked for.
        attenuation: Db,
        /// The most a design takes.
        max: Db,
    },
    /// A transition band that does not fit between its edge and 0, Nyquist or
    /// the next edge.
    #[error("transition {transition:?} does not fit; the widest here is {widest:?}")]
    TransitionTooWide {
        /// The requested width.
        transition: Hz,
        /// The widest width the edges allow.
        widest: Hz,
    },
    /// A resolution that is not positive and finite.
    #[error("resolution {0:?} is not positive")]
    NonPositiveResolution(Hz),
    /// A resolution at or above the Nyquist frequency, which no length resolves.
    #[error("resolution {resolution:?} is not below the Nyquist frequency {nyquist:?}")]
    ResolutionTooWide {
        /// The resolution asked for.
        resolution: Hz,
        /// The Nyquist frequency of the rate.
        nyquist: Hz,
    },
    /// A band that is not one input and at least one output.
    #[error("a band has one input and an output, not {inputs} in and {outputs} out")]
    BandShape {
        /// The band's inputs.
        inputs: usize,
        /// The band's outputs.
        outputs: usize,
    },
    /// A target magnitude with a value that is not finite, or a band with no response.
    #[error("the target magnitude has a value that is not finite")]
    NonFiniteTarget,
    /// Bands that ring too long to sample on the largest grid, or never decay.
    #[error("the bands need a grid of {needed} points; the most is {max}")]
    GridTooLarge {
        /// The points needed, `usize::MAX` for bands that do not decay.
        needed: usize,
        /// The most a design takes, [`MAX_GRID`].
        max: usize,
    },
    /// A filter longer than the supported maximum.
    #[error("a filter of {len} taps exceeds the {max} supported")]
    TooLong {
        /// The length the settings need.
        len: usize,
        /// The longest length built.
        max: usize,
    },
}

/// The Kaiser window parameter `β` for a stopband attenuation of
/// `attenuation` dB (Kaiser 1974; Oppenheim & Schafer eq. 7.62): `0.1102 (A −
/// 8.7)` above 50 dB, `0.5842 (A − 21)^0.4 + 0.07886 (A − 21)` from 21 to
/// 50 dB, and 0 below 21 dB.
#[must_use]
pub fn kaiser_beta(attenuation: Db) -> f64 {
    let a = attenuation.get();
    if a > 50.0 {
        0.1102 * (a - 8.7)
    } else if a >= 21.0 {
        0.5842 * libm::pow(a - 21.0, 0.4) + 0.07886 * (a - 21.0)
    } else {
        0.0
    }
}

/// Kaiser's estimate of the filter length for `attenuation` dB and a
/// transition of `delta_omega` rad/sample (Kaiser 1974):
/// `(A − 7.95) / (2.285 Δω) + 1`, rounded up.
fn kaiser_order(attenuation: f64, delta_omega: f64) -> usize {
    let n = (attenuation - 7.95) / (2.285 * delta_omega) + 1.0;
    // A small or negative estimate still yields a filter of three taps.
    libm::ceil(n.max(3.0)) as usize
}

/// `len` if odd, else `len + 1`.
fn odd_up(len: usize) -> usize {
    len | 1
}

#[cfg(test)]
mod tests;
