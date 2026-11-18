//! Cross-correlation through the FFT.

use rustfft::FftNum;
use tutti_buffers::{Float, Mono, MonoMut};

use super::{Complex, FftScratch};

/// The buffers of an FFT cross-correlation, reused from call to call.
///
/// The Wiener-Khinchin route: zero-pad both inputs to the transform size,
/// multiply one spectrum by the conjugate of the other, transform back. That is
/// `O(n log n)` where the direct sum is `O(n * lags)`. The padded signals and
/// both spectra live here, so a call allocates only when the transform size
/// grows past every earlier one; [`with_capacity`](Self::with_capacity) sizes
/// them up front. Generic over the [`Float`] it runs in, `f32` by default.
#[derive(Debug)]
pub struct Correlator<F: Float + FftNum = f32> {
    a: Vec<F>,
    b: Vec<F>,
    spectrum_a: Vec<Complex<F>>,
    spectrum_b: Vec<Complex<F>>,
}

impl<F: Float + FftNum> Default for Correlator<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: Float + FftNum> Correlator<F> {
    /// A correlator that allocates on its first call.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// A correlator sized for transforms of up to `size` points.
    #[must_use]
    pub fn with_capacity(size: usize) -> Self {
        Self {
            a: vec![F::ZERO; size],
            b: vec![F::ZERO; size],
            spectrum_a: vec![zero(); size / 2 + 1],
            spectrum_b: vec![zero(); size / 2 + 1],
        }
    }

    /// The circular cross-correlation of `a` against `b` on `out.len()` points:
    /// `out[k mod size] = sum_n a[n + k] * b[n]`, so a **positive lag `k` means
    /// `a` is a delayed copy of `b`**. Negative lags wrap to the end of `out`.
    ///
    /// The result is linear, with no lag aliased onto another, when
    /// `size >= a.len() + b.len() - 1`; a smaller size aliases the lags past
    /// `size - b.len()` onto the negative ones, which a caller that reads only
    /// the short lags can accept. `shape` sees the cross-spectrum `A * conj(B)`,
    /// `size / 2 + 1` bins, before the inverse transform, for a weighting such
    /// as the phase transform. The result is not normalised: the zero lag of a
    /// buffer with itself is its energy.
    ///
    /// # Panics
    ///
    /// When `out` is shorter than 2, or either input is longer than `out`.
    pub fn correlate(
        &mut self,
        a: Mono<'_, F>,
        b: Mono<'_, F>,
        fft: &mut FftScratch<F>,
        shape: impl FnOnce(&mut [Complex<F>]),
        mut out: MonoMut<'_, F>,
    ) {
        let (a, b, out) = (a.samples(), b.samples(), out.samples_mut());
        let size = out.len();
        assert!(size >= 2, "a correlation needs at least two points");
        assert!(
            a.len() <= size && b.len() <= size,
            "the inputs must fit the transform"
        );
        let bins = size / 2 + 1;
        spectrum(&mut self.a, &mut self.spectrum_a, a, size, bins, fft);
        spectrum(&mut self.b, &mut self.spectrum_b, b, size, bins, fft);
        for (x, y) in self.spectrum_a.iter_mut().zip(&self.spectrum_b) {
            *x = *x * y.conj();
        }
        shape(&mut self.spectrum_a);
        fft.inverse(&self.spectrum_a, MonoMut::new(out));
    }
}

/// The spectrum of `signal` zero-padded to `size`, in `padded` and `out`.
fn spectrum<F: Float + FftNum>(
    padded: &mut Vec<F>,
    out: &mut Vec<Complex<F>>,
    signal: &[F],
    size: usize,
    bins: usize,
    fft: &mut FftScratch<F>,
) {
    padded.clear();
    padded.extend_from_slice(signal);
    padded.resize(size, F::ZERO);
    out.resize(bins, zero());
    fft.forward_unwindowed(Mono::new(padded), out);
}

fn zero<F: Float>() -> Complex<F> {
    Complex::new(F::ZERO, F::ZERO)
}

#[cfg(test)]
mod tests;
