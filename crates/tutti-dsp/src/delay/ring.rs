//! [`DelayRing`]: the one `f32` ring every delay in the crate stores its history in.

/// A ring of `len` frames with a cursor: `put` writes the slot at the cursor, `advance` moves
/// it to the next, and a read counts back from it.
///
/// The slot at the cursor is the frame being written: `back(0)` and `back(len)` read what it
/// holds before the `put`, and `back(k)` is the frame `k` slots before it. A read after
/// `advance` therefore reaches the frame just put with `back(1)`. The length is fixed at
/// construction, so no call allocates after it.
#[derive(Clone, Debug)]
pub(crate) struct DelayRing {
    buf: Vec<f32>,
    w: usize,
}

impl DelayRing {
    /// A silent ring of `len` frames, at least one. Allocates.
    #[must_use]
    pub(crate) fn new(len: usize) -> Self {
        assert!(len >= 1, "a ring holds at least one frame");
        Self {
            buf: vec![0.0; len],
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
        self.buf.fill(0.0);
        self.w = 0;
    }

    /// Writes `x` at the cursor.
    #[inline]
    pub(crate) fn put(&mut self, x: f32) {
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
    pub(crate) fn push(&mut self, x: f32) {
        self.put(x);
        self.advance();
    }

    /// The slot at the cursor: the frame `len` slots back, the oldest the ring holds.
    #[inline]
    pub(crate) fn oldest(&self) -> f32 {
        self.buf[self.w]
    }

    /// The frame `k` slots before the cursor, for `k` up to the length.
    #[inline]
    pub(crate) fn back(&self, k: usize) -> f32 {
        let len = self.buf.len();
        debug_assert!(k <= len, "{k} slots back in a ring of {len}");
        // `w < len` and `k <= len`, so the sum is below `2 * len`: one subtract wraps it.
        let i = self.w + len - k;
        self.buf[if i >= len { i - len } else { i }]
    }

    /// The frame `delay` slots before the cursor, linearly interpolated between the two whole
    /// slots around it: `delay` is at least 0 and its next whole slot is at most the length.
    #[inline]
    pub(crate) fn at(&self, delay: f32) -> f32 {
        let whole = delay as usize;
        let frac = delay - whole as f32;
        let a = self.back(whole);
        if frac == 0.0 {
            return a;
        }
        crate::lerp(a, self.back(whole + 1), frac)
    }
}

#[cfg(test)]
mod tests;
