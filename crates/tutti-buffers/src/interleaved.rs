//! [`Interleaved`] and [`InterleavedMut`]: a flat run of whole frames that
//! carries its own width.

use crate::{BufferError, Sample, bounds};
use core::fmt;
use core::iter::FusedIterator;
use core::ops::Range;
use tutti_types::{ChannelLayout, Samples};

/// Checks that `len` samples are whole frames of `layout`.
fn check(len: usize, layout: ChannelLayout) -> Result<(), BufferError> {
    if layout.count() == 0 {
        return Err(BufferError::ZeroWidth);
    }
    if !len.is_multiple_of(usize::from(layout.count())) {
        return Err(BufferError::PartialFrame { len, width: layout });
    }
    Ok(())
}

/// A borrowed run of whole interleaved frames that knows its own width.
///
/// A flat slice plus a separate channel count reads identically whether an
/// index is a frame index or a sample index; the two differ by the width.
/// Here the width travels with the buffer: [`frames`](Self::frames) counts
/// frames, and every range and index is in frames. Construction checks once
/// that the slice is whole frames; nothing after it re-checks.
///
/// # Not for inner loops
///
/// Take one at a signature, then destructure with [`samples`](Self::samples)
/// and [`channels`](Self::channels) at the top of the body and index raw below.
///
/// ```
/// use tutti_buffers::Interleaved;
/// use tutti_types::{ChannelLayout, Samples};
///
/// let data = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6]; // three stereo frames
/// let buf = Interleaved::new(&data, ChannelLayout::STEREO)?;
/// assert_eq!(buf.frames(), Samples(3));
/// assert_eq!(buf.frame(Samples(1)), &[0.3, 0.4]);
/// assert_eq!(buf.slice(Samples(1)..Samples(3)).samples(), &[0.3, 0.4, 0.5, 0.6]);
/// assert_eq!(buf.iter().len(), 3);
/// assert!(Interleaved::new(&data[..5], ChannelLayout::STEREO).is_err());
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Clone, Copy, PartialEq)]
pub struct Interleaved<'a, S: Sample = f32> {
    data: &'a [S],
    layout: ChannelLayout,
}

impl<'a, S: Sample> Interleaved<'a, S> {
    /// Wraps `data` as frames `layout` wide.
    ///
    /// # Errors
    ///
    /// [`BufferError::ZeroWidth`] if `layout` has no channels, and
    /// [`BufferError::PartialFrame`] if `data` ends mid-frame. Nothing is
    /// truncated.
    #[inline]
    pub fn new(data: &'a [S], layout: ChannelLayout) -> Result<Self, BufferError> {
        check(data.len(), layout)?;
        Ok(Self { data, layout })
    }

    /// Returns the width these frames carry.
    #[inline]
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the channel count, the interleave stride: `layout().count()` as
    /// a `usize`.
    #[inline]
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.layout.count() as usize
    }

    /// Returns the frame count, not the sample count.
    #[inline]
    #[must_use]
    pub const fn frames(&self) -> Samples {
        self.layout.from_interleaved_len(self.data.len())
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Returns the flat interleaved samples: what encoders, ring buffers and
    /// device callbacks want.
    #[inline]
    #[must_use]
    pub const fn samples(&self) -> &'a [S] {
        self.data
    }

    /// Returns the sub-run covering a frame range; the stride is applied here,
    /// once.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds ([`get_slice`](Self::get_slice) is the
    /// form that returns `None`).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice(&self, frames: Range<Samples>) -> Interleaved<'a, S> {
        let r = bounds::slice(&frames, self.frames().get());
        self.run(&r)
    }

    /// Returns the sub-run covering a frame range, or `None` if the range is
    /// out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice(&self, frames: Range<Samples>) -> Option<Interleaved<'a, S>> {
        bounds::get(&frames, self.frames().get()).map(|r| self.run(&r))
    }

    /// The sub-run of an in-bounds frame range.
    fn run(&self, r: &Range<usize>) -> Interleaved<'a, S> {
        let start = self.layout.interleaved_len(Samples(r.start));
        let end = self.layout.interleaved_len(Samples(r.end));
        Interleaved {
            data: &self.data[start..end],
            layout: self.layout,
        }
    }

    /// Splits into the first `at` frames and the rest.
    ///
    /// # Panics
    ///
    /// If `at` is past [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn split_at(&self, at: Samples) -> (Interleaved<'a, S>, Interleaved<'a, S>) {
        let at = bounds::split(at, self.frames().get());
        let (a, b) = self.data.split_at(self.layout.interleaved_len(Samples(at)));
        let layout = self.layout;
        (
            Interleaved { data: a, layout },
            Interleaved { data: b, layout },
        )
    }

    /// Returns one frame, by frame index.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn frame(&self, i: Samples) -> &'a [S] {
        let i = bounds::index(i, self.frames().get());
        let start = self.layout.interleaved_len(Samples(i));
        &self.data[start..start + self.channels()]
    }

    /// Returns one frame, by frame index, or `None` past the end.
    #[inline]
    #[must_use]
    pub fn get_frame(&self, i: Samples) -> Option<&'a [S]> {
        let start = self.layout.interleaved_len(i);
        self.data.get(start..start.checked_add(self.channels())?)
    }

    /// Iterates frame by frame; each item is [`channels`](Self::channels)
    /// long.
    #[inline]
    #[must_use]
    pub fn iter(&self) -> Frames<'a, S> {
        Frames(self.data.chunks_exact(self.channels()))
    }
}

