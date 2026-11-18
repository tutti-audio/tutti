//! The frames an analysis reads: a window and the hop between them, the cutter
//! that takes them from a stream, and the band check every framed analysis
//! shares.

use core::ops::RangeInclusive;

use tutti_dsp::{FrameCount, StftGeometry};
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
    framing: Framing,
    /// The samples from absolute index `base` that no frame has finished with.
    buffer: Vec<f32>,
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
            base: Samples(0),
            next_start: Samples(0),
            frames: FrameCount(0),
        }
    }

    /// Takes `block` and calls `each(index, frame)` for every frame it
    /// completes.
    pub(crate) fn feed(&mut self, block: &[f32], mut each: impl FnMut(usize, &[f32])) {
        let (window, hop) = (self.framing.window, self.framing.hop);
        self.buffer.extend_from_slice(block);
        let held = Samples(self.buffer.len());
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
        self.base += Samples(done);
    }

    /// Forgets everything fed, keeping the buffer.
    pub(crate) fn reset(&mut self) {
        self.buffer.clear();
        self.base = Samples(0);
        self.next_start = Samples(0);
        self.frames = FrameCount(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Samples = Samples(1024);
    const HOP: Samples = Samples(256);

    /// `(len - window) / hop + 1` whole frames fit: none below one window, one
    /// at exactly a window, the next at a window plus a hop.
    #[test]
    fn whole_frames_fit_at_window_plus_a_hop_each() {
        let f = Framing::new(WINDOW, HOP);
        for (len, frames) in [
            (0, 0),
            (1023, 0),
            (1024, 1),
            (1279, 1),
            (1280, 2),
            (4096, 13),
        ] {
            assert_eq!(f.frames_for(Samples(len)), FrameCount(frames), "{len}");
        }
    }

    /// A zero window or hop fits no frame, rather than dividing by zero.
    #[test]
    fn a_zero_hop_or_window_fits_no_frame() {
        assert_eq!(
            Framing::new(WINDOW, Samples(0)).frames_for(Samples(9999)),
            FrameCount(0)
        );
        assert_eq!(
            Framing::new(Samples(0), HOP).frames_for(Samples(9999)),
            FrameCount(0)
        );
    }

    /// A hop wider than the window leaves the samples between frames unread:
    /// frames start at `0, hop, 2 hop ..` and the last must end inside `len`.
    #[test]
    fn a_hop_wider_than_the_window_counts_the_frames_that_fit() {
        let f = Framing::new(Samples(100), Samples(300));
        assert_eq!(f.frames_for(Samples(99)), FrameCount(0));
        assert_eq!(f.frames_for(Samples(100)), FrameCount(1));
        assert_eq!(f.frames_for(Samples(399)), FrameCount(1));
        assert_eq!(f.frames_for(Samples(400)), FrameCount(2));
    }

    /// The window range includes both ends.
    #[test]
    fn the_window_range_includes_both_ends() {
        let accepted = Samples(64)..=Samples(256);
        assert!(check_window(Samples(64), &accepted).is_ok());
        assert!(check_window(Samples(256), &accepted).is_ok());
        for window in [63, 257] {
            assert_eq!(
                check_window(Samples(window), &accepted),
                Err(Error::WindowOutOfRange {
                    window: Samples(window),
                    min: Samples(64),
                    max: Samples(256)
                })
            );
        }
    }

    /// A hop equal to the window is within it; one more sample is not.
    #[test]
    fn a_hop_is_within_a_window_up_to_its_length() {
        assert!(check_hop_within(WINDOW, WINDOW).is_ok());
        assert_eq!(
            check_hop_within(WINDOW, Samples(1025)),
            Err(Error::HopWiderThanWindow {
                hop: Samples(1025),
                window: WINDOW
            })
        );
    }

    /// Exactly `MAX_OVERLAP` hops across a window are accepted: a window of 64
    /// hops is, a window of 64 hops and one sample is not.
    #[test]
    fn the_overlap_limit_is_exact() {
        assert!(check_overlap(Samples(4096), Samples(64)).is_ok());
        assert_eq!(
            check_overlap(Samples(4097), Samples(64)),
            Err(Error::TooManyOverlaps {
                window: Samples(4097),
                hop: Samples(64),
                max: MAX_OVERLAP
            })
        );
    }

    /// The frame limit is inclusive.
    #[test]
    fn the_frame_limit_is_inclusive() {
        let max = FrameCount(10);
        assert!(check_frame_count(FrameCount(10), max).is_ok());
        assert_eq!(
            check_frame_count(FrameCount(11), max),
            Err(Error::TooManyFrames {
                frames: FrameCount(11),
                max
            })
        );
    }
}
