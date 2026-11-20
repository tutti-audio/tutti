//! The frames an analysis reads: a window and the hop between them, the cutter
//! that takes them from a stream, and the band check every framed analysis
//! shares.

use core::ops::RangeInclusive;

use tutti_dsp::{FrameCount, HopSize, StftGeometry, WindowLength};
use tutti_units::{Hz, SampleRate, Samples};

use crate::convert::BLOCK;
use crate::{Error, Result};

/// A window and the hop between successive windows, in [`Samples`].
///
/// A framing holds no sample rate: the call that takes the config and the
/// signal binds the rate and derives the [`StftGeometry`], and checks the
/// framing against what the analysis accepts (a window range, a hop no wider
/// than the window, the overlap a tiny hop makes), answering with a named
/// [`Error`]. A zero window or hop is [`Error::ZeroWindow`] or
/// [`Error::ZeroHop`], wherever a framing is first used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Framing {
    window: Samples,
    hop: Samples,
}

impl Framing {
    /// Frames `window` samples long, `hop` samples apart.
    #[must_use]
    pub const fn new(window: Samples, hop: Samples) -> Self {
        Self { window, hop }
    }

    /// The frame length, in [`Samples`].
    #[inline]
    #[must_use]
    pub const fn window(&self) -> Samples {
        self.window
    }

    /// The advance between frames, in [`Samples`].
    #[inline]
    #[must_use]
    pub const fn hop(&self) -> Samples {
        self.hop
    }

    /// Whether the window and the hop are not zero.
    ///
    /// # Errors
    ///
    /// [`Error::ZeroWindow`] for a zero window, [`Error::ZeroHop`] for a zero
    /// hop.
    pub(crate) fn check(self) -> Result<()> {
        if self.window.is_zero() {
            return Err(Error::ZeroWindow);
        }
        if self.hop.is_zero() {
            return Err(Error::ZeroHop);
        }
        Ok(())
    }

    /// The geometry of these frames at `rate`.
    ///
    /// # Errors
    ///
    /// As [`check`](Self::check).
    pub(crate) fn geometry(self, rate: SampleRate) -> Result<StftGeometry> {
        let window = WindowLength::new(self.window).ok_or(Error::ZeroWindow)?;
        let hop = HopSize::new(self.hop).ok_or(Error::ZeroHop)?;
        Ok(StftGeometry::new(rate, window, hop))
    }
}

/// The most frames that may overlap in one window; each frame is a whole
/// transform, so this bounds the cost of a tiny hop.
pub(crate) const MAX_OVERLAP: usize = 64;

impl Framing {
    /// How many whole frames fit over `len` samples: none for a signal shorter
    /// than the window, otherwise `(len - window) / hop + 1`.
    pub(crate) fn frames_for(self, len: Samples) -> FrameCount {
        FrameCount::fitting(len, self.window, self.hop)
    }
}

/// Refuses a `window` outside `accepted`: the FFT works on the window, so an
/// absurd one would ask for gigabytes, and a tiny one resolves nothing.
///
/// # Errors
///
/// [`Error::WindowOutOfRange`].
pub(crate) fn check_window(window: Samples, accepted: &RangeInclusive<Samples>) -> Result<()> {
    if accepted.contains(&window) {
        return Ok(());
    }
    Err(Error::WindowOutOfRange {
        window,
        min: *accepted.start(),
        max: *accepted.end(),
    })
}

/// Refuses a `hop` wider than the `window`: the samples between frames would be
/// read by none.
///
/// # Errors
///
/// [`Error::HopWiderThanWindow`].
pub(crate) fn check_hop_within(window: Samples, hop: Samples) -> Result<()> {
    if hop > window {
        return Err(Error::HopWiderThanWindow { hop, window });
    }
    Ok(())
}

/// Refuses a `hop` that puts more than [`MAX_OVERLAP`] frames in one `window`.
///
/// # Errors
///
/// [`Error::TooManyOverlaps`].
pub(crate) fn check_overlap(window: Samples, hop: Samples) -> Result<()> {
    if window.get().div_ceil(hop.get()) > MAX_OVERLAP {
        return Err(Error::TooManyOverlaps {
            window,
            hop,
            max: MAX_OVERLAP,
        });
    }
    Ok(())
}

/// Refuses `frames` beyond `max`, which bounds what one call returns.
///
/// # Errors
///
/// [`Error::TooManyFrames`].
pub(crate) fn check_frame_count(frames: FrameCount, max: FrameCount) -> Result<()> {
    if frames > max {
        return Err(Error::TooManyFrames { frames, max });
    }
    Ok(())
}

/// Refuses a frequency above half `rate`.
///
/// # Errors
///
/// [`Error::AboveNyquist`] when `top` exceeds half the sample rate.
pub(crate) fn check_nyquist(top: Hz, rate: SampleRate) -> Result<()> {
    let nyquist = rate.nyquist();
    if top > nyquist {
        return Err(Error::AboveNyquist { freq: top, nyquist });
    }
    Ok(())
}

/// Cuts a stream, fed in chunks of any length, into frames of one window every
/// hop, and hands each to the caller in order.
///
/// A frame is cut as soon as its window is in, so the frames do not depend on
/// how the stream is chunked. The buffer holds at most a window, the block
/// being fed and a hop, and is sized for that when the cutter is made, so
/// feeding allocates nothing as long as blocks are at most `BLOCK` samples.
#[derive(Debug, Clone)]
pub(crate) struct FrameCutter {
    framing: Framing,
    /// The samples from absolute index `base` that no frame has finished with.
    buffer: Vec<f64>,
    base: Samples,
    /// The first sample of the next frame.
    next_start: Samples,
    /// The frames cut so far.
    frames: FrameCount,
}

impl FrameCutter {
    pub(crate) fn new(framing: Framing) -> Self {
        Self {
            framing,
            buffer: Vec::with_capacity(framing.window.get() + BLOCK + framing.hop.get()),
            base: Samples::new(0),
            next_start: Samples::new(0),
            frames: FrameCount(0),
        }
    }

    /// Takes `block` and calls `each(index, frame)` for every frame it
    /// completes.
    pub(crate) fn feed(&mut self, block: &[f64], mut each: impl FnMut(usize, &[f64])) {
        let (window, hop) = (self.framing.window, self.framing.hop);
        self.buffer.extend_from_slice(block);
        let held = Samples::new(self.buffer.len());
        while self.next_start + window <= self.base + held {
            let at = self.next_start.remaining_after(self.base).get();
            each(self.frames.get(), &self.buffer[at..at + window.get()]);
            self.frames = FrameCount(self.frames.get() + 1);
            self.next_start += hop;
        }
        let done = self
            .next_start
            .remaining_after(self.base)
            .get()
            .min(held.get());
        self.buffer.drain(..done);
        self.base += Samples::new(done);
    }

    /// Forgets everything fed, keeping the buffer.
    pub(crate) fn reset(&mut self) {
        self.buffer.clear();
        self.base = Samples::new(0);
        self.next_start = Samples::new(0);
        self.frames = FrameCount(0);
    }
}

#[cfg(test)]
mod tests;
