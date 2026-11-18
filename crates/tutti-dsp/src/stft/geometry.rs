//! The analysis grid an STFT is computed on.
//!
//! One type owning window, hop, and sample rate together, because they are
//! only meaningful together: whether a transform inverts cleanly is a fact
//! about the *pair* `(window, hop)`, not about either alone.
//!
//! The derived quantities (hop count, bin count, frame times) are computed from
//! it, never stored beside it, so they cannot contradict it.

use tutti_types::{Hz, SampleRate, Samples, Seconds};

use crate::stft::error::{Result, StftError};
use crate::stft::grid::{BinCount, BinIndex, FrameCount};
use crate::stft::window::{CosineWindow, Window};

/// Window, hop, window shape, and sample rate — validated once, on construction.
///
/// Fields are private and there is no literal constructor, so invalid
/// instances cannot exist: a zero hop, a
/// hop wider than its window, or a stored bin
/// count contradicting the window it came from.
///
/// **`window` and `window_fn` are different nouns**, and the names are chosen
/// to keep them apart: in DSP prose "the window is 2048" is a *length* and "the
/// window function is Hann" is a *shape*. The length decides frequency
/// resolution; the shape decides the sidelobe skirts around each partial.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StftGeometry {
    window: Samples,
    hop: Samples,
    sample_rate: SampleRate,
    window_fn: CosineWindow,
}

impl StftGeometry {
    /// An analysis grid.
    ///
    /// Accepts any positive window and hop, **including a hop wider than the
    /// window**: decimated display transforms deliberately sample sparsely,
    /// skipping the audio between frames. Such a grid analyses fine and cannot
    /// reconstruct, which is why the inverse path goes through
    /// [`cola`](Self::cola) instead of this.
    ///
    /// # Errors
    ///
    /// [`StftError::ZeroWindow`] or [`StftError::ZeroHop`] for a zero length.
    pub fn new(
        sample_rate: impl Into<SampleRate>,
        window: impl Into<Samples>,
        hop: impl Into<Samples>,
    ) -> Result<Self> {
        let (sample_rate, window, hop) = (sample_rate.into(), window.into(), hop.into());

        if window.is_zero() {
            return Err(StftError::ZeroWindow);
        }
        if hop.is_zero() {
            return Err(StftError::ZeroHop);
        }

        Ok(Self {
            window,
            hop,
            sample_rate,
            // Spelled literally rather than `CosineWindow::default()`: changing the
            // default must not silently re-tune every grid this constructor
            // has ever built.
            window_fn: CosineWindow::HANN,
        })
    }

    /// Whether frames overlap at all. False for a decimated display grid,
    /// which leaves gaps of unanalysed audio between frames.
    #[inline]
    #[must_use]
    fn frames_overlap(self) -> bool {
        self.hop < self.window
    }

    /// A COLA-compliant grid — the only kind an inverse transform accepts.
    ///
    /// A window squared is constant-overlap-add when the hop divides the window
    /// and the overlap reaches that window's own factor
    /// ([`CosineWindow::cola_overlap`]). Under that condition the inverse's
    /// per-sample window-sum normalization is exact, so untouched bins
    /// reconstruct to float precision.
    ///
    /// Built on [`CosineWindow::HANN`]; use [`cola_with`](Self::cola_with) to check
    /// a different shape, whose required overlap may be stricter — Blackman
    /// needs 8x where Hann needs 4x.
    ///
    /// Checked here rather than left to the forward transform: a hop five times
    /// wider than its window otherwise reaches the inverse and produces a comb
    /// of islands separated by silence.
    ///
    /// # Errors
    /// Returns [`StftError::HopExceedsWindow`] if frames do not overlap, or
    /// [`StftError::NotColaCompliant`] if the hop does not divide the
    /// window at 4x overlap — this constructor fixes the shape as Hann, which
    /// is where that figure comes from. Use
    /// [`cola_with`](Self::cola_with) for another window.
    pub fn cola(
        sample_rate: impl Into<SampleRate>,
        window: impl Into<Samples>,
        hop: impl Into<Samples>,
    ) -> Result<Self> {
        Self::cola_with(sample_rate, window, hop, CosineWindow::HANN)
    }

    /// [`cola`](Self::cola) for a given window shape.
    ///
    /// Separate from `with_window_fn` because the check is the point: a grid
    /// that reconstructs under Hann may not under Blackman, and the difference
    /// is silent — the transform runs, and the output has a 2.1% amplitude
    /// ripple that reads as a tremolo nobody asked for.
    ///
    /// # Errors
    ///
    /// Everything [`new`](Self::new) returns, plus
    /// [`StftError::HopExceedsWindow`] when frames do not overlap and
    /// [`StftError::NotColaCompliant`] when the hop does not divide the window at
    /// the overlap `window_fn` requires.
    pub fn cola_with(
        sample_rate: impl Into<SampleRate>,
        window: impl Into<Samples>,
        hop: impl Into<Samples>,
        window_fn: CosineWindow,
    ) -> Result<Self> {
        let geometry = Self::new(sample_rate, window, hop)?.with_window_fn(window_fn);
        if !geometry.frames_overlap() {
            return Err(StftError::HopExceedsWindow {
                window: geometry.window,
                hop: geometry.hop,
            });
        }
        if !geometry.is_cola() {
            return Err(StftError::NotColaCompliant {
                window: geometry.window,
                hop: geometry.hop,
                window_fn,
            });
        }
        Ok(geometry)
    }

