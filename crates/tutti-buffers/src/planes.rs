//! [`Planes`] and [`PlanesMut`]: one equal-length run per channel.

use crate::{BufferError, Mono, MonoMut, Sample, bounds};
use core::fmt;
use core::iter::FusedIterator;
use core::ops::Range;
use tutti_types::{ChannelLayout, Samples};

/// Checks the planes' width and that every plane is as long as the first,
/// returning the width and the common length.
fn check<S: Sample, P: AsRef<[S]>>(planes: &[P]) -> Result<(ChannelLayout, Samples), BufferError> {
    let Some(first) = planes.first() else {
        return Err(BufferError::ZeroWidth);
    };
    let layout = ChannelLayout::try_from(planes.len()).map_err(|_| BufferError::TooWide {
        channels: planes.len(),
    })?;
    let expected = Samples(first.as_ref().len());
    for (plane, p) in planes.iter().enumerate() {
        let found = Samples(p.as_ref().len());
        if found != expected {
            return Err(BufferError::RaggedPlane {
                plane,
                expected,
                found,
            });
        }
    }
    Ok((layout, expected))
}

/// Borrowed planar audio: one equal-length run per channel.
///
/// The width is the number of planes. Construction checks once that there is
/// at least one plane and that no plane is longer or shorter than the first;
/// nothing after it re-checks. The planes come from a table of slices
/// ([`new`](Self::new)) or from the `Vec`s a caller already holds
/// ([`from_vecs`](Self::from_vecs)).
///
/// [`default`](Default::default) is the view of no channels and no frames: the
/// one `Planes` that [`new`](Self::new) would refuse, there to stand where a
/// view is wanted and none is.
///
/// Two views are equal when they have the same width and every plane holds the
/// same samples over the frames the view covers.
///
/// ```
/// use tutti_buffers::Planes;
/// use tutti_types::Samples;
///
/// let (l, r) = ([1.0f32, 2.0, 3.0], [4.0, 5.0, 6.0]);
/// let slices = [&l[..], &r[..]];
/// let planes = Planes::new(&slices)?;
/// assert_eq!(planes.frames(), Samples(3));
/// assert_eq!(planes.plane(1).samples(), &[4.0, 5.0, 6.0]);
/// assert_eq!(planes.iter().len(), 2);
/// assert!(Planes::new(&[&l[..], &r[..2]]).is_err());
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Clone, Copy)]
pub struct Planes<'a, S: Sample = f32> {
    planes: Table<'a, S>,
    layout: ChannelLayout,
    start: usize,
    frames: usize,
}

/// The channel slices a [`Planes`] reads: its own shared ones, a
/// [`PlanesMut`]'s read through a shared borrow of them, or a caller's `Vec`s.
enum Table<'a, S> {
    Shared(&'a [&'a [S]]),
    Unique(&'a [&'a mut [S]]),
    Vecs(&'a [Vec<S>]),
}

impl<S> Clone for Table<'_, S> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<S> Copy for Table<'_, S> {}

impl<'a, S> Table<'a, S> {
    /// Channel `c`, whole, or `None` past the last.
    #[inline]
    fn get(self, c: usize) -> Option<&'a [S]> {
        match self {
            Self::Shared(p) => p.get(c).copied(),
            Self::Unique(p) => p.get(c).map(|p| &**p),
            Self::Vecs(p) => p.get(c).map(Vec::as_slice),
        }
    }

    #[inline]
    fn len(self) -> usize {
        match self {
            Self::Shared(p) => p.len(),
            Self::Unique(p) => p.len(),
            Self::Vecs(p) => p.len(),
        }
    }
}

