//! Noise shaping: an error-feedback filter that moves the quantization error
//! to where the ear is least sensitive.
//!
//! [`NoiseShaping`] is the filter, designed for one sample rate. The sink's
//! quantizer runs it as error feedback: it quantizes `x + d + Σ c_k e[n-k]`
//! by the rule in [`Sample`] and records `e[n] = y[n] - (x[n] + Σ c_k e[n-k])`,
//! the whole error including the dither `d`. So `y - x = e * NTF`, with
//! `NTF(z) = 1 + Σ c_k z^-k`, and `e` is white at the unshaped error's
//! variance whatever the signal is.

use crate::{Complex, RealFft};
use tutti_buffers::{Float, Interleaved, InterleavedMut, MonoMut, Sample};
use tutti_units::{Amplitude, Db, Hz, SampleRate};

/// The weighting is held at its value at this frequency below it.
const WEIGHTING_FLOOR: Hz = Hz::new(100.0);
/// The weighting is held at its value at this frequency above it.
const WEIGHTING_CEILING: Hz = Hz::new(12_000.0);
/// The frame length of the cepstral design, a power of two far longer than the
/// weighting's features need.
const DESIGN_SIZE: usize = 4096;

/// The magnitude `|NTF|` the design follows, as the natural log of an
/// amplitude, at `frequency`: Terhardt's approximation of the absolute
/// threshold of hearing,
/// `T(f) = 3.64 f^-0.8 - 6.5 e^(-0.6 (f - 3.3)^2) + 10^-3 f^4` dB with `f` in kHz
/// (E. Terhardt, "Calculating virtual pitch", Hearing Research 1, 1979).
/// The noise is allowed where the ear's threshold is high.
fn weighting_ln(frequency: Hz) -> f64 {
    let khz = frequency.clamp(WEIGHTING_FLOOR, WEIGHTING_CEILING).get() / 1000.0;
    let db = 3.64 * libm::pow(khz, -0.8) - 6.5 * libm::exp(-0.6 * (khz - 3.3) * (khz - 3.3))
        + 1e-3 * libm::pow(khz, 4.0);
    libm::log(Db::new(db).to_amplitude().get())
}

/// The number of taps at `rate`: 32 at 44.1 kHz, and in proportion to the rate, so
/// the filter spans the same time (about 0.73 ms) and its response is the same in Hz at
/// any rate. At most `DESIGN_SIZE / 2 - 1`.
fn tap_count(rate: SampleRate) -> usize {
    let taps = libm::round(rate.get() * 32.0 / SampleRate::SR_44K1.get()) as usize;
    taps.clamp(1, DESIGN_SIZE / 2 - 1)
}

/// A noise-transfer function `NTF(z) = 1 + Σ c_k z^-k` designed for one
/// sample rate: monic and minimum phase, with `|NTF|` following the absolute
/// threshold of hearing (Terhardt's approximation) as a function of frequency
/// in Hz, held at its 100 Hz and 12 kHz values outside that range.
///
/// The taps span about 0.73 ms at every rate, which follows the threshold within
/// 2.5 dB from 2 kHz to 16 kHz (the worst deviation measured is 2.1 dB). Below 2 kHz the filter is too short for the
/// threshold's slope and its response is flatter than the threshold's: the noise
/// there is shaped less, not more.
///
/// Monic and minimum phase mean `∫ ln |NTF| dω = 0` over `[0, π]` (the
/// Gerzon-Craven theorem): the noise the filter removes where the ear is
/// sensitive is paid for, in log terms, where it is not. The design takes the
/// weighting's log magnitude on a dense frequency grid, folds its real
/// cepstrum onto the causal side, and exponentiates the series, which gives the
/// minimum-phase response with that magnitude; the first
/// [`order`](Self::order) terms are the taps. Truncating there leaves the
/// zero count unchanged but can move a zero across the unit circle, so the
/// integral is exactly zero while every zero stays inside the unit circle,
/// which holds at every rate tested (8 kHz to 192 kHz).
///
/// ```
/// use tutti_dsp::NoiseShaping;
/// use tutti_units::SampleRate;
///
/// let shaping = NoiseShaping::new(SampleRate::SR_44K1);
/// assert_eq!(shaping.order(), 32);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct NoiseShaping {
    taps: Vec<f64>,
}