    /// The analysis window in [`Samples`] — also the FFT size.
    #[inline]
    #[must_use]
    pub const fn window(self) -> Samples {
        self.window
    }

    /// How far frames advance, in [`Samples`]. Smaller than the window whenever
    /// frames overlap.
    #[inline]
    #[must_use]
    pub const fn hop(self) -> Samples {
        self.hop
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

    /// The same grid analysed with a different window shape.
    ///
    /// A builder rather than a fourth constructor argument: every call site in
    /// the tree wants [`CosineWindow::HANN`], and a fourth positional parameter on
    /// two fallible three-argument constructors is exactly the transposition
    /// hazard `new` exists to reject.
    ///
    /// **Infallible, and that is not the same as safe.** A grid built by
    /// [`cola`](Self::cola) for one window may not be COLA for another, and
    /// this does not re-check — Blackman needs 8x overlap where Hann needs 4x,
    /// so `cola(rate, 2048, 512).with_window_fn(Blackman)` yields a grid that
    /// analyses fine and reconstructs with a 2.1% ripple. Use
    /// [`cola_with`](Self::cola_with) when the result must invert.
    #[inline]
    #[must_use]
    pub const fn with_window_fn(mut self, window_fn: CosineWindow) -> Self {
        self.window_fn = window_fn;
        self
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
        self.window.get().is_multiple_of(self.hop.get())
            && self.window.get() / self.hop.get() >= self.window_fn.cola_overlap()
    }

    /// Real-spectrum bins including Nyquist — the complex path's count.
    #[inline]
    #[must_use]
    pub fn bins_per_frame(self) -> BinCount {
        BinCount(self.window.get() / 2 + 1)
    }

    /// The centre frequency of a bin.
    #[inline]
    #[must_use]
    pub fn bin_frequency(self, bin: BinIndex) -> Hz {
        Hz((bin.get() as f64 * self.sample_rate.get() / self.window.get() as f64) as f32)
    }

    /// The fractional bin `frequency` falls on, `frequency * window / sample_rate`,
    /// computed in `f64`: bin `k` is centred on `bin_of(bin_frequency(k)) == k`,
    /// and a frequency on a bin boundary gives a whole number.
    #[inline]
    #[must_use]
    pub fn bin_of(self, frequency: Hz) -> f64 {
        self.bin_of_f64(f64::from(frequency.get()))
    }

    /// [`bin_of`](Self::bin_of) for a frequency in hertz held in `f64`, such as a band edge
    /// designed at `f64` precision, which an `f32` [`Hz`] would round before the index is taken.
    #[inline]
    #[must_use]
    pub fn bin_of_f64(self, frequency: f64) -> f64 {
        frequency * self.window.get() as f64 / self.sample_rate.get()
    }

    /// The frequency of a fractional bin, the inverse of [`bin_of`](Self::bin_of):
    /// computed in `f64` and narrowed once.
    #[inline]
    #[must_use]
    pub fn frequency_of(self, bin: f64) -> Hz {
        Hz((bin * self.sample_rate.get() / self.window.get() as f64) as f32)
    }

    /// The time between frames, `hop / sample_rate`.
    #[inline]
    #[must_use]
    pub fn frame_interval(self) -> Seconds {
        Seconds(self.hop.get() as f64 / self.sample_rate.get())
    }

    /// How many whole frames fit over `len` samples.
    #[inline]
    pub fn frames_for(self, len: impl Into<Samples>) -> FrameCount {
        let len = len.into().get();
        if len < self.window.get() {
            return FrameCount(0);
        }
        FrameCount((len - self.window.get()) / self.hop.get() + 1)
    }

    /// How many samples resynthesizing `frames` frames produces.
    #[inline]
    #[must_use]
    pub fn output_len(self, frames: FrameCount) -> Samples {
        if frames.get() == 0 {
            return Samples(0);
        }
        Samples((frames.get() - 1) * self.hop.get() + self.window.get())
    }

    /// The delay this grid imposes: one whole window must arrive before the
    /// first frame can be analyzed.
    #[inline]
    #[must_use]
    pub fn latency(self) -> Seconds {
        Seconds(self.window.get() as f64 / self.sample_rate.get())
    }

    /// The analysis window's coefficients, `window` points of `window_fn`.
    ///
    /// Deliberately longer than its neighbours: it **allocates**, where
    /// [`window`](Self::window) and [`window_fn`](Self::window_fn) are `const`
    /// field reads. A short name would put an allocation and two free reads at
    /// the same apparent cost.
    #[inline]
    #[must_use]
    pub fn window_coefficients(self) -> Vec<f32> {
        self.window_fn.coefficients(self.window.get())
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
    pub fn magnitude_gain(self) -> f32 {
        self.window_fn.coherent_gain() * self.window.get().max(1) as f32
    }
}

#[cfg(test)]
mod tests;
