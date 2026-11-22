//! Take alignment and delay finding by GCC-PHAT.
//!
//! The generalised cross-correlation with the phase transform (Knapp and
//! Carter, 1976): the cross-spectrum `A * conj(B)` of the two takes is divided
//! by its own magnitude before the inverse transform, so every frequency counts
//! equally and only phase, which carries the delay, is left. Plain
//! cross-correlation weights bins by energy, so a loud hum or a strongly
//! coloured source smears or displaces the peak; PHAT leaves a sharp one.
//! The weighting whitens across about 40 dB of per-input dynamic range below
//! the strongest in-band bin; bins under that are damped, not boosted. A hum
//! stronger than that over the rest of the spectrum belongs outside a band.
//!
//! An offline or UI-thread API: it allocates the padded spectra and the
//! correlation on every call, through [`cross_correlate_shaped`].

use tutti_buffers::{Mono, Sample};
use tutti_units::{Confidence, SampleRate, Samples, Seconds};

use crate::AlignError;
use crate::analyses::xcorr::{CrossCorrelation, cross_correlate_shaped};
use crate::convert::widen;
use crate::framing::check_nyquist;
use crate::meters::chroma::FrequencyBand;

use tutti_dsp::Complex64;
use tutti_dsp::FftScratch;
use tutti_dsp::parabolic_peak;

/// A bin whose cross-spectrum magnitude is below this fraction of the
/// strongest in-band bin is damped rather than boosted to unit magnitude.
/// Relative, so the weighting does not depend on the inputs' level. The
/// magnitude is a product of two amplitudes, so this is a ratio of
/// `sqrt(1e-4)`, 40 dB, per input. Lower, and the spectral leakage of a
/// truncated sinusoid (about 50 dB down) is whitened into the result and
/// biases the sub-sample lag; higher, and a hum 40 dB over the rest reduces
/// the weighting to plain correlation.
const PHAT_FLOOR: f64 = 1e-4;

/// The share of the peak below which the peak's lobe is taken to have ended.
const LOBE_FRACTION: f64 = 0.5;

/// What [`align`] searches and weights. Free of a sample rate: [`align`] takes
/// it and checks the band against its Nyquist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlignConfig {
    max_lag: Samples,
    band: Option<FrequencyBand>,
}

impl AlignConfig {
    /// Search lags `-max_lag..=max_lag`, over the whole spectrum.
    #[must_use]
    pub fn new(max_lag: Samples) -> Self {
        Self {
            max_lag,
            band: None,
        }
    }

    /// Weight only bins inside `band`, leaving the rest out of the
    /// correlation: a band where both takes carry the same source, away from
    /// rumble or hiss that does not. A band's low edge is above 0 (a
    /// [`FrequencyBand`] holds no zero), so a low-pass-only band is written with
    /// a small low edge, which drops the DC bin from the PHAT weighting.
    /// [`align`] refuses a band whose top exceeds half the sample rate
    /// ([`AboveNyquist`](crate::AboveNyquist)).
    #[must_use]
    pub fn band(mut self, band: FrequencyBand) -> Self {
        self.band = Some(band);
        self
    }
}

/// Where a take sits against its reference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alignment {
    /// The whole-sample lag: positive means the take is a **delayed** copy of
    /// the reference, so shifting it earlier by this much aligns it.
    pub lag: isize,
    /// The sub-sample correction to `lag`, in samples, within `-0.5..=0.5`.
    /// A parabola through the peak and its two neighbours; zero when a
    /// neighbour is outside the overlap or the three do not bend down.
    ///
    /// A bare `f64`: [`Samples`] is a whole-sample count, and a fraction of a
    /// sample has no unit type of its own.
    pub offset: f64,
    /// How distinct the peak is: `1 - second / peak`, where `second` is the
    /// strongest value in the search window outside the peak's own lobe (the
    /// run around it that stays above half its height, down to the next local
    /// minimum). A reading of how clearly one lag wins, not a probability:
    /// near one for a sharp peak over a quiet floor, near zero for unrelated
    /// signals, and zero when the window holds no competitor (a bound of
    /// zero, or a lobe that fills it).
    pub confidence: Confidence,
}

impl Alignment {
    /// The delay in samples, `lag + offset`.
    ///
    /// The sum is taken in `f64`, so a long lag keeps its sub-sample offset.
    #[inline]
    #[must_use]
    pub fn delay_samples(&self) -> f64 {
        self.lag as f64 + self.offset
    }

    /// The delay in seconds at `sample_rate`.
    ///
    /// [`Seconds`] is `f64`, finer than the sub-sample estimate itself.
    #[inline]
    #[must_use]
    pub fn delay(&self, sample_rate: impl Into<SampleRate>) -> Seconds {
        Seconds::new(self.delay_samples() / sample_rate.into().get())
    }
}