impl<'a, S: Sample> Planes<'a, S> {
    /// Wraps `planes`, one slice per channel.
    ///
    /// # Errors
    ///
    /// [`BufferError::ZeroWidth`] for no planes, [`BufferError::TooWide`] for
    /// more than `u16::MAX`, and [`BufferError::RaggedPlane`] for a plane whose
    /// length differs from the first's. Nothing is truncated.
    pub fn new(planes: &'a [&'a [S]]) -> Result<Self, BufferError> {
        let (layout, frames) = check(planes)?;
        Ok(Self {
            planes: Table::Shared(planes),
            layout,
            start: 0,
            frames: frames.get(),
        })
    }

    /// Wraps `planes`, one `Vec` per channel, each read whole. Nothing is
    /// copied.
    ///
    /// # Errors
    ///
    /// The same as [`new`](Self::new).
    pub fn from_vecs(planes: &'a [Vec<S>]) -> Result<Self, BufferError> {
        let (layout, frames) = check(planes)?;
        Ok(Self {
            planes: Table::Vecs(planes),
            layout,
            start: 0,
            frames: frames.get(),
        })
    }

    /// Returns the width: the number of planes.
    #[inline]
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the channel count.
    #[inline]
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.layout.count() as usize
    }

    /// Returns the frame count of every plane.
    #[inline]
    #[must_use]
    pub const fn frames(&self) -> Samples {
        Samples(self.frames)
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// Returns channel `c`.
    ///
    /// # Panics
    ///
    /// If `c` is not below the width ([`get_plane`](Self::get_plane) is the
    /// form that returns `None`).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn plane(&self, c: usize) -> Mono<'a, S> {
        match self.get_plane(c) {
            Some(plane) => plane,
            None => panic!(
                "channel {c} is out of bounds for {} channels",
                self.channels()
            ),
        }
    }

    /// Returns channel `c`, or `None` past the last.
    #[inline]
    #[must_use]
    pub fn get_plane(&self, c: usize) -> Option<Mono<'a, S>> {
        let plane = self.planes.get(c)?;
        Some(Mono(&plane[self.start..self.start + self.frames]))
    }

    /// Iterates the channels in order.
    #[inline]
    #[must_use]
    pub fn iter(&self) -> PlaneIter<'a, S> {
        PlaneIter {
            table: self.planes,
            channels: 0..self.planes.len(),
            start: self.start,
            frames: self.frames,
        }
    }

    /// Returns the sub-run of every plane covering a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds ([`get_slice`](Self::get_slice) is the
    /// form that returns `None`).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice(&self, frames: Range<Samples>) -> Planes<'a, S> {
        self.run(&bounds::slice(&frames, self.frames))
    }

    /// Returns the sub-run of every plane covering a frame range, or `None` if
    /// the range is out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice(&self, frames: Range<Samples>) -> Option<Planes<'a, S>> {
        bounds::get(&frames, self.frames).map(|r| self.run(&r))
    }

    /// The sub-run of an in-bounds frame range.
    #[inline]
    fn run(&self, r: &Range<usize>) -> Planes<'a, S> {
        Planes {
            start: self.start + r.start,
            frames: r.end - r.start,
            ..*self
        }
    }

    /// Splits every plane into the first `at` frames and the rest.
    ///
    /// # Panics
    ///
    /// If `at` is past [`frames`](Self::frames).
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn split_at(&self, at: Samples) -> (Planes<'a, S>, Planes<'a, S>) {
        let at = bounds::split(at, self.frames);
        (self.run(&(0..at)), self.run(&(at..self.frames)))
    }
}

impl<S: Sample> Default for Planes<'_, S> {
    /// No planes, no frames: the empty view. It has no channel, so
    /// [`plane(0)`](Planes::plane) panics on it; [`get_plane`](Planes::get_plane)
    /// is the checked form.
    fn default() -> Self {
        Self {
            planes: Table::Shared(&[]),
            layout: ChannelLayout::EMPTY,
            start: 0,
            frames: 0,
        }
    }
}

impl<S: Sample> PartialEq for Planes<'_, S> {
    fn eq(&self, other: &Self) -> bool {
        self.layout == other.layout
            && self.frames == other.frames
            && self.iter().zip(other.iter()).all(|(a, b)| a == b)
    }
}

impl<S: Sample> fmt::Debug for Planes<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Planes")
            .field("frames", &self.frames())
            .field("layout", &self.layout)
            .finish()
    }
}

impl<'a, S: Sample> IntoIterator for Planes<'a, S> {
    type Item = Mono<'a, S>;
    type IntoIter = PlaneIter<'a, S>;

