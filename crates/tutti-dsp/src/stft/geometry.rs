//! The analysis grid an STFT is computed on.
//!
//! One type owning window, hop, and sample rate together, because they are
//! only meaningful together: whether a transform inverts cleanly is a fact
//! about the *pair* `(window, hop)`, not about either alone.
//!
//! The derived quantities (hop count, bin count, frame times) are computed from
//! it, never stored beside it, so they cannot contradict it.

use tutti_buffers::Float;
use tutti_units::{Hz, SampleRate, Samples, Seconds};

use crate::stft::error::StftError;
use crate::stft::grid::{BinCount, BinIndex, FrameCount};
use crate::stft::window::{CosineWindow, Window};

tutti_units::ranged! {
    /// An analysis window's length: at least one sample.
    ///
    /// [`new`](Self::new) refuses a zero length and never clamps.
    WindowLength(Samples), WindowLengthError, "a window of at least one sample",
    |raw| raw.get() > 0
}

tutti_units::ranged! {
    /// How far an STFT's frames advance: at least one sample.
    ///
    /// [`new`](Self::new) refuses a zero hop, which would never advance, and never clamps.
    HopSize(Samples), HopSizeError, "a hop of at least one sample",
    |raw| raw.get() > 0
}

/// Window, hop, window shape, and sample rate, valid together by construction.
///
/// Fields are private and the window and the hop are non-zero types, so
/// invalid instances cannot exist: a zero window, a zero hop, or a stored bin
/// count contradicting the window it came from.
///
/// **`window` and `window_fn` are different nouns**, and the names are chosen
/// to keep them apart: in DSP prose "the window is 2048" is a *length* and "the
/// window function is Hann" is a *shape*. The length decides frequency
/// resolution; the shape decides the sidelobe skirts around each partial.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StftGeometry {
    window: WindowLength,
    hop: HopSize,
    sample_rate: SampleRate,
    window_fn: CosineWindow,
}

impl StftGeometry {
    /// An analysis grid with the Hann window shape.
    ///
    /// Accepts any window and hop, **including a hop wider than the
    /// window**: decimated display transforms deliberately sample sparsely,
    /// skipping the audio between frames. Such a grid analyses fine and cannot
    /// reconstruct, which is why the inverse path goes through
    /// [`ColaGeometry`] instead of this.
    #[must_use]
    pub const fn new(sample_rate: SampleRate, window: WindowLength, hop: HopSize) -> Self {
        Self {
            window,
            hop,
            sample_rate,
            // Spelled literally rather than `CosineWindow::default()`: changing the
            // default must not silently re-tune every grid this constructor
            // has ever built.
            window_fn: CosineWindow::HANN,
        }
    }

    /// Whether frames overlap at all. False for a decimated display grid,
    /// which leaves gaps of unanalysed audio between frames.
    #[inline]
    #[must_use]
    fn frames_overlap(self) -> bool {
        self.hop().get() < self.window().get()
    }

    /// The same grid analysed with the window shape `window_fn`.
    ///
    /// An option rather than a fourth argument to `new`: every call site in
    /// the tree wants [`CosineWindow::HANN`], and a fourth positional parameter
    /// beside two lengths is exactly the transposition hazard `new` exists to
    /// reject.
    ///
    /// **Infallible, and that is not the same as safe.** A grid that is COLA
    /// for one window may not be COLA for another, and this does not re-check:
    /// Blackman needs 8x overlap where Hann needs 4x, so a 2048-sample window at
    /// a hop of 512 with Blackman analyses fine and reconstructs with a 2.1%
    /// ripple. Convert to [`ColaGeometry`] when the result must invert.
    #[inline]
    #[must_use]
    pub const fn shape(mut self, window_fn: CosineWindow) -> Self {
        self.window_fn = window_fn;
        self
    }

    /// The analysis window in [`Samples`] — also the FFT size.
    #[inline]
    #[must_use]
    pub const fn window(self) -> Samples {
        self.window.get()
    }

    /// How far frames advance, in [`Samples`]. Smaller than the window whenever
    /// frames overlap.
    #[inline]
    #[must_use]
    pub const fn hop(self) -> Samples {
        self.hop.get()
    }

    /// The rate the analyzed buffer is denominated at, which sets what each bin
    /// is worth in [`Hz`].
    #[inline]
    #[must_use]
    pub const fn sample_rate(self) -> SampleRate {
        self.sample_rate
    }

    /// The window *shape*. See [`window`](Self::window) for its length.
    #[inline]
    #[must_use]
    pub const fn window_fn(self) -> CosineWindow {
        self.window_fn
    }

    /// Whether this window squared reconstructs exactly at this window/hop pair.
    ///
    /// Two conditions belonging to two different types: the hop must divide the
    /// window (a fact about the *grid*) and the overlap must reach the window's
    /// own COLA factor (a fact about the *window*, given by
    /// [`CosineWindow::cola_overlap`]).
    #[inline]
    #[must_use]
    pub fn is_cola(self) -> bool {
        self.window().get().is_multiple_of(self.hop().get())
            && self.window().get() / self.hop().get() >= self.window_fn.cola_overlap()
    }

