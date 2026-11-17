//! Owned collection with a cap fixed at construction, for the audio thread.

/// A per-block collection whose cap is chosen when it is built, off the audio
/// thread, and never grows: the counterpart of [`RtVec`](crate::RtVec) for a
/// cap known only at run time (a parameter count times a block length, known
/// at activation).
///
/// [`with_capacity`](Self::with_capacity) allocates the whole cap once, where
/// it is called. After that, [`push`](Self::push) **refuses** at the cap and
/// returns `false`, latching [`is_overflowed`](Self::is_overflowed) until the
/// next [`clear`](Self::clear); nothing reallocates, so filling and clearing on
/// the audio thread never touch the heap. (Items may: `clear` drops them where
/// it runs.) Dropping the collection frees its storage, so it is dropped on
/// the control side.
///
/// # Examples
///
/// ```
/// use tutti_rt::CappedVec;
///
/// let mut points: CappedVec<u32> = CappedVec::with_capacity(2);
/// assert!(points.push(10));
/// assert!(points.push(20));
/// assert!(!points.push(30)); // refused, never grown
/// assert_eq!(points.as_slice(), &[10, 20]);
/// assert!(points.is_overflowed());
///
/// points.clear();
/// assert!(points.is_empty() && !points.is_overflowed());
/// assert_eq!(points.capacity(), 2);
/// ```
pub struct CappedVec<T> {
    buf: Vec<T>,
    cap: usize,
    overflowed: bool,
}

impl<T> CappedVec<T> {
    /// An empty collection with room for `cap` items, allocated here.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
            overflowed: false,
        }
    }

    /// Drops all items and resets the overflow flag, keeping the storage.
    #[inline]
    pub fn clear(&mut self) {
        self.buf.clear();
        self.overflowed = false;
    }

    /// Appends one item if there is room.
    ///
    /// Returns `false` (dropping `v`, and latching
    /// [`is_overflowed`](Self::is_overflowed)) when already at the cap.
    #[inline]
    pub fn push(&mut self, v: T) -> bool {
        if self.buf.len() == self.cap {
            self.overflowed = true;
            return false;
        }
        // Below the cap, and the storage holds at least the cap: this push
        // writes into reserved room and never reallocates.
        self.buf.push(v);
        true
    }

    /// Removes and returns the last item, or `None` when empty. Leaves the
    /// overflow flag as it is: a pop does not undo a refused push. Never
    /// frees storage.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        self.buf.pop()
    }

    /// Removes every item and yields it in order, keeping the storage, and
    /// resets the overflow flag. Each item drops where the iterator is, so a
    /// caller draining on the audio thread holds only items free to drop
    /// there.
    pub fn drain(&mut self) -> std::vec::Drain<'_, T> {
        self.overflowed = false;
        self.buf.drain(..)
    }

    /// Latches [`is_overflowed`](Self::is_overflowed) without pushing: the
    /// caller refused a group of items as a whole and dropped it.
    #[inline]
    pub fn mark_overflowed(&mut self) {
        self.overflowed = true;
    }

    /// Appends from an iterator, stopping at the cap.
    ///
    /// Returns how many items were written; a short return means the rest were
    /// dropped.
    #[inline]
    pub fn extend(&mut self, it: impl IntoIterator<Item = T>) -> usize {
        let mut written = 0;
        for v in it {
            if !self.push(v) {
                break;
            }
            written += 1;
        }
        written
    }

    /// The filled run.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.buf
    }

    /// The filled run mutably, for in-place fixups (sorting by frame offset,
    /// clamping) that do not change the length.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.buf
    }

    /// Whether anything has been dropped since the last [`clear`](Self::clear).
    #[inline]
    #[must_use]
    pub fn is_overflowed(&self) -> bool {
        self.overflowed
    }

    /// How many items are in the filled run; never more than the cap.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been pushed since the last [`clear`](Self::clear).
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Whether the next `push` is refused.
    #[inline]
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.buf.len() == self.cap
    }

    /// The room left before the cap.
    #[inline]
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.cap - self.buf.len()
    }

    /// The cap chosen at construction.
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.cap
    }

    /// Iterates over the filled run.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.buf.iter()
    }
}

impl<T> core::ops::Deref for CappedVec<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<'a, T> IntoIterator for &'a CappedVec<T> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.buf.iter()
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for CappedVec<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CappedVec")
            .field("items", &self.as_slice())
            .field("cap", &self.cap)
            .field("overflowed", &self.overflowed)
            .finish()
    }
}

#[cfg(test)]
mod tests;