    #[inline]
    fn into_iter(self) -> PlaneIter<'a, S> {
        self.iter()
    }
}

impl<'s, 'a, S: Sample> From<PlanesMut<'s, 'a, S>> for Planes<'s, S> {
    #[inline]
    fn from(view: PlanesMut<'s, 'a, S>) -> Self {
        Self {
            planes: match view.planes {
                MutTable::Slices(p) => Table::Unique(&*p),
                MutTable::Vecs(p) => Table::Vecs(&*p),
            },
            layout: view.layout,
            start: view.start,
            frames: view.frames,
        }
    }
}

/// The channels of a [`Planes`], in order: each item is one channel's
/// [`Mono`] over the frames the view covers.
///
/// A named type so a struct can hold one; it knows its remaining length and
/// runs from either end.
#[derive(Clone)]
pub struct PlaneIter<'a, S: Sample = f32> {
    table: Table<'a, S>,
    channels: Range<usize>,
    start: usize,
    frames: usize,
}

impl<'a, S: Sample> PlaneIter<'a, S> {
    /// Channel `c` over the covered frames. `c` is within the table.
    #[inline]
    fn plane(&self, c: usize) -> Option<Mono<'a, S>> {
        let plane = self.table.get(c)?;
        Some(Mono(&plane[self.start..self.start + self.frames]))
    }
}

impl<S: Sample> fmt::Debug for PlaneIter<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaneIter")
            .field("remaining", &self.channels.len())
            .finish()
    }
}

impl<'a, S: Sample> Iterator for PlaneIter<'a, S> {
    type Item = Mono<'a, S>;

    #[inline]
    fn next(&mut self) -> Option<Mono<'a, S>> {
        let c = self.channels.next()?;
        self.plane(c)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.channels.size_hint()
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Mono<'a, S>> {
        let c = self.channels.nth(n)?;
        self.plane(c)
    }
}

impl<S: Sample> DoubleEndedIterator for PlaneIter<'_, S> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        let c = self.channels.next_back()?;
        self.plane(c)
    }
}

impl<S: Sample> ExactSizeIterator for PlaneIter<'_, S> {}

impl<S: Sample> FusedIterator for PlaneIter<'_, S> {}

/// The channel slices a [`PlanesMut`] writes: a table of slices, or the `Vec`s
/// a caller already holds.
enum MutTable<'s, 'a, S> {
    Slices(&'s mut [&'a mut [S]]),
    Vecs(&'s mut [Vec<S>]),
}

impl<'a, S> MutTable<'_, 'a, S> {
    #[inline]
    fn shared(&self) -> Table<'_, S> {
        match self {
            Self::Slices(p) => Table::Unique(p),
            Self::Vecs(p) => Table::Vecs(p),
        }
    }

    #[inline]
    fn reborrow(&mut self) -> MutTable<'_, 'a, S> {
        match self {
            Self::Slices(p) => MutTable::Slices(p),
            Self::Vecs(p) => MutTable::Vecs(p),
        }
    }

    #[inline]
    fn get_mut(&mut self, c: usize) -> Option<&mut [S]> {
        match self {
            Self::Slices(p) => p.get_mut(c).map(|p| &mut **p),
            Self::Vecs(p) => p.get_mut(c).map(Vec::as_mut_slice),
        }
    }
}

/// The write side of [`Planes`].
///
/// [`iter_mut`](Self::iter_mut) yields one [`MonoMut`] per channel, and the
/// borrow checker keeps them disjoint. [`slice_mut`](Self::slice_mut) narrows
/// every plane to a frame range, and [`as_planes`](Self::as_planes) reads the
/// current contents of every channel, for a kernel that writes in place.
///
/// Two lifetimes: `'s` borrows the table of channel slices and `'a` is the
/// slices' own, so a sub-view can borrow the table for less time than the
/// audio. There is no `split_at_mut`: two disjoint views of one table of
/// slices need a second table, which allocates.
///
/// ```
/// use tutti_buffers::PlanesMut;
/// use tutti_types::Samples;
///
/// let (mut l, mut r) = ([0.0f32; 4], [0.0f32; 4]);
/// let mut planes = [&mut l[..], &mut r[..]];
/// let mut out = PlanesMut::new(&mut planes)?;
/// for (c, mut plane) in out.slice_mut(Samples(2)..Samples(4)).iter_mut().enumerate() {
///     plane.samples_mut().fill(c as f32 + 1.0);
/// }
/// assert_eq!(out.as_planes().plane(1).samples(), &[0.0, 0.0, 2.0, 2.0]);
/// assert_eq!((l, r), ([0.0, 0.0, 1.0, 1.0], [0.0, 0.0, 2.0, 2.0]));
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
pub struct PlanesMut<'s, 'a, S: Sample = f32> {
    planes: MutTable<'s, 'a, S>,
    layout: ChannelLayout,
    start: usize,
    frames: usize,
}

