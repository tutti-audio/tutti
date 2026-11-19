//! [`Mono`] and [`MonoMut`]: one channel's run of samples.

use crate::ChannelLayout;
use crate::{Sample, bounds};
use core::fmt;
use core::ops::Range;
use tutti_units::Samples;

/// A borrowed run of one channel's samples.
///
/// Any slice is a valid mono run, so [`new`](Self::new) cannot fail. It reads
/// as `&[S]` through [`samples`](Self::samples) and [`AsRef`], and counts in
/// frames ([`Samples`]).
///
/// ```
/// use tutti_buffers::Mono;
/// use tutti_units::Samples;
///
/// let data = [0.1f32, 0.2, 0.3, 0.4];
/// let run = Mono::new(&data);
/// assert_eq!(run.frames(), Samples::new(4));
/// assert_eq!(run.slice(Samples::new(1)..Samples::new(3)).samples(), &[0.2, 0.3]);
/// assert_eq!(run.get_slice(Samples::new(3)..Samples::new(9)), None);
/// ```
#[derive(Clone, Copy, Default, PartialEq)]
#[repr(transparent)]
pub struct Mono<'a, S: Sample = f32>(pub(crate) &'a [S]);

impl<'a, S: Sample> Mono<'a, S> {
    /// Wraps `data` as one channel.
    #[inline]
    #[must_use]
    pub const fn new(data: &'a [S]) -> Self {
        Self(data)
    }

    /// Returns the frame count.
    #[inline]
    #[must_use]
    pub const fn frames(&self) -> Samples {
        Samples::new(self.0.len())
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the width, always [`ChannelLayout::MONO`].
    #[inline]
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        ChannelLayout::MONO
    }

    /// Returns the channel count, always 1.
    #[inline]
    #[must_use]
    pub const fn channels(&self) -> usize {
        1
    }

    /// Returns the samples as a slice.
    #[inline]
    #[must_use]
    pub const fn samples(&self) -> &'a [S] {
        self.0
    }

    /// Returns frame `i`.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn frame(&self, i: Samples) -> &'a S {
        &self.0[bounds::index(i, self.0.len())]
    }

    /// Returns frame `i`, or `None` past the end.
    #[inline]
    #[must_use]
    pub fn get_frame(&self, i: Samples) -> Option<&'a S> {
        self.0.get(i.get())
    }

    /// Iterates the samples in order.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'a, S> {
        self.0.iter()
    }

    /// Returns the sub-run covering a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds ([`get_slice`](Self::get_slice) is the
    /// form that returns `None`).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice(&self, frames: Range<Samples>) -> Mono<'a, S> {
        Mono(&self.0[bounds::slice(&frames, self.0.len())])
    }

    /// Returns the sub-run covering a frame range, or `None` if the range is
    /// out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice(&self, frames: Range<Samples>) -> Option<Mono<'a, S>> {
        bounds::get(&frames, self.0.len()).map(|r| Mono(&self.0[r]))
    }

    /// Splits into the first `at` frames and the rest.
    ///
    /// # Panics
    ///
    /// If `at` is past [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn split_at(&self, at: Samples) -> (Mono<'a, S>, Mono<'a, S>) {
        let (a, b) = self.0.split_at(bounds::split(at, self.0.len()));
        (Mono(a), Mono(b))
    }
}

impl<S: Sample> fmt::Debug for Mono<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mono")
            .field("frames", &self.frames())
            .finish()
    }
}

impl<S: Sample> AsRef<[S]> for Mono<'_, S> {
    #[inline]
    fn as_ref(&self) -> &[S] {
        self.0
    }
}

impl<'a, S: Sample> From<&'a [S]> for Mono<'a, S> {
    #[inline]
    fn from(data: &'a [S]) -> Self {
        Self(data)
    }
}

impl<'a, S: Sample> From<MonoMut<'a, S>> for Mono<'a, S> {
    #[inline]
    fn from(view: MonoMut<'a, S>) -> Self {
        Self(view.0)
    }
}

impl<'a, S: Sample> IntoIterator for Mono<'a, S> {
    type Item = &'a S;
    type IntoIter = core::slice::Iter<'a, S>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// The write side of [`Mono`].
///
/// Separate because a mutable borrow cannot be `Copy`.
#[repr(transparent)]
pub struct MonoMut<'a, S: Sample = f32>(pub(crate) &'a mut [S]);