    /// Real-spectrum bins including Nyquist — the complex path's count.
    #[inline]
    #[must_use]
    pub fn bins_per_frame(self) -> BinCount {
        BinCount(self.window().get() / 2 + 1)
    }

    /// The centre frequency of a bin.
    #[inline]
    #[must_use]
    pub fn bin_frequency(self, bin: BinIndex) -> Hz {
        Hz::new(bin.get() as f64 * self.sample_rate.get() / self.window().get() as f64)
    }

    /// The fractional bin `frequency` falls on, `frequency * window / sample_rate`:
    /// bin `k` is centred on `bin_of(bin_frequency(k)) == k`,
    /// and a frequency on a bin boundary gives a whole number.
    #[inline]
    #[must_use]
    pub fn bin_of(self, frequency: Hz) -> f64 {
        frequency.get() * self.window().get() as f64 / self.sample_rate.get()
    }

    /// The frequency of a fractional bin, the inverse of [`bin_of`](Self::bin_of).
    #[inline]
    #[must_use]
    pub fn frequency_of(self, bin: f64) -> Hz {
        Hz::new(bin * self.sample_rate.get() / self.window().get() as f64)
    }

    /// The time between frames, `hop / sample_rate`.
    #[inline]
    #[must_use]
    pub fn frame_interval(self) -> Seconds {
        Seconds::new(self.hop().get() as f64 / self.sample_rate.get())
    }

    /// How many whole frames fit over `len` samples.
    #[inline]
    pub fn frames_for(self, len: impl Into<Samples>) -> FrameCount {
        FrameCount::fitting(len.into(), self.window(), self.hop())
    }

    /// How many samples resynthesizing `frames` frames produces.
    #[inline]
    #[must_use]
    pub fn output_len(self, frames: FrameCount) -> Samples {
        if frames.get() == 0 {
            return Samples::new(0);
        }
        Samples::new((frames.get() - 1) * self.hop().get() + self.window().get())
    }

    /// The delay this grid imposes: one whole window must arrive before the
    /// first frame can be analyzed.
    #[inline]
    #[must_use]
    pub fn latency(self) -> Seconds {
        Seconds::new(self.window().get() as f64 / self.sample_rate.get())
    }

    /// The analysis window's coefficients, `window` points of `window_fn`.
    ///
    /// Deliberately longer than its neighbours: it **allocates**, where
    /// [`window`](Self::window) and [`window_fn`](Self::window_fn) are `const`
    /// field reads. A short name would put an allocation and two free reads at
    /// the same apparent cost.
    #[inline]
    #[must_use]
    pub fn window_coefficients<F: Float>(self) -> Vec<F> {
        let design = self.window_fn.coefficients(self.window().get());
        design.into_iter().map(F::from_design).collect()
    }

    /// The DC gain this grid's forward transform leaves in every magnitude.
    ///
    /// A forward transform sums `window` samples per bin without scaling, so
    /// magnitudes carry a gain of `coherent_gain × window`. A display dividing
    /// by that is window-*independent*: a 2048-point Hann analysis and a
    /// 512-point Blackman one of the same audio land on the same scale, so a dB
    /// window tuned once stays tuned.
    ///
    /// The reciprocal is what a magnitude consumer actually wants; it is
    /// spelled out rather than provided because "gain" and "the thing you
    /// multiply by" being two methods is how one of them gets used by mistake.
    #[inline]
    #[must_use]
    pub fn magnitude_gain<F: Float>(self) -> F {
        F::from_design(self.window_fn.coherent_gain() * self.window().get().max(1) as f64)
    }
}

/// A grid an inverse transform accepts: its window squared is constant-overlap-add.
///
/// A window squared is COLA when the hop divides the window and the overlap reaches that
/// window's own factor ([`CosineWindow::cola_overlap`]). Under that condition the inverse's
/// per-sample window-sum normalization is exact, so untouched bins reconstruct to float
/// precision. Checked at the conversion rather than left to the forward transform: a hop
/// five times wider than its window otherwise reaches the inverse and produces a comb of
/// islands separated by silence.
///
/// `ColaGeometry::try_from(geometry)` depends on the window and the hop together, and on the
/// window shape that sets the overlap they must reach (Blackman needs 8x where Hann needs 4x).
/// [`StftGeometry::from`] gives the grid back.
///
/// # Errors
///
/// [`StftError::HopExceedsWindow`] if frames do not overlap, or
/// [`StftError::NotColaCompliant`] if the hop does not divide the window at the overlap the
/// window shape requires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColaGeometry(StftGeometry);

impl TryFrom<StftGeometry> for ColaGeometry {
    type Error = StftError;

    fn try_from(geometry: StftGeometry) -> Result<Self, StftError> {
        if !geometry.frames_overlap() {
            return Err(StftError::HopExceedsWindow {
                window: geometry.window(),
                hop: geometry.hop(),
            });
        }
        if !geometry.is_cola() {
            return Err(StftError::NotColaCompliant {
                window: geometry.window(),
                hop: geometry.hop(),
                window_fn: geometry.window_fn,
            });
        }
        Ok(Self(geometry))
    }
}

impl From<ColaGeometry> for StftGeometry {
    fn from(cola: ColaGeometry) -> Self {
        cola.0
    }
}

#[cfg(test)]
mod tests;