impl<'s, 'a, S: Sample> PlanesMut<'s, 'a, S> {
    /// Wraps `planes`, one writable slice per channel.
    ///
    /// # Errors
    ///
    /// The same as [`Planes::new`].
    pub fn new(planes: &'s mut [&'a mut [S]]) -> Result<Self, BufferError> {
        let (layout, frames) = check(planes)?;
        Ok(Self {
            planes: MutTable::Slices(planes),
            layout,
            start: 0,
            frames: frames.get(),
        })
    }

    /// Wraps `planes`, one `Vec` per channel, each written whole. Nothing is
    /// copied and no `Vec` grows.
    ///
    /// # Errors
    ///
    /// The same as [`Planes::new`].
    pub fn from_vecs(planes: &'s mut [Vec<S>]) -> Result<Self, BufferError> {
        let (layout, frames) = check(planes)?;
        Ok(Self {
            planes: MutTable::Vecs(planes),
            layout,
            start: 0,
            frames: frames.get(),
        })
    }

    /// Returns the width: the number of planes.
    #[inline]
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the channel count.
    #[inline]
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.layout.count() as usize
    }

    /// Returns the frame count of every plane.
    #[inline]
    #[must_use]
    pub const fn frames(&self) -> Samples {
        Samples(self.frames)
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// Reads every channel as it is now.
    #[inline]
    #[must_use]
    pub fn as_planes(&self) -> Planes<'_, S> {
        Planes {
            planes: self.planes.shared(),
            layout: self.layout,
            start: self.start,
            frames: self.frames,
        }
    }

    /// Reborrows for a shorter lifetime, so the view can be passed on and used
    /// again.
    #[inline]
    #[must_use]
    pub fn reborrow(&mut self) -> PlanesMut<'_, 'a, S> {
        PlanesMut {
            planes: self.planes.reborrow(),
            layout: self.layout,
            start: self.start,
            frames: self.frames,
        }
    }

    /// Returns channel `c` read-only.
    ///
    /// # Panics
    ///
    /// If `c` is not below the width.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn plane(&self, c: usize) -> Mono<'_, S> {
        self.as_planes().plane(c)
    }

    /// Returns channel `c` read-only, or `None` past the last.
    #[inline]
    #[must_use]
    pub fn get_plane(&self, c: usize) -> Option<Mono<'_, S>> {
        self.as_planes().get_plane(c)
    }

    /// Returns channel `c` writable.
    ///
    /// # Panics
    ///
    /// If `c` is not below the width.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn plane_mut(&mut self, c: usize) -> MonoMut<'_, S> {
        let channels = self.channels();
        match self.get_plane_mut(c) {
            Some(plane) => plane,
            None => panic!("channel {c} is out of bounds for {channels} channels"),
        }
    }

    /// Returns channel `c` writable, or `None` past the last.
    #[inline]
    #[must_use]
    pub fn get_plane_mut(&mut self, c: usize) -> Option<MonoMut<'_, S>> {
        let (start, end) = (self.start, self.start + self.frames);
        let plane = self.planes.get_mut(c)?;
        Some(MonoMut(&mut plane[start..end]))
    }

    /// Iterates the channels in order, read-only.
    #[inline]
    #[must_use]
    pub fn iter(&self) -> PlaneIter<'_, S> {
        self.as_planes().iter()
    }

    /// Iterates the channels in order, each a writable view disjoint from the
    /// others.
    #[inline]
    #[must_use]
    pub fn iter_mut(&mut self) -> PlaneIterMut<'_, 'a, S> {
        let (start, end) = (self.start, self.start + self.frames);
        let inner = match &mut self.planes {
            MutTable::Slices(p) => Inner::Slices(p.iter_mut()),
            MutTable::Vecs(p) => Inner::Vecs(p.iter_mut()),
        };
        PlaneIterMut { inner, start, end }
    }

    /// Returns every plane narrowed to a frame range, read-only.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice(&self, frames: Range<Samples>) -> Planes<'_, S> {
        self.as_planes().slice(frames)
    }

    /// Returns every plane narrowed to a frame range, read-only, or `None` if
    /// the range is out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice(&self, frames: Range<Samples>) -> Option<Planes<'_, S>> {
        self.as_planes().get_slice(frames)
    }

    /// Returns every plane narrowed to a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds.
    #[inline]
    #[must_use]
    #[track_caller]
    pub fn slice_mut(&mut self, frames: Range<Samples>) -> PlanesMut<'_, 'a, S> {
        let r = bounds::slice(&frames, self.frames);
        self.run_mut(&r)
    }

    /// Returns every plane narrowed to a frame range, or `None` if the range is
    /// out of bounds.
    #[inline]
    #[must_use]
    pub fn get_slice_mut(&mut self, frames: Range<Samples>) -> Option<PlanesMut<'_, 'a, S>> {
        bounds::get(&frames, self.frames).map(|r| self.run_mut(&r))
    }

    /// The sub-view of an in-bounds frame range.
    #[inline]
    fn run_mut(&mut self, r: &Range<usize>) -> PlanesMut<'_, 'a, S> {
        PlanesMut {
            planes: self.planes.reborrow(),
            layout: self.layout,
            start: self.start + r.start,
            frames: r.end - r.start,
        }
    }
}