/// Finds how late `take` is against `reference`, by GCC-PHAT.
///
/// The sub-sample [`Alignment::offset`] is a parabolic fit: on a signal that
/// fills the band (about one partial for every few bins of the spectrum, as
/// noise or dense music does) it is good to about a tenth of a sample, and it
/// degrades as the signal narrows or noise rises. A few exact tones do not
/// fill it: the phase transform whitens their truncation leakage to the same
/// weight as the tones, and the leakage does not move with the delay.
/// [`cross_correlate`](crate::cross_correlate) is the tool for tonal material. The lag is searched over
/// `-max_lag..=max_lag`, clamped to the overlap.
///
/// # Errors
///
/// [`AboveNyquist`](crate::AboveNyquist) when the band's top exceeds half `rate`;
/// [`AlignError::InsufficientInput`] when either buffer is empty;
/// [`AlignError::NonFiniteInput`] when a sample is NaN or infinite;
/// [`AlignError::NoSignal`] when there is nothing to correlate: a silent input, or
/// a band that holds no spectrum bin.
pub fn align<S: Sample>(
    config: &AlignConfig,
    reference: Mono<'_, S>,
    take: Mono<'_, S>,
    rate: impl Into<SampleRate>,
) -> Result<Alignment, AlignError> {
    let rate = rate.into();
    if let Some(band) = config.band {
        check_nyquist(band.max(), rate)?;
    }
    let (reference, take) = (widen(reference.samples()), widen(take.samples()));
    let (reference, take) = (&reference[..], &take[..]);
    for buffer in [reference, take] {
        if buffer.is_empty() {
            return Err(AlignError::InsufficientInput {
                needed: Samples::new(1),
                got: Samples::new(0),
            });
        }
        if !buffer.iter().all(|v| v.is_finite()) {
            return Err(AlignError::NonFiniteInput);
        }
    }
    let mut signal = false;
    let mut fft = FftScratch::new();
    let correlation = cross_correlate_shaped(take, reference, &mut fft, |bins| {
        signal = phase_transform(bins, config.band, rate);
    });
    if !signal {
        return Err(AlignError::NoSignal);
    }
    // Two non-empty buffers always overlap at lag zero, so the window is never
    // empty; the error is the type's way of saying so.
    let peak = correlation
        .peak_within(config.max_lag)
        .ok_or(AlignError::NoSignal)?;
    Ok(Alignment {
        lag: peak.lag,
        offset: parabolic_offset(&correlation, peak.lag),
        confidence: distinctness(
            |l| correlation.at(l).unwrap_or(f64::NEG_INFINITY),
            peak.lag,
            peak.value,
            window(&correlation, config.max_lag),
        ),
    })
}

/// The lags `peak_within(max_lag)` searches: `-max_lag..=max_lag` clamped to the
/// overlap.
fn window(correlation: &CrossCorrelation, max_lag: Samples) -> (isize, isize) {
    let bound = isize::try_from(max_lag.get()).unwrap_or(isize::MAX);
    (
        correlation.min_lag().max(-bound),
        correlation.max_lag().min(bound),
    )
}

/// Whitens the in-band bins and zeroes the rest; `false` when no in-band bin
/// carries energy.
///
/// Each in-band bin is divided by `|bin| + floor`, `floor` being
/// [`PHAT_FLOOR`] of the strongest **in-band** bin: out-of-band energy (a hum
/// below the band) does not set the level. Bins more than `PHAT_FLOOR` below
/// the strongest are damped rather than boosted, so the whitening spans the
/// source's dynamic range down to that level.
fn phase_transform(bins: &mut [Complex64], band: Option<FrequencyBand>, rate: SampleRate) -> bool {
    // `bins` holds `size / 2 + 1` bins of a `size`-point transform.
    let size = (bins.len().saturating_sub(1) * 2).max(1);
    let in_band = |k: usize| {
        band.is_none_or(|b| {
            let hz = k as f64 * rate.get() / size as f64;
            hz >= b.min().get() && hz <= b.max().get()
        })
    };
    let strongest = bins
        .iter()
        .enumerate()
        .filter(|&(k, _)| in_band(k))
        .map(|(_, b)| b.norm())
        .fold(0.0f64, f64::max);
    if strongest <= 0.0 {
        bins.fill(Complex64::default());
        return false;
    }
    let floor = strongest * PHAT_FLOOR;
    for (k, bin) in bins.iter_mut().enumerate() {
        *bin = if in_band(k) {
            *bin / (bin.norm() + floor)
        } else {
            Complex64::default()
        };
    }
    true
}

/// The parabola's vertex through `lag - 1`, `lag`, `lag + 1`, relative to
/// `lag`; zero when a neighbour is outside the overlap.
fn parabolic_offset(correlation: &CrossCorrelation, lag: isize) -> f64 {
    let (Some(l), Some(c), Some(r)) = (
        correlation.at(lag - 1),
        correlation.at(lag),
        correlation.at(lag + 1),
    ) else {
        return 0.0;
    };
    parabolic_peak(l, c, r).offset
}

/// `1 - second / peak` for the strongest value in `lo..=hi` outside the peak's
/// own lobe; `NONE` when the window has no such competitor.
///
/// The lobe runs outward from `lag` while values stay at or above
/// [`LOBE_FRACTION`] of the peak (so ripple or an exact tie at the top stays
/// inside), then on down to the next local minimum.
fn distinctness(
    value: impl Fn(isize) -> f64,
    lag: isize,
    peak: f64,
    (lo, hi): (isize, isize),
) -> Confidence {
    if peak <= 0.0 {
        return Confidence::NONE;
    }
    let edge = |step: isize, limit: isize| {
        let mut at = lag;
        while at != limit && value(at + step) >= LOBE_FRACTION * peak {
            at += step;
        }
        while at != limit && value(at + step) < value(at) {
            at += step;
        }
        at
    };
    let (left, right) = (edge(-1, lo), edge(1, hi));
    (lo..left)
        .chain(right + 1..=hi)
        .map(value)
        .reduce(f64::max)
        .map_or(Confidence::NONE, |second| {
            Confidence::new_clamped(1.0 - second.max(0.0) / peak)
        })
}

#[cfg(test)]
mod tests;