impl<S: Sample> fmt::Debug for Interleaved<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interleaved")
            .field("frames", &self.frames())
            .field("layout", &self.layout)
            .finish()
    }
}

impl<S: Sample> AsRef<[S]> for Interleaved<'_, S> {
    #[inline]
    fn as_ref(&self) -> &[S] {
        self.data
    }
}

impl<'a, S: Sample> From<InterleavedMut<'a, S>> for Interleaved<'a, S> {
    #[inline]
    fn from(view: InterleavedMut<'a, S>) -> Self {
        Self {
            data: view.data,
            layout: view.layout,
        }
    }
}

impl<'a, S: Sample> IntoIterator for Interleaved<'a, S> {
    type Item = &'a [S];
    type IntoIter = Frames<'a, S>;

    #[inline]
    fn into_iter(self) -> Frames<'a, S> {
        self.iter()
    }
}

/// The write side of [`Interleaved`].
///
/// Separate because a mutable borrow cannot be `Copy`, and the read side is
/// passed around freely.
pub struct InterleavedMut<'a, S: Sample = f32> {
    data: &'a mut [S],
    layout: ChannelLayout,
}

impl<'a, S: Sample> InterleavedMut<'a, S> {
    /// Wraps `data` as writable frames `layout` wide.
    ///
    /// # Errors
    ///
    /// The same as [`Interleaved::new`].
    #[inline]
    pub fn new(data: &'a mut [S], layout: ChannelLayout) -> Result<Self, BufferError> {
        check(data.len(), layout)?;
        Ok(Self { data, layout })
    }

