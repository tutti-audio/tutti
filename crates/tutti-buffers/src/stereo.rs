//! [`StereoPlanes`]: exactly two equal-length planes.

use crate::ChannelLayout;
use crate::{BufferError, Mono, Sample, bounds};
use core::fmt;
use core::ops::Range;
use tutti_units::Samples;

/// Two equal-length channel planes: the deinterleaved stereo pair that
/// correlation and amplitude metering consume.
///
/// Mid/side and L/R correlation are defined at exactly two channels, so the
/// arity is in the type rather than a [`Planes`](crate::Planes) of any width.
/// Construction checks that the lengths agree, so a measurement never divides
/// by the wrong count. The pair is read-only: nothing writes a stereo pair as
/// one value.
///
/// ```
/// use tutti_buffers::StereoPlanes;
/// use tutti_units::Samples;
///
/// let (l, r) = ([1.0f32, 2.0], [3.0, 4.0]);
/// let pair = StereoPlanes::new(&l, &r)?;
/// assert_eq!(pair.frames(), Samples(2));
/// assert!(StereoPlanes::new(&l, &r[..1]).is_err());
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Clone, Copy, Default, PartialEq)]
pub struct StereoPlanes<'a, S: Sample = f32> {
    left: &'a [S],
    right: &'a [S],
}

impl<'a, S: Sample> StereoPlanes<'a, S> {
    /// Pairs two planes.
    ///
    /// # Errors
    ///
    /// [`BufferError::RaggedPlane`] if the right plane's length differs from
    /// the left's.
    #[inline]
    pub const fn new(left: &'a [S], right: &'a [S]) -> Result<Self, BufferError> {
        if left.len() == right.len() {
            Ok(Self { left, right })
        } else {
            Err(BufferError::RaggedPlane {
                plane: 1,
                expected: Samples(left.len()),
                found: Samples(right.len()),
            })
        }
    }

    /// Returns the frames in each plane.
    #[inline]
    #[must_use]
    pub const fn frames(&self) -> Samples {
        Samples(self.left.len())
    }

    /// Returns the width, always [`ChannelLayout::STEREO`].
    #[inline]
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    /// Returns the channel count, always 2.
    #[inline]
    #[must_use]
    pub const fn channels(&self) -> usize {
        2
    }

    /// Returns whether the planes are empty.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.left.is_empty()
    }

    /// Returns the left plane.
    #[inline]
    #[must_use]
    pub const fn left(&self) -> &'a [S] {
        self.left
    }

    /// Returns the right plane.
    #[inline]
    #[must_use]
    pub const fn right(&self) -> &'a [S] {
        self.right
    }

    /// Returns channel `c`: 0 is the left plane, 1 the right.
    ///
    /// # Panics
    ///
    /// If `c` is not 0 or 1.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn plane(&self, c: usize) -> Mono<'a, S> {
        match self.get_plane(c) {
            Some(plane) => plane,
            None => panic!("channel {c} is out of bounds for 2 channels"),
        }
    }

    /// Returns channel `c`, or `None` past the right plane.
    #[inline]
    #[must_use]
    pub const fn get_plane(&self, c: usize) -> Option<Mono<'a, S>> {
        match c {
            0 => Some(Mono(self.left)),
            1 => Some(Mono(self.right)),
            _ => None,
        }
    }

    /// Returns the sub-run of both planes covering a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice(&self, frames: Range<Samples>) -> StereoPlanes<'a, S> {
        let r = bounds::slice(&frames, self.left.len());
        StereoPlanes {
            left: &self.left[r.clone()],
            right: &self.right[r],
        }
    }

    /// Returns the sub-run of both planes covering a frame range, or `None` if
    /// the range is out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice(&self, frames: Range<Samples>) -> Option<StereoPlanes<'a, S>> {
        bounds::get(&frames, self.left.len()).map(|r| StereoPlanes {
            left: &self.left[r.clone()],
            right: &self.right[r],
        })
    }

    /// Splits into the first `at` frames and the rest.
    ///
    /// # Panics
    ///
    /// If `at` is past [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn split_at(&self, at: Samples) -> (StereoPlanes<'a, S>, StereoPlanes<'a, S>) {
        let at = bounds::split(at, self.left.len());
        let ((l0, l1), (r0, r1)) = (self.left.split_at(at), self.right.split_at(at));
        (
            StereoPlanes {
                left: l0,
                right: r0,
            },
            StereoPlanes {
                left: l1,
                right: r1,
            },
        )
    }
}

impl<S: Sample> fmt::Debug for StereoPlanes<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StereoPlanes")
            .field("frames", &self.frames())
            .finish()
    }
}
