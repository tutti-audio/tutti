//! The short-time Fourier transform and its inverse.
//!
//! Three result types, so "can this be inverted?" is a question the compiler
//! answers:
//!
//! - [`Stft`] — complex bins, as the transform produces them.
//! - [`StftPolar`] — the same information in magnitude/phase form, read through
//!   its magnitude and phase grids. Not invertible.
//! - [`StftMagnitude`] — phase **discarded**. Lossy, and has no inverse.
//!
//! Only the first reconstructs, through [`istft_transform`](crate::istft_transform); the
//! other two are for reading, and the phase-less third is a real capability
//! boundary. Collapsing all
//! three into one struct is how a phase-less result reached the inverse and
//! panicked, and how a decimated display transform reached it and produced a
//! comb of islands separated by silence.

use tutti_buffers::{Mono, MonoMut, Sample};
use tutti_units::{Amplitude, SampleRate, Samples};

use crate::convert::widen;
use crate::{FramingError, TransformError};

use crate::Region;
use tutti_dsp::Complex64;
use tutti_dsp::CosineWindow;
use tutti_dsp::FftScratch;
use tutti_dsp::{BinIndex, FrameCount, FrameIndex, Grid};
use tutti_dsp::{ColaGeometry, HopSize, StftGeometry, WindowLength};

/// Linear magnitudes, in the units the transform produced. **The invertible
/// form.**
///
/// Distinct from [`NormalizedMagnitudes`] on purpose. As bare `Vec<f64>` both
/// reach the inverse interchangeably, and passing the normalized set compiles
/// and yields audio scaled by `1/peak` — quiet, plausible, and hard to notice.
#[derive(Debug, Clone, PartialEq)]
pub struct RawMagnitudes(Grid<f64>);

impl RawMagnitudes {
    /// Wraps a `frames x bins` grid of linear magnitudes.
    #[must_use]
    pub fn new(grid: Grid<f64>) -> Self {
        Self(grid)
    }

    /// The underlying `frames x bins` grid, borrowed.
    #[inline]
    pub fn grid(&self) -> &Grid<f64> {
        &self.0
    }

    /// The largest magnitude anywhere in the grid.
    pub fn peak(&self) -> Amplitude {
        Amplitude::new(
            self.0
                .as_slice()
                .iter()
                .copied()
                .fold(0.0f64, |a, b| a.max(b)),
        )
    }

    /// Scales to `0..1` against the peak, returning the display-only form.
    ///
    /// Lazy on purpose. Normalizing eagerly at construction means storing both
    /// forms — roughly 84 MB of duplication for a 4-minute file — when no
    /// consumer reads both, so the caller decides whether to pay for it.
    pub fn normalize(&self) -> NormalizedMagnitudes {
        let peak = self.peak();
        let values = if peak.get() > 0.0 {
            let inv = 1.0 / peak.get();
            self.0.as_slice().iter().map(|m| m * inv).collect()
        } else {
            self.0.as_slice().to_vec()
        };
        NormalizedMagnitudes {
            values: self.0.same_shape(values),
            peak,
        }
    }
}

/// Peak-normalized magnitudes plus the peak they were divided by. Display
/// only — there is no path from here back to audio.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedMagnitudes {
    values: Grid<f64>,
    peak: Amplitude,
}

impl NormalizedMagnitudes {
    /// The `frames x bins` grid of `0..1` values, borrowed.
    #[inline]
    pub fn grid(&self) -> &Grid<f64> {
        &self.values
    }

    /// The peak the values were scaled against — enough to recover the raw
    /// magnitudes if needed.
    #[inline]
    pub fn peak(&self) -> Amplitude {
        self.peak
    }
}

/// Complex bins, `DC..=Nyquist` per frame.
///
/// Array-of-structs on purpose: the spectral edit path multiplies each bin by
/// a complex mask, which touches magnitude and phase together. Splitting them
/// would force a deinterleave per edit.
#[derive(Debug, Clone, PartialEq)]
pub struct Stft {
    bins: Grid<Complex64>,
    geometry: StftGeometry,
}

/// Magnitude and phase, in separate grids.
///
/// Structure-of-arrays on purpose, and the opposite choice from [`Stft`] for
/// the opposite reason: consumers read magnitudes without phase — one uploads
/// the magnitude grid straight to the GPU as a texture — so interleaving would
/// force a deinterleave on every read.
#[derive(Debug, Clone, PartialEq)]
pub struct StftPolar {
    magnitudes: RawMagnitudes,
    phases: Grid<f64>,
    geometry: StftGeometry,
}