    /// Returns the width these frames carry.
    #[inline]
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the channel count, the interleave stride.
    #[inline]
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.layout.count() as usize
    }

    /// Returns the frame count, not the sample count.
    #[inline]
    #[must_use]
    pub const fn frames(&self) -> Samples {
        self.layout.from_interleaved_len(self.data.len())
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Returns the flat interleaved samples.
    #[inline]
    #[must_use]
    pub const fn samples(&self) -> &[S] {
        self.data
    }

    /// Returns the flat interleaved samples, mutably.
    #[inline]
    #[must_use]
    pub const fn samples_mut(&mut self) -> &mut [S] {
        self.data
    }

    /// Consumes the view into its flat samples, for the view's whole lifetime.
    #[inline]
    #[must_use]
    pub const fn into_samples_mut(self) -> &'a mut [S] {
        self.data
    }

    /// Reborrows as a read-only view.
    #[inline]
    #[must_use]
    pub const fn as_interleaved(&self) -> Interleaved<'_, S> {
        Interleaved {
            data: self.data,
            layout: self.layout,
        }
    }

    /// Reborrows for a shorter lifetime, so the view can be passed on and used
    /// again.
    #[inline]
    #[must_use]
    pub const fn reborrow(&mut self) -> InterleavedMut<'_, S> {
        InterleavedMut {
            data: self.data,
            layout: self.layout,
        }
    }

    /// Returns the sub-run covering a frame range, read-only.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice(&self, frames: Range<Samples>) -> Interleaved<'_, S> {
        self.as_interleaved().slice(frames)
    }

    /// Returns the sub-run covering a frame range, read-only, or `None` if the
    /// range is out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice(&self, frames: Range<Samples>) -> Option<Interleaved<'_, S>> {
        self.as_interleaved().get_slice(frames)
    }

    /// Returns the writable sub-run covering a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice_mut(&mut self, frames: Range<Samples>) -> InterleavedMut<'_, S> {
        let r = bounds::slice(&frames, self.frames().get());
        self.run_mut(&r)
    }

    /// Returns the writable sub-run covering a frame range, or `None` if the
    /// range is out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice_mut(&mut self, frames: Range<Samples>) -> Option<InterleavedMut<'_, S>> {
        bounds::get(&frames, self.frames().get()).map(|r| self.run_mut(&r))
    }

    /// The writable sub-run of an in-bounds frame range.
    fn run_mut(&mut self, r: &Range<usize>) -> InterleavedMut<'_, S> {
        let start = self.layout.interleaved_len(Samples(r.start));
        let end = self.layout.interleaved_len(Samples(r.end));
        InterleavedMut {
            data: &mut self.data[start..end],
            layout: self.layout,
        }
    }

    /// Splits into the first `at` frames and the rest, two disjoint views.
    ///
    /// # Panics
    ///
    /// If `at` is past [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn split_at_mut(&mut self, at: Samples) -> (InterleavedMut<'_, S>, InterleavedMut<'_, S>) {
        let at = bounds::split(at, self.frames().get());
        let layout = self.layout;
        let (a, b) = self.data.split_at_mut(layout.interleaved_len(Samples(at)));
        (
            InterleavedMut { data: a, layout },
            InterleavedMut { data: b, layout },
        )
    }

    /// Returns one frame, by frame index.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn frame(&self, i: Samples) -> &[S] {
        self.as_interleaved().frame(i)
    }

    /// Returns one frame, by frame index, or `None` past the end.
    #[inline]
    #[must_use]
    pub fn get_frame(&self, i: Samples) -> Option<&[S]> {
        self.as_interleaved().get_frame(i)
    }

    /// Returns one frame mutably, by frame index.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn frame_mut(&mut self, i: Samples) -> &mut [S] {
        let i = bounds::index(i, self.frames().get());
        let start = self.layout.interleaved_len(Samples(i));
        let channels = self.channels();
        &mut self.data[start..start + channels]
    }

    /// Returns one frame mutably, by frame index, or `None` past the end.
    #[inline]
    #[must_use]
    pub fn get_frame_mut(&mut self, i: Samples) -> Option<&mut [S]> {
        let start = self.layout.interleaved_len(i);
        let end = start.checked_add(self.channels())?;
        self.data.get_mut(start..end)
    }

    /// Iterates frame by frame; each item is [`channels`](Self::channels)
    /// long.
    #[inline]
    #[must_use]
    pub fn iter(&self) -> Frames<'_, S> {
        self.as_interleaved().iter()
    }

    /// Iterates frame by frame, mutably; each item is
    /// [`channels`](Self::channels) long.
    #[inline]
    #[must_use]
    pub fn iter_mut(&mut self) -> FramesMut<'_, S> {
        let channels = self.channels();
        FramesMut(self.data.chunks_exact_mut(channels))
    }
}

impl<S: Sample> fmt::Debug for InterleavedMut<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InterleavedMut")
            .field("frames", &self.frames())
            .field("layout", &self.layout)
            .finish()
    }
}

impl<S: Sample> AsRef<[S]> for InterleavedMut<'_, S> {
    #[inline]
    fn as_ref(&self) -> &[S] {
        self.data
    }
}

impl<S: Sample> AsMut<[S]> for InterleavedMut<'_, S> {
    #[inline]
    fn as_mut(&mut self) -> &mut [S] {
        self.data
    }
}

/// The frames of an [`Interleaved`], in order: each item is one frame's
/// samples.
///
/// A named type so a struct can hold one; it knows its remaining length and
/// runs from either end.
#[derive(Clone, Debug)]
pub struct Frames<'a, S: Sample = f32>(core::slice::ChunksExact<'a, S>);

impl<'a, S: Sample> Iterator for Frames<'a, S> {
    type Item = &'a [S];

    #[inline]
    fn next(&mut self) -> Option<&'a [S]> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<&'a [S]> {
        self.0.nth(n)
    }
}

impl<S: Sample> DoubleEndedIterator for Frames<'_, S> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back()
    }
}

impl<S: Sample> ExactSizeIterator for Frames<'_, S> {}

impl<S: Sample> FusedIterator for Frames<'_, S> {}

/// The frames of an [`InterleavedMut`], in order: each item is one frame's
/// samples, writable and disjoint from the others.
#[derive(Debug)]
pub struct FramesMut<'a, S: Sample = f32>(core::slice::ChunksExactMut<'a, S>);

impl<'a, S: Sample> Iterator for FramesMut<'a, S> {
    type Item = &'a mut [S];

    #[inline]
    fn next(&mut self) -> Option<&'a mut [S]> {
        self.0.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<&'a mut [S]> {
        self.0.nth(n)
    }
}

impl<S: Sample> DoubleEndedIterator for FramesMut<'_, S> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back()
    }
}

impl<S: Sample> ExactSizeIterator for FramesMut<'_, S> {}

impl<S: Sample> FusedIterator for FramesMut<'_, S> {}