impl NoiseShaping {
    /// The filter for blocks at `rate`. Allocates: call it on the control
    /// side.
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        let order = tap_count(rate);
        let fft = RealFft::<f64>::new(DESIGN_SIZE);
        let mut scratch = fft.new_scratch();
        let step = rate.get() / DESIGN_SIZE as f64;
        let mut bins: Vec<Complex<f64>> = (0..fft.bins())
            .map(|k| Complex::new(weighting_ln(Hz::new(step * k as f64)), 0.0))
            .collect();
        let mut cepstrum = vec![0.0; DESIGN_SIZE];
        fft.inverse(&mut bins, MonoMut::new(&mut cepstrum), &mut scratch);
        // The inverse is unnormalized, and the mean of the log magnitude (the
        // zeroth term) is dropped: that is what makes the response monic.
        let scale = 2.0 / DESIGN_SIZE as f64;
        let folded: Vec<f64> = cepstrum
            .iter()
            .take(DESIGN_SIZE / 2)
            .enumerate()
            .map(|(n, &c)| if n == 0 { 0.0 } else { c * scale })
            .collect();
        // h[n] = sum_{k=1..n} (k / n) folded[k] h[n - k]: the series of exp(folded(z)).
        let mut h = vec![0.0; order + 1];
        h[0] = 1.0;
        for n in 1..=order {
            h[n] = (1..=n)
                .map(|k| k as f64 / n as f64 * folded[k] * h[n - k])
                .sum();
        }
        h.remove(0);
        Self { taps: h }
    }

    /// The number of taps `n` of `NTF(z) = 1 + Σ c_k z^-k`, `k = 1..=n`.
    #[must_use]
    pub fn order(&self) -> usize {
        self.taps.len()
    }

    /// The taps `c_1..=c_n`.
    pub(crate) fn taps(&self) -> &[f64] {
        &self.taps
    }
}

/// The running error-feedback state of every channel: the last `order`
/// errors of each, sized at construction.
///
/// The fed-back error is held within two least significant bits. With dither up
/// to one LSB in amplitude the error `y - (x + feedback)` is at most 1.5 LSB, so
/// the bound is never reached while the quantizer is in range; it only acts when
/// a value clips, where the error is as large as the overload. The feedback is
/// then at most `2 Σ|c_k|` LSB and the filter forgets the overload after as
/// many frames as it has taps.
#[derive(Debug, Clone)]
pub(crate) struct ErrorFeedback<F> {
    /// `c_n..=c_1`: oldest error first, to match a window of the history.
    taps: Vec<F>,
    /// Per channel, `2 * order` errors: each is written at `head` and at
    /// `head + order`, so `[head, head + order)` holds the last `order`
    /// errors, oldest first, as one slice.
    history: Vec<Vec<F>>,
    head: usize,
}

impl<F: Float> ErrorFeedback<F> {
    pub(crate) fn new(shaping: &NoiseShaping, channels: usize) -> Self {
        let taps: Vec<F> = shaping
            .taps()
            .iter()
            .rev()
            .map(|&c| F::from_design(c))
            .collect();
        Self {
            history: vec![vec![F::ZERO; 2 * taps.len()]; channels],
            taps,
            head: 0,
        }
    }

    /// The largest error held in the history, in float units.
    #[cfg(test)]
    pub(crate) fn peak(&self) -> F {
        self.history
            .iter()
            .flatten()
            .fold(F::ZERO, |m, &e| m.max(e.abs()))
    }

    /// The largest error held for channel `c`, in float units.
    #[cfg(test)]
    pub(crate) fn peak_of(&self, c: usize) -> F {
        self.history[c].iter().fold(F::ZERO, |m, &e| m.max(e.abs()))
    }

    /// Quantizes `dithered` (the signal `clean` with its noise) into `out` by
    /// the rule in [`Sample`], feeding back the error of each value against
    /// `clean`; returns the clips. `lsb` is one step of `S`.
    pub(crate) fn process<S: Sample>(
        &mut self,
        lsb: Amplitude,
        clean: Interleaved<'_, F>,
        dithered: Interleaved<'_, F>,
        mut out: InterleavedMut<'_, S>,
    ) -> usize {
        let order = self.taps.len();
        let bound = F::from_unit(lsb) * F::lit(2.0);
        let mut clipped = 0;
        let frames = clean.iter().zip(dithered.iter()).zip(out.iter_mut());
        for ((clean, dithered), out) in frames {
            let channels = clean.iter().zip(dithered).zip(out).zip(&mut self.history);
            for (((&x, &u), y), history) in channels {
                let window = &history[self.head..self.head + order];
                let feedback = self
                    .taps
                    .iter()
                    .zip(window)
                    .fold(F::ZERO, |acc, (&tap, &e)| acc + tap * e);
                let (value, clip) = (u + feedback).convert_to_clipped::<S>();
                *y = value;
                clipped += usize::from(clip);
                let error = value.convert_to::<F>() - (x + feedback);
                // A non-finite input leaves nothing behind, a clip no more than the bound.
                let error = if error.is_finite() {
                    error.clamp(-bound, bound)
                } else {
                    F::ZERO
                };
                history[self.head] = error;
                history[self.head + order] = error;
            }
            self.head = (self.head + 1) % order;
        }
        clipped
    }
}

#[cfg(test)]
mod tests;