impl<'a, S: Sample> MonoMut<'a, S> {
    /// Wraps `data` as one writable channel.
    #[inline]
    #[must_use]
    pub const fn new(data: &'a mut [S]) -> Self {
        Self(data)
    }

    /// Returns the frame count.
    #[inline]
    #[must_use]
    pub const fn frames(&self) -> Samples {
        Samples::new(self.0.len())
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the width, always [`ChannelLayout::MONO`].
    #[inline]
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        ChannelLayout::MONO
    }

    /// Returns the channel count, always 1.
    #[inline]
    #[must_use]
    pub const fn channels(&self) -> usize {
        1
    }

    /// Returns the samples as a slice.
    #[inline]
    #[must_use]
    pub const fn samples(&self) -> &[S] {
        self.0
    }

    /// Returns the samples as a mutable slice.
    #[inline]
    #[must_use]
    pub const fn samples_mut(&mut self) -> &mut [S] {
        self.0
    }

    /// Consumes the view into its samples, for the view's whole lifetime.
    #[inline]
    #[must_use]
    pub const fn into_samples_mut(self) -> &'a mut [S] {
        self.0
    }

    /// Reborrows as a read-only view.
    #[inline]
    #[must_use]
    pub const fn as_mono(&self) -> Mono<'_, S> {
        Mono(self.0)
    }

    /// Reborrows for a shorter lifetime, so the view can be passed on and used
    /// again.
    #[inline]
    #[must_use]
    pub const fn reborrow(&mut self) -> MonoMut<'_, S> {
        MonoMut(self.0)
    }

    /// Returns frame `i`.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn frame(&self, i: Samples) -> &S {
        &self.0[bounds::index(i, self.0.len())]
    }

    /// Returns frame `i`, or `None` past the end.
    #[inline]
    #[must_use]
    pub fn get_frame(&self, i: Samples) -> Option<&S> {
        self.0.get(i.get())
    }

    /// Returns frame `i` mutably.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn frame_mut(&mut self, i: Samples) -> &mut S {
        let at = bounds::index(i, self.0.len());
        &mut self.0[at]
    }

    /// Returns frame `i` mutably, or `None` past the end.
    #[inline]
    #[must_use]
    pub fn get_frame_mut(&mut self, i: Samples) -> Option<&mut S> {
        self.0.get_mut(i.get())
    }

    /// Returns the sub-run covering a frame range, read-only.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice(&self, frames: Range<Samples>) -> Mono<'_, S> {
        Mono(&self.0[bounds::slice(&frames, self.0.len())])
    }

    /// Returns the sub-run covering a frame range, read-only, or `None` if the
    /// range is out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice(&self, frames: Range<Samples>) -> Option<Mono<'_, S>> {
        bounds::get(&frames, self.0.len()).map(|r| Mono(&self.0[r]))
    }

    /// Returns the writable sub-run covering a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice_mut(&mut self, frames: Range<Samples>) -> MonoMut<'_, S> {
        let r = bounds::slice(&frames, self.0.len());
        MonoMut(&mut self.0[r])
    }

    /// Returns the writable sub-run covering a frame range, or `None` if the
    /// range is out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice_mut(&mut self, frames: Range<Samples>) -> Option<MonoMut<'_, S>> {
        bounds::get(&frames, self.0.len()).map(|r| MonoMut(&mut self.0[r]))
    }

    /// Splits into the first `at` frames and the rest, two disjoint views.
    ///
    /// # Panics
    ///
    /// If `at` is past [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn split_at_mut(&mut self, at: Samples) -> (MonoMut<'_, S>, MonoMut<'_, S>) {
        let at = bounds::split(at, self.0.len());
        let (a, b) = self.0.split_at_mut(at);
        (MonoMut(a), MonoMut(b))
    }
}

impl<S: Sample> fmt::Debug for MonoMut<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MonoMut")
            .field("frames", &self.frames())
            .finish()
    }
}

impl<S: Sample> AsRef<[S]> for MonoMut<'_, S> {
    #[inline]
    fn as_ref(&self) -> &[S] {
        self.0
    }
}

impl<S: Sample> AsMut<[S]> for MonoMut<'_, S> {
    #[inline]
    fn as_mut(&mut self) -> &mut [S] {
        self.0
    }
}

impl<'a, S: Sample> From<&'a mut [S]> for MonoMut<'a, S> {
    #[inline]
    fn from(data: &'a mut [S]) -> Self {
        Self(data)
    }
}