impl<S: Sample> fmt::Debug for PlanesMut<'_, '_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanesMut")
            .field("frames", &self.frames())
            .field("layout", &self.layout)
            .finish()
    }
}

/// The table's own iterator: slices of slices, or `Vec`s.
enum Inner<'x, 'a, S> {
    Slices(core::slice::IterMut<'x, &'a mut [S]>),
    Vecs(core::slice::IterMut<'x, Vec<S>>),
}

/// The channels of a [`PlanesMut`], in order: each item is one channel's
/// [`MonoMut`], disjoint from the others.
///
/// A named type so a struct can hold one; it knows its remaining length and
/// runs from either end.
pub struct PlaneIterMut<'x, 'a, S: Sample = f32> {
    inner: Inner<'x, 'a, S>,
    start: usize,
    end: usize,
}

impl<S: Sample> fmt::Debug for PlaneIterMut<'_, '_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaneIterMut")
            .field("remaining", &self.len())
            .finish()
    }
}

impl<'x, S: Sample> Iterator for PlaneIterMut<'x, '_, S> {
    type Item = MonoMut<'x, S>;

    #[inline]
    fn next(&mut self) -> Option<MonoMut<'x, S>> {
        let (start, end) = (self.start, self.end);
        match &mut self.inner {
            Inner::Slices(it) => it.next().map(|p| MonoMut(&mut p[start..end])),
            Inner::Vecs(it) => it.next().map(|p| MonoMut(&mut p[start..end])),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.inner {
            Inner::Slices(it) => it.size_hint(),
            Inner::Vecs(it) => it.size_hint(),
        }
    }
}

impl<S: Sample> DoubleEndedIterator for PlaneIterMut<'_, '_, S> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        let (start, end) = (self.start, self.end);
        match &mut self.inner {
            Inner::Slices(it) => it.next_back().map(|p| MonoMut(&mut p[start..end])),
            Inner::Vecs(it) => it.next_back().map(|p| MonoMut(&mut p[start..end])),
        }
    }
}

impl<S: Sample> ExactSizeIterator for PlaneIterMut<'_, '_, S> {}

impl<S: Sample> FusedIterator for PlaneIterMut<'_, '_, S> {}
