//! The frames an analysis reads: a window and the hop between them, the cutter
//! that takes them from a stream, and the band check every framed analysis
//! shares.

use tutti_dsp::StftGeometry;
use tutti_types::{Hz, SampleRate, Samples};

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
        self.check()?;
        Ok(StftGeometry::new(rate, self.window, self.hop)?)
    }
}

/// Refuses a frequency above half `rate`.
///
/// # Errors
///
/// [`Error::AboveNyquist`] when `top` exceeds half the sample rate.
pub(crate) fn check_nyquist(top: Hz, rate: SampleRate) -> Result<()> {
    let nyquist = Hz((rate.get() / 2.0) as f32);
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
    window: usize,
    hop: usize,
    /// The samples from absolute index `base` that no frame has finished with.
    buffer: Vec<f32>,
    base: usize,
    /// The first sample of the next frame.
    next_start: usize,
    /// The frames cut so far.
    frames: usize,
}

impl FrameCutter {
    pub(crate) fn new(framing: Framing) -> Self {
        let (window, hop) = (framing.window.get(), framing.hop.get());
        Self {
            window,
            hop,
            buffer: Vec::with_capacity(window + BLOCK + hop),
            base: 0,
            next_start: 0,
            frames: 0,
        }
    }

    /// Takes `block` and calls `each(index, frame)` for every frame it
    /// completes.
    pub(crate) fn feed(&mut self, block: &[f32], mut each: impl FnMut(usize, &[f32])) {
        self.buffer.extend_from_slice(block);
        while self.next_start + self.window <= self.base + self.buffer.len() {
            let at = self.next_start - self.base;
            each(self.frames, &self.buffer[at..at + self.window]);
            self.frames += 1;
            self.next_start += self.hop;
        }
        let done = (self.next_start - self.base).min(self.buffer.len());
        self.buffer.drain(..done);
        self.base += done;
    }

    /// Forgets everything fed, keeping the buffer.
    pub(crate) fn reset(&mut self) {
        self.buffer.clear();
        self.base = 0;
        self.next_start = 0;
        self.frames = 0;
    }
}