/// Magnitudes with phase discarded. **Not invertible.**
///
/// What a decimated display transform produces. Its own type rather than an
/// invertible result carrying an empty phase vector: that shape is statically
/// indistinguishable from a real one, and reaches the inverse as a panic.
#[derive(Debug, Clone, PartialEq)]
pub struct StftMagnitude {
    magnitudes: RawMagnitudes,
    geometry: StftGeometry,
}

impl Stft {
    /// The complex bins as a `frames x bins` grid, `DC..=Nyquist` per frame.
    #[inline]
    pub fn bins(&self) -> &Grid<Complex64> {
        &self.bins
    }

    /// The window, hop and sample rate this transform was taken at.
    #[inline]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }

    /// How many frames the transform produced.
    #[inline]
    pub fn frames(&self) -> FrameCount {
        self.bins.frames()
    }

    /// `(frames, bins)` as plain counts — the shape a mask or display grid
    /// must match.
    #[inline]
    pub fn dims(&self) -> (usize, usize) {
        (self.bins.frames().get(), self.bins.bins().get())
    }

    /// Whether the transform produced no frames at all.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bins.is_empty()
    }

    /// The [`Amplitude`] at one bin — the complex bin's modulus, phase
    /// discarded for this read only.
    #[inline]
    pub fn magnitude_at(&self, frame: FrameIndex, bin: BinIndex) -> Amplitude {
        Amplitude::new(self.bins.at(frame, bin).norm())
    }

    /// Converts to magnitude/phase. Lossless.
    #[must_use]
    pub fn to_polar(&self) -> StftPolar {
        let (mut magnitudes, mut phases) = (
            Vec::with_capacity(self.bins.as_slice().len()),
            Vec::with_capacity(self.bins.as_slice().len()),
        );
        for bin in self.bins.as_slice() {
            magnitudes.push(bin.norm());
            phases.push(bin.arg());
        }
        StftPolar {
            magnitudes: RawMagnitudes(self.bins.same_shape(magnitudes)),
            phases: self.bins.same_shape(phases),
            geometry: self.geometry,
        }
    }

    /// Discards phase. One-way, and the return type says so.
    #[must_use]
    pub fn to_magnitude(&self) -> StftMagnitude {
        let magnitudes: Vec<f64> = self.bins.as_slice().iter().map(|b| b.norm()).collect();
        StftMagnitude {
            magnitudes: RawMagnitudes(self.bins.same_shape(magnitudes)),
            geometry: self.geometry,
        }
    }

    /// Applies a complex mask and resynthesizes — the edit path's whole operation
    /// in one call, so no caller hand-writes the zip plus the inverse.
    ///
    /// # Errors
    /// Returns [`TransformError::GridShapeDisagreement`] if `mask` does not
    /// match this transform's `frames x bins` shape.
    pub fn resynthesize_masked<S: Sample>(
        &self,
        mask: &Grid<Complex64>,
    ) -> Result<Vec<S>, TransformError> {
        if !self.bins.has_same_shape(mask) {
            return Err(TransformError::GridShapeDisagreement);
        }
        let masked: Vec<Complex64> = self
            .bins
            .as_slice()
            .iter()
            .zip(mask.as_slice())
            .map(|(b, m)| b * m)
            .collect();
        let masked = Stft {
            bins: self.bins.same_shape(masked),
            geometry: self.geometry,
        };
        Ok(istft(&masked))
    }
}

impl StftPolar {
    /// The magnitude half, in the units the transform produced.
    #[inline]
    pub fn magnitudes(&self) -> &RawMagnitudes {
        &self.magnitudes
    }

    /// The phase half, in radians — same `frames x bins` shape as the
    /// magnitudes.
    #[inline]
    pub fn phases(&self) -> &Grid<f64> {
        &self.phases
    }

    /// The window, hop and sample rate this transform was taken at.
    #[inline]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }
}

impl StftMagnitude {
    /// The magnitudes, in the units the transform produced. There is no phase
    /// counterpart — that is what makes this type one-way.
    #[inline]
    pub fn magnitudes(&self) -> &RawMagnitudes {
        &self.magnitudes
    }

    /// The window, hop and sample rate this transform was taken at. A decimated
    /// hop generally breaks COLA, which is why this result cannot invert.
    #[inline]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }

    /// How many frames the transform produced.
    #[inline]
    pub fn frames(&self) -> FrameCount {
        self.magnitudes.grid().frames()
    }

    /// `(frames, bins)` as plain counts.
    #[inline]
    pub fn dims(&self) -> (usize, usize) {
        let grid = self.magnitudes.grid();
        (grid.frames().get(), grid.bins().get())
    }

    /// Whether the transform produced no frames at all.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.magnitudes.grid().is_empty()
    }
}

