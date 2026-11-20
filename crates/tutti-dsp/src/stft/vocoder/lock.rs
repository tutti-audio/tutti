//! The one phase solution every channel of a [`PhaseVocoder`](super::PhaseVocoder) shares.
//!
//! The solution is solved on a reference spectrum (the channel sum) and applied to every
//! channel as a per-bin rotation, so each channel keeps the phase it had relative to the
//! reference and the inter-channel phase differences survive the stretch.

use tutti_buffers::Float;
use tutti_units::Radians;

use crate::fft::{Complex, FftFloat};

/// How far under the channels' summed power the sum's power may fall before the reference
/// follows the loudest channel instead: a sum 30 dB down (the channels within about 2.6 degrees
/// of antiphase) has no phase worth tracking.
const SUM_FLOOR: f64 = 1e-3;

/// The phase of `z`, in the unit's `f64`.
#[inline]
fn arg<F: Float>(z: Complex<F>) -> Radians {
    Radians::new(z.im.atan2(z.re).into())
}

/// The unit complex number at `angle`, narrowed to `F`.
#[inline]
fn unit<F: Float>(angle: Radians) -> Complex<F> {
    let angle = F::from_unit(angle);
    Complex::new(angle.cos(), angle.sin())
}

/// The reference spectrum, its history, and the per-bin rotation solved from them.
///
/// Identity phase locking (Laroche and Dolson, "Improved phase vocoder time-scale
/// modification of audio", IEEE Trans. Speech and Audio Processing 7(3), 1999): the reference's
/// spectral peaks are the only bins whose phase advance is measured and accumulated; every bin
/// between two peaks, up to the midpoint, takes its peak's rotation, so a partial's lobe keeps
/// the phase relationships its window gave it (vertical coherence). The lock spans channels
/// too: the rotation of bin `k` is one value for all channels, so channel `c`'s bin keeps its
/// phase relative to the reference's and the stereo image survives.
pub(super) struct PhaseLock<F: Float> {
    /// The reference of the frame in hand.
    reference: Vec<Complex<F>>,
    /// The previous frame's reference.
    previous: Vec<Complex<F>>,
    /// The previous frame's reference after its rotation: the phase the output last carried.
    synthesized: Vec<Complex<F>>,
    /// `|reference|^2` of the frame in hand.
    power: Vec<F>,
    /// The peak bins of the frame in hand, ascending.
    peaks: Vec<usize>,
    /// The rotation of each bin, a unit complex number.
    rotation: Vec<Complex<F>>,
    /// Whether a frame has been solved since the last reset.
    primed: bool,
}

impl<F: FftFloat> PhaseLock<F> {
    /// A lock over spectra of `bins` bins, silent.
    pub(super) fn new(bins: usize) -> Self {
        let zero = Complex::new(F::ZERO, F::ZERO);
        Self {
            reference: vec![zero; bins],
            previous: vec![zero; bins],
            synthesized: vec![zero; bins],
            power: vec![F::ZERO; bins],
            peaks: Vec::with_capacity(bins),
            rotation: vec![Complex::new(F::ONE, F::ZERO); bins],
            primed: false,
        }
    }

    /// Forgets every frame: the next one is adopted as it is.
    pub(super) fn reset(&mut self) {
        let zero = Complex::new(F::ZERO, F::ZERO);
        self.previous.fill(zero);
        self.synthesized.fill(zero);
        self.primed = false;
    }

    /// The rotation of every bin for the frame whose channel spectra are `spectra`.
    ///
    /// `hops` is `(analysis, synthesis)`, the frame's measured step through the input and the
    /// step the output takes, for every frame after the first. `window` is the frame length.
    /// A peak's phase advance over `analysis` samples is its bin's expected advance plus the
    /// wrapped deviation (the instantaneous frequency), carried forward over `synthesis`
    /// samples from the phase the output last carried. No allocation.
    pub(super) fn solve(
        &mut self,
        spectra: &[Vec<Complex<F>>],
        hops: Option<(usize, usize)>,
        window: usize,
    ) -> &[Complex<F>] {
        self.fold(spectra);
        self.find_peaks();
        self.rotation.fill(Complex::new(F::ONE, F::ZERO));
        if let Some((analysis, synthesis)) = hops.filter(|_| self.primed) {
            self.rotate_peaks(analysis, synthesis, window);
        }
        for ((previous, synthesized), (&reference, &rotation)) in self
            .previous
            .iter_mut()
            .zip(&mut self.synthesized)
            .zip(self.reference.iter().zip(&self.rotation))
        {
            *previous = reference;
            *synthesized = reference * rotation;
        }
        self.primed = true;
        &self.rotation
    }

    /// The reference: the channel sum, or the loudest channel's bin where the sum cancels.
    fn fold(&mut self, spectra: &[Vec<Complex<F>>]) {
        let floor = F::from_design(SUM_FLOOR);
        for (k, reference) in self.reference.iter_mut().enumerate() {
            let mut sum = Complex::new(F::ZERO, F::ZERO);
            let (mut total, mut loudest, mut loudest_power) = (F::ZERO, 0, F::ZERO);
            for (c, spectrum) in spectra.iter().enumerate() {
                let bin = spectrum[k];
                let power = bin.norm_sqr();
                sum = sum + bin;
                total += power;
                if power > loudest_power {
                    (loudest, loudest_power) = (c, power);
                }
            }
            *reference = if sum.norm_sqr() >= floor * total {
                sum
            } else {
                spectra[loudest][k]
            };
        }
        for (power, reference) in self.power.iter_mut().zip(&self.reference) {
            *power = reference.norm_sqr();
        }
    }

    /// The bins whose power exceeds that of the two bins below and reaches that of the two above
    /// (so two peaks are never adjacent).
    fn find_peaks(&mut self) {
        self.peaks.clear();
        let power = &self.power;
        for k in 0..power.len() {
            let p = power[k];
            let left = (1..=2).all(|d| k < d || p > power[k - d]);
            let right = (1..=2).all(|d| k + d >= power.len() || p >= power[k + d]);
            if p > F::ZERO && left && right {
                self.peaks.push(k);
            }
        }
    }

    /// Solves each peak's rotation and spreads it over the bins up to the midpoint to its
    /// neighbouring peaks.
    fn rotate_peaks(&mut self, analysis: usize, synthesis: usize, window: usize) {
        let stretch = synthesis as f64 / analysis as f64;
        let bins = self.power.len();
        for (i, &peak) in self.peaks.iter().enumerate() {
            let current = self.reference[peak];
            let before = self.previous[peak];
            let carried = if before.norm_sqr() > F::ZERO {
                let expected = Radians::TAU * (peak as f64 * analysis as f64 / window as f64);
                let measured = arg(current * before.conj());
                let advance = expected + (measured - expected).wrapped_signed();
                arg(self.synthesized[peak]) + advance * stretch
            } else {
                // A partial that was silent a frame ago has no advance to measure: it starts
                // from the phase it has.
                arg(current)
            };
            let magnitude = current.norm_sqr().sqrt();
            let phase_of_current = Complex::new(current.re / magnitude, -current.im / magnitude);
            let rotation = unit::<F>(carried) * phase_of_current;
            let from = if i == 0 {
                0
            } else {
                (self.peaks[i - 1] + peak) / 2 + 1
            };
            let to = self
                .peaks
                .get(i + 1)
                .map_or(bins, |&next| (peak + next) / 2 + 1);
            self.rotation[from..to].fill(rotation);
        }
    }
}
