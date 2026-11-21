//! [`DelayRing`]: the one ring every delay in the crate stores its history in.

use tutti_buffers::Float;
use tutti_units::ReadPosition;

/// A ring of `len` frames with a cursor: `put` writes the slot at the cursor, `advance` moves
/// it to the next, and a read counts back from it.
///
/// The slot at the cursor is the frame being written: `back(0)` and `back(len)` read what it
/// holds before the `put`, and `back(k)` is the frame `k` slots before it. A read after
/// `advance` therefore reaches the frame just put with `back(1)`. The length is fixed at
/// construction, so no call allocates after it.
#[derive(Clone, Debug)]
pub(crate) struct DelayRing<F> {
    buf: Vec<F>,
    w: usize,
}

impl<F: Float> DelayRing<F> {
    /// A silent ring of `len` frames, at least one. Allocates.
    #[must_use]
    pub(crate) fn new(len: usize) -> Self {
        assert!(len >= 1, "a ring holds at least one frame");
        Self {
            buf: vec![F::ZERO; len],
            w: 0,
        }
    }

    /// The frames the ring holds.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// Silences the ring and returns the cursor to slot 0.
    pub(crate) fn clear(&mut self) {
        self.buf.fill(F::ZERO);
        self.w = 0;
    }

    /// Writes `x` at the cursor.
    #[inline]
    pub(crate) fn put(&mut self, x: F) {
        self.buf[self.w] = x;
    }

    /// Moves the cursor to the next slot, wrapping at the length.
    #[inline]
    pub(crate) fn advance(&mut self) {
        self.w += 1;
        if self.w == self.buf.len() {
            self.w = 0;
        }
    }

    /// `put` then `advance`.
    #[inline]
    pub(crate) fn push(&mut self, x: F) {
        self.put(x);
        self.advance();
    }

    /// The slot at the cursor: the frame `len` slots back, the oldest the ring holds.
    #[inline]
    pub(crate) fn oldest(&self) -> F {
        self.buf[self.w]
    }

    /// The frame `k` slots before the cursor, for `k` up to the length.
    #[inline]
    pub(crate) fn back(&self, k: usize) -> F {
        let len = self.buf.len();
        debug_assert!(k <= len, "{k} slots back in a ring of {len}");
        // `w < len` and `k <= len`, so the sum is below `2 * len`: one subtract wraps it.
        let i = self.w + len - k;
        self.buf[if i >= len { i - len } else { i }]
    }

    /// Adds `x` to the frame `k` slots before the cursor (`back(k)`'s slot), for `k` in
    /// `1..=len`: a signal injected `k` frames into the ring's path.
    #[inline]
    pub(crate) fn add_back(&mut self, k: usize, x: F) {
        let len = self.buf.len();
        debug_assert!((1..=len).contains(&k), "{k} slots back in a ring of {len}");
        let i = self.w + len - k;
        let i = if i >= len { i - len } else { i };
        self.buf[i] += x;
    }

    /// The frame `delay` slots before the cursor, linearly interpolated between the two whole
    /// slots around it: `delay` is at least 0 and its next whole slot is at most the length.
    ///
    /// The position's whole frames index the ring and its fraction, the top mantissa bits as
    /// `F`, weights the blend, so a delay of 10 s at 192 kHz keeps a fraction to `F`'s own
    /// resolution however many frames back it is.
    #[inline]
    pub(crate) fn at(&self, delay: ReadPosition) -> F {
        let (whole, frac) = F::split_position(delay);
        let whole = whole.get();
        let a = self.back(whole);
        if frac == F::ZERO {
            return a;
        }
        crate::lerp(a, self.back(whole + 1), frac)
    }
}

#[cfg(test)]
mod tests;
