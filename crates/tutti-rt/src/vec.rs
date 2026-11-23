//! Owned, capped, slice-lending collection for the audio thread.

use core::mem::MaybeUninit;

/// A capped per-block collection that owns its storage and lends it out as
/// `&[T]`.
///
/// This is the "collect during the block, then hand the filled run to a
/// caller" shape, the most common one on the RT path.
///
/// A caller that needs the same buffer through `&self` (behind an `Arc`, in a
/// COM object) wraps it in an [`AudioThreadCell`](crate::AudioThreadCell).
///
/// # The cap is the point
///
/// `push` **refuses** at `N` and returns `false`; it never grows. A bare
/// `Vec` in this position grows instead, which means a `malloc`
/// inside the audio callback (and, if drained with `mem::take`, a `free` there
/// too). The storage is `N` inline slots, which have no heap path at all, so
/// the collection itself never touches the heap. (Its items may: `clear`
/// drops them where it runs, so an `RtVec<Box<_>>` cleared on the audio
/// thread frees there.)
///
/// # Overflow is visible, not silent
///
/// Dropping events is a real behaviour change, so it is reportable rather
/// than quiet: [`push`](Self::push) returns whether the item was stored,
/// [`extend`](Self::extend) returns how many it wrote, and
/// [`is_overflowed`](Self::is_overflowed) stays set for the whole block once
/// anything has been dropped. A caller that must not lose data checks
/// `is_overflowed()` after filling and reports upward — the audio thread still
/// does not allocate either way.
///
/// `clear` resets both the contents and the flag, so the flag always
/// describes the current block.
///
/// # Examples
///
/// ```
/// use tutti_rt::RtVec;
///
/// let mut due: RtVec<u32, 2> = RtVec::new();
/// assert!(due.push(10));
/// assert!(due.push(20));
/// assert!(!due.push(30)); // refused, never grown
/// assert_eq!(due.as_slice(), &[10, 20]);
/// assert!(due.is_overflowed());
///
/// due.clear();
/// assert!(due.is_empty() && !due.is_overflowed());
/// ```
pub struct RtVec<T, const N: usize> {
    /// The slots `..len` hold items; the rest are uninitialised.
    len: usize,
    overflowed: bool,
    slots: [MaybeUninit<T>; N],
}

impl<T, const N: usize> RtVec<T, N> {
    /// Creates an empty collection with `N` inline slots.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            len: 0,
            overflowed: false,
            slots: [const { MaybeUninit::uninit() }; N],
        }
    }

    /// Creates an empty collection in a box, built in place: no `RtVec` passes
    /// through the stack, in any build. For a large `N` built on a thread
    /// whose stack would not hold it.
    #[must_use]
    pub fn boxed() -> Box<Self> {
        let mut uninit = Box::<Self>::new_uninit();
        let at = uninit.as_mut_ptr();
        // SAFETY: `at` points at the box's allocation for one `Self`. The two
        // writes initialise `len` and `overflowed` through raw place
        // projections, which make no reference to the uninitialised value;
        // `slots` is an array of `MaybeUninit`, valid uninitialised. Every
        // field is then initialised, so the box holds a valid empty `Self`.
        unsafe {
            (&raw mut (*at).len).write(0);
            (&raw mut (*at).overflowed).write(false);
            uninit.assume_init()
        }
    }

    /// Drops all items and resets the overflow flag, keeping the storage.
    #[inline]
    pub fn clear(&mut self) {
        let items: *mut [T] = self.as_mut_slice();
        // The length goes to zero first, so a panicking item drop never leaves
        // a slot that drops twice; `drop_in_place` still drops the others.
        self.len = 0;
        self.overflowed = false;
        // SAFETY: `items` covered exactly the initialised slots `..len`, and
        // `len` is now 0, so nothing else reads or drops them.
        unsafe { core::ptr::drop_in_place(items) };
    }

    /// Appends one item if there is room.
    ///
    /// Returns `false` (dropping `v`, and latching
    /// [`is_overflowed`](Self::is_overflowed)) when already at `N`.
    #[inline]
    pub fn push(&mut self, v: T) -> bool {
        let Some(slot) = self.slots.get_mut(self.len) else {
            self.overflowed = true;
            return false;
        };
        slot.write(v);
        self.len += 1;
        true
    }

    /// Removes and returns the last item, or `None` when empty. Leaves the
    /// overflow flag as it is: a pop does not undo a refused push. Never
    /// frees storage.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        self.len = self.len.checked_sub(1)?;
        // SAFETY: the slot at the old `len - 1` was initialised, and lowering
        // `len` past it hands its item out exactly once.
        Some(unsafe { self.slots[self.len].assume_init_read() })
    }

    /// Latches [`is_overflowed`](Self::is_overflowed) without pushing: the caller
    /// refused a group of items as a whole (one that would have to be split
    /// to fit) and dropped it.
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

    /// Returns the filled run.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the slots `..len` are initialised, and `MaybeUninit<T>` has
        // `T`'s layout.
        unsafe { core::slice::from_raw_parts(self.slots.as_ptr().cast(), self.len) }
    }

    /// Returns the filled run mutably, for in-place fixups (sorting by frame
    /// offset, clamping) that do not change the length.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, through the unique borrow of `self`.
        unsafe { core::slice::from_raw_parts_mut(self.slots.as_mut_ptr().cast(), self.len) }
    }

    /// Returns whether anything has been dropped since the last
    /// [`clear`](Self::clear).
    #[inline]
    pub fn is_overflowed(&self) -> bool {
        self.overflowed
    }

    /// Returns how many items are in the filled run; never more than `N`.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether nothing has been pushed since the last
    /// [`clear`](Self::clear).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns whether the next `push` will be refused.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.len == N
    }

    /// Returns the room left before the cap.
    #[inline]
    pub fn remaining(&self) -> usize {
        N - self.len
    }

    /// Returns the cap, `N`, for code where `N` is not in scope.
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Iterates over the filled run.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, T> {
        self.as_slice().iter()
    }
}

impl<T, const N: usize> Drop for RtVec<T, N> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T, const N: usize> Default for RtVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> core::ops::Deref for RtVec<T, N> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a RtVec<T, N> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: core::fmt::Debug, const N: usize> core::fmt::Debug for RtVec<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RtVec")
            .field("items", &self.as_slice())
            .field("cap", &N)
            .field("overflowed", &self.is_overflowed())
            .finish()
    }
}

#[cfg(test)]
mod tests;