/// How the hop between frames is chosen.
///
/// A named policy rather than an `Option<usize>` doing enum duty, where `None`
/// and `Some(n)` have to carry "use my hop" and "override it to hit n frames"
/// between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopPolicy {
    /// Use exactly this hop, in [`Samples`].
    Fixed(Samples),
    /// Widen the hop to produce roughly `frames` frames, never going below
    /// `min_hop`.
    ///
    /// Decimation is a display concern: the resulting hop generally breaks
    /// COLA, so this yields [`StftMagnitude`], which cannot be inverted.
    TargetFrames {
        /// Roughly how many frames to land on.
        frames: FrameCount,
        /// Floor on the widened hop, in [`Samples`].
        min_hop: Samples,
    },
}

/// Which part of the buffer to analyze.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq)]
pub enum SampleRange {
    /// The whole buffer.
    All,
    /// The half-open [`Region`], clamped to the buffer.
    Span(Region),
}

impl SampleRange {
    fn resolve(self, len: usize) -> (usize, usize) {
        match self {
            Self::All => (0, len),
            Self::Span(region) => {
                let start = region.start().get().min(len);
                (start, region.end().get().clamp(start, len))
            }
        }
    }
}

/// What a short-time transform computes: the window, the hop and the window
/// shape. The part of the buffer and the sample rate belong to the call.
#[derive(Debug, Clone, Copy)]
pub struct StftConfig {
    /// Analysis window, in [`Samples`]. Also the FFT size.
    window: Samples,
    /// How far to advance between frames — fixed, or decimated to a frame count.
    hop: HopPolicy,
    /// The window *shape*. [`CosineWindow::HANN`] unless
    /// [`window_fn`](Self::window_fn) says otherwise.
    window_fn: CosineWindow,
}

impl StftConfig {
    /// A config at a fixed hop. The sample rate is the transform call's, which
    /// resolves the bin frequencies.
    pub fn new(window: impl Into<Samples>, hop: impl Into<Samples>) -> Self {
        Self {
            window: window.into(),
            hop: HopPolicy::Fixed(hop.into()),
            window_fn: CosineWindow::HANN,
        }
    }

    /// Analyses with a different window shape.
    ///
    /// A builder rather than a fourth argument to `new`, for the reason
    /// [`StftGeometry::shape`] gives: three `Samples`-adjacent
    /// positional parameters are already the transposition limit.
    ///
    /// Note this can make a config that [`resolve_cola`](Self::resolve_cola)
    /// refuses but [`resolve`](Self::resolve) accepts — Blackman needs 8x
    /// overlap where Hann needs 4x, so a grid that inverted before may not
    /// after. That refusal is the point.
    #[must_use]
    pub fn window_fn(mut self, window_fn: CosineWindow) -> Self {
        self.window_fn = window_fn;
        self
    }

    /// How the hop is chosen: a fixed one, or one widened to a target frame
    /// count (a display transform).
    #[must_use]
    pub fn hop(mut self, hop: HopPolicy) -> Self {
        self.hop = hop;
        self
    }

    /// The geometry this config resolves to over `len` samples at `rate`.
    ///
    /// Only ever an overlapping grid — use [`resolve_cola`](Self::resolve_cola)
    /// when the result must invert.
    ///
    /// # Errors
    /// Returns a [`TransformError`] for a zero window or hop, a hop wider than
    /// the window, or a non-positive sample rate.
    pub fn resolve(
        &self,
        rate: impl Into<SampleRate>,
        len: Samples,
    ) -> Result<StftGeometry, TransformError> {
        let hop = self.effective_hop(len.get());
        self.geometry_at(rate.into(), hop)
    }

    /// The geometry, additionally required to be invertible.
    ///
    /// # Errors
    /// Everything [`resolve`](Self::resolve) rejects, plus
    /// [`tutti_dsp::StftError::NotColaCompliant`] (as [`TransformError::Dsp`]) when the hop does not divide the
    /// window at the overlap this geometry's window shape requires — 4x for
    /// Hann and Hamming, 8x for Blackman.
    pub fn resolve_cola(
        &self,
        rate: impl Into<SampleRate>,
        len: Samples,
    ) -> Result<StftGeometry, TransformError> {
        let hop = self.effective_hop(len.get());
        Ok(ColaGeometry::try_from(self.geometry_at(rate.into(), hop)?)?.into())
    }

    /// The geometry of this config's window and shape at `hop` and `rate`.
    fn geometry_at(&self, rate: SampleRate, hop: Samples) -> Result<StftGeometry, TransformError> {
        let window = WindowLength::new(self.window).ok_or(FramingError::ZeroWindow)?;
        let hop = HopSize::new(hop).ok_or(FramingError::ZeroHop)?;
        Ok(StftGeometry::new(rate, window, hop).shape(self.window_fn))
    }

