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
    refused: usize,
}

impl<T> CappedVec<T> {
    /// An empty collection with room for `cap` items, allocated here.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
            refused: 0,
        }
    }

    /// Drops all items and resets the refusal count, keeping the storage.
    #[inline]
    pub fn clear(&mut self) {
        self.buf.clear();
        self.refused = 0;
    }

    /// Appends one item if there is room.
    ///
    /// Returns `false` (dropping `v`, and latching
    /// [`is_overflowed`](Self::is_overflowed)) when already at the cap.
    #[inline]
    pub fn push(&mut self, v: T) -> bool {
        if self.buf.len() == self.cap {
            self.refused = self.refused.saturating_add(1);
            return false;
        }
        // Below the cap, and the storage holds at least the cap: this push
        // writes into reserved room and never reallocates.
        self.buf.push(v);
        true
    }

    /// Appends one item if there is room, and hands it back when there is
    /// none: the name and the shape of `Vec::push_within_capacity`.
    ///
    /// A refused item is not dropped here, so a caller on the audio thread
    /// that must not free it can park or leak it. A refusal latches
    /// [`is_overflowed`](Self::is_overflowed) like [`push`](Self::push).
    ///
    /// # Errors
    ///
    /// `Err(value)` when the collection is already at the cap.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_rt::CappedVec;
    ///
    /// let mut owned: CappedVec<String> = CappedVec::with_capacity(1);
    /// assert!(owned.push_within_capacity("kept".to_owned()).is_ok());
    /// let refused = owned.push_within_capacity("refused".to_owned());
    /// assert_eq!(refused, Err("refused".to_owned())); // handed back, not dropped
    /// assert!(owned.is_overflowed());
    /// ```
    #[inline]
    pub fn push_within_capacity(&mut self, value: T) -> Result<(), T> {
        if self.buf.len() == self.cap {
            self.refused = self.refused.saturating_add(1);
            return Err(value);
        }
        // Below the cap: this push writes into reserved room.
        self.buf.push(value);
        Ok(())
    }

    /// Inserts `value` at `index`, shifting the items after it up one, and
    /// hands it back when there is no room: the shape of
    /// [`push_within_capacity`](Self::push_within_capacity). A refusal latches
    /// [`is_overflowed`](Self::is_overflowed). Never reallocates.
    ///
    /// # Errors
    ///
    /// `Err(value)` when the collection is already at the cap.
    ///
    /// # Panics
    ///
    /// If `index` is past [`len`](Self::len).
    #[inline]
    pub fn insert_within_capacity(&mut self, index: usize, value: T) -> Result<(), T> {
        assert!(
            index <= self.buf.len(),
            "insert index {index} past len {}",
            self.buf.len()
        );
        if self.buf.len() == self.cap {
            self.refused = self.refused.saturating_add(1);
            return Err(value);
        }
        // Below the cap: the insert writes into reserved room.
        self.buf.insert(index, value);
        Ok(())
    }

    /// Keeps the items `keep` accepts, in order, visiting each once and
    /// letting `keep` change it; the rest are dropped here. Never frees the
    /// storage and leaves the overflow flag as it is.
    #[inline]
    pub fn retain_mut(&mut self, keep: impl FnMut(&mut T) -> bool) {
        self.buf.retain_mut(keep);
    }

    /// Removes the items in `range` and yields them in order; the rest shift
    /// down. Dropping the iterator removes what was not yielded. Never frees
    /// the storage and leaves the overflow flag as it is.
    ///
    /// # Panics
    ///
    /// If the range starts after its end or ends past [`len`](Self::len).
    #[inline]
    pub fn drain<R: core::ops::RangeBounds<usize>>(&mut self, range: R) -> std::vec::Drain<'_, T> {
        self.buf.drain(range)
    }

    /// Removes and returns the last item, or `None` when empty. Leaves the
    /// overflow flag as it is: a pop does not undo a refused push. Never
    /// frees storage.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        self.buf.pop()
    }

    /// Keeps the first `len` items and drops the rest here; a `len` at or past
    /// [`len`](Self::len) does nothing. Never frees the storage and leaves the
    /// overflow flag as it is.
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.buf.truncate(len);
    }

    /// Removes and returns the item at `index`, moving the last item into its
    /// place: O(1), and the order is not kept. Never frees the storage and
    /// leaves the overflow flag as it is.
    ///
    /// # Panics
    ///
    /// If `index` is not below [`len`](Self::len).
    #[inline]
    pub fn swap_remove(&mut self, index: usize) -> T {
        self.buf.swap_remove(index)
    }

    /// Removes and returns the item at `index`, shifting the later items down:
    /// the order is kept, at O(len). Never frees the storage and leaves the
    /// overflow flag as it is.
    ///
    /// # Panics
    ///
    /// If `index` is not below [`len`](Self::len).
    #[inline]
    pub fn remove(&mut self, index: usize) -> T {
        self.buf.remove(index)
    }

    /// Counts one refusal in [`refused`](Self::refused) without pushing: the
    /// caller refused a group of items as a whole and dropped it.
    #[inline]
    pub fn mark_overflowed(&mut self) {
        self.refused = self.refused.saturating_add(1);
    }

    /// Appends from an iterator, stopping at the cap.
    ///
    /// Returns how many items were written; a short return means the rest were
    /// dropped. [`refused`](Self::refused) counts the first refused item and
    /// at least the items the rest of the iterator's size hint reports: exact
    /// for a slice or an `ExactSizeIterator`, a lower bound otherwise. It
    /// never iterates past the first refusal, so it returns for an endless
    /// iterator.
    #[inline]
    pub fn extend(&mut self, it: impl IntoIterator<Item = T>) -> usize {
        let mut written = 0;
        let mut it = it.into_iter();
        while let Some(v) = it.next() {
            if !self.push(v) {
                // `push` counted the item it refused; the rest are counted
                // by their size hint, without being iterated.
                self.refused = self.refused.saturating_add(it.size_hint().0);
                break;
            }
            written += 1;
        }
        written
    }

    /// Appends a copy of `src`, as much as fits, with one bulk copy.
    ///
    /// Returns how many items were written; a short return means the rest
    /// were dropped, and [`refused`](Self::refused) counts them, as
    /// [`extend`](Self::extend) does for a slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_rt::CappedVec;
    ///
    /// let mut v: CappedVec<u32> = CappedVec::with_capacity(3);
    /// assert_eq!(v.extend_from_slice(&[1, 2]), 2);
    /// assert_eq!(v.extend_from_slice(&[3, 4]), 1);
    /// assert_eq!(v.as_slice(), &[1, 2, 3]);
    /// assert_eq!(v.refused(), 1);
    /// ```
    #[inline]
    pub fn extend_from_slice(&mut self, src: &[T]) -> usize
    where
        T: Copy,
    {
        let take = src.len().min(self.cap - self.buf.len());
        // Cut to the room, so the storage reserved at construction is enough
        // and this never reallocates.
        self.buf.extend_from_slice(&src[..take]);
        if take < src.len() {
            self.refused = self.refused.saturating_add(src.len() - take);
        }
        take
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

    /// Whether anything has been dropped since the last [`clear`](Self::clear):
    /// [`refused`](Self::refused) is not zero.
    #[inline]
    #[must_use]
    pub fn is_overflowed(&self) -> bool {
        self.refused > 0
    }

    /// How many items were refused since the last [`clear`](Self::clear): one
    /// for each refused [`push`](Self::push) or
    /// [`push_within_capacity`](Self::push_within_capacity), each item an
    /// [`extend`](Self::extend) did not take, and each
    /// [`mark_overflowed`](Self::mark_overflowed). Saturates at `usize::MAX`.
    ///
    /// ```
    /// use tutti_rt::CappedVec;
    ///
    /// let mut v: CappedVec<u32> = CappedVec::with_capacity(1);
    /// v.extend([1, 2, 3]);
    /// assert_eq!(v.refused(), 2);
    /// ```
    #[inline]
    #[must_use]
    pub fn refused(&self) -> usize {
        self.refused
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

impl<T> Default for CappedVec<T> {
    /// A collection with a cap of zero, which allocates nothing and refuses
    /// every push.
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl<T> core::ops::Deref for CappedVec<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> core::ops::DerefMut for CappedVec<T> {
    /// The filled run, for in-place edits: a slice cannot grow, so the cap holds.
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
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
            .field("refused", &self.refused)
            .finish()
    }
}

#[cfg(test)]
mod tests;