    fn effective_hop(&self, len: usize) -> Samples {
        match self.hop {
            HopPolicy::Fixed(hop) => hop,
            HopPolicy::TargetFrames { frames, min_hop } => {
                if frames.get() == 0 || len <= self.window.get() {
                    return min_hop;
                }
                let span = len - self.window.get();
                Samples::new((span / frames.get()).max(min_hop.get()).max(1))
            }
        }
    }
}

/// Computes the complex STFT of `samples` at `rate`.
///
/// The geometry must be COLA-compliant, because this result can be inverted.
///
/// # Errors
///
/// As [`StftConfig::resolve_cola`]: an invalid geometry, or one that is not
/// COLA-compliant.
pub fn stft<S: Sample>(
    config: &StftConfig,
    samples: Mono<'_, S>,
    range: SampleRange,
    rate: impl Into<SampleRate>,
) -> Result<Stft, TransformError> {
    let samples = widen(samples.samples());
    let (start, end) = range.resolve(samples.len());
    let slice = &samples[start..end];
    let geometry = config.resolve_cola(rate, Samples::new(slice.len()))?;
    Ok(compute(slice, geometry, &mut FftScratch::new()))
}

/// Computes the STFT in magnitude/phase form. Also invertible.
///
/// # Errors
///
/// As [`stft`].
pub fn stft_polar<S: Sample>(
    config: &StftConfig,
    samples: Mono<'_, S>,
    range: SampleRange,
    rate: impl Into<SampleRate>,
) -> Result<StftPolar, TransformError> {
    Ok(stft(config, samples, range, rate)?.to_polar())
}

/// Computes a magnitude-only STFT — the display path.
///
/// Accepts any overlapping geometry, including decimated hops that break COLA,
/// because the result cannot be inverted anyway. That is the whole reason this
/// returns a different type.
///
/// # Errors
///
/// As [`StftConfig::resolve`]: an invalid geometry.
pub fn stft_magnitude<S: Sample>(
    config: &StftConfig,
    samples: Mono<'_, S>,
    range: SampleRange,
    rate: impl Into<SampleRate>,
) -> Result<StftMagnitude, TransformError> {
    let samples = widen(samples.samples());
    let (start, end) = range.resolve(samples.len());
    let slice = &samples[start..end];
    let geometry = config.resolve(rate, Samples::new(slice.len()))?;
    Ok(compute(slice, geometry, &mut FftScratch::new()).to_magnitude())
}

/// Inverts a complex STFT by overlap-add.
///
/// Weighted overlap-add with per-sample window-sum normalization, which is
/// exact when the geometry is COLA — and [`Stft`] can only be built on one.
pub fn istft<S: Sample>(transform: &Stft) -> Vec<S> {
    let mut fft = FftScratch::new();
    let geometry = transform.geometry;
    let frames = transform.bins.frames();
    let window_len = geometry.window().get();

    let out_len = geometry.output_len(frames).get();
    if out_len == 0 {
        return Vec::new();
    }

    let window = geometry.window_coefficients::<f64>();
    let mut out = vec![0.0f64; out_len];
    let mut window_sum = vec![0.0f64; out_len];
    let mut frame_buf = vec![0.0f64; window_len];

    for frame in frames.indices() {
        fft.inverse(transform.bins.row(frame), MonoMut::new(&mut frame_buf));
        let offset = frame.get() * geometry.hop().get();
        for i in 0..window_len {
            out[offset + i] += frame_buf[i] * window[i];
            window_sum[offset + i] += window[i] * window[i];
        }
    }

    for (sample, &sum) in out.iter_mut().zip(&window_sum) {
        if sum > 1e-8 {
            *sample /= sum;
        }
    }
    out.iter().map(|s| s.convert_to::<S>()).collect()
}

/// The shared forward pass. Both entry points differ only in what they accept
/// and what they return.
fn compute(samples: &[f64], geometry: StftGeometry, fft: &mut FftScratch<f64>) -> Stft {
    let frames = geometry.frames_for(Samples::new(samples.len()));
    let bins_per_frame = geometry.bins_per_frame();
    let window = geometry.window_coefficients::<f64>();
    let window_len = geometry.window().get();

    let mut bins = vec![Complex64::default(); frames.get() * bins_per_frame.get()];
    for frame in frames.indices() {
        let offset = frame.get() * geometry.hop().get();
        let start = frame.get() * bins_per_frame.get();
        fft.forward(
            Mono::new(&samples[offset..offset + window_len]),
            &window,
            &mut bins[start..start + bins_per_frame.get()],
        );
    }

    Stft {
        bins: Grid::try_from((bins, frames, bins_per_frame)).expect("shape built to match"),
        geometry,
    }
}

#[cfg(test)]
mod tests;
