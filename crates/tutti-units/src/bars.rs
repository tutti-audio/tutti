//! [`BarNumber`] and [`BarCount`]: a bar's position on the timeline and a
//! number of bars, an affine pair of integer quantities like `Beat` and
//! `BeatDuration`.

unit! {
    /// A bar's position on the timeline.
    ///
    /// **1-based**: bar 1 is the first bar, which is what every display and every
    /// musician means. Bar 0 and below are legal and describe pre-roll — a count-in
    /// sits at negative beats.
    ///
    /// CLAP's `bar_number` is **0-based** ("bar at song pos 0 has the number 0",
    /// `events.h`), so a host converts at that edge with [`index`](Self::index).
    ///
    /// A position, not a count: `BarNumber + BarNumber` has no meaning without an
    /// origin, exactly as `Beat + Beat` does not. Subtracting two gives a
    /// [`BarCount`].
    pub struct BarNumber(pub i64): Display, Affine(BarCount);
}

unit! {
    /// A number of bars — the displacement between two [`BarNumber`]s.
    pub struct BarCount(pub i64): Display, Additive, Signed;
}

// Both fields are public: a bar number has no range invariant for a
// constructor to enforce, but `new`/`get` exist anyway, because
// every other unit newtype in this crate has them (`Samples`, `Beat`, `Bpm`, …)
// and a host reaching for the familiar spelling should find it.

impl BarNumber {
    /// The first bar of the timeline.
    pub const FIRST: Self = Self(1);

    /// Wraps a **1-based** bar number. For a 0-based index use
    /// [`from_index`](Self::from_index).
    #[inline]
    #[must_use]
    pub const fn new(n: i64) -> Self {
        Self(n)
    }

    /// Returns the 1-based bar number as displayed. Signed, so a pickup bar before bar
    /// 1 is representable.
    #[inline]
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }

    /// Returns how many whole bars precede this one — `BarNumber::new(1)` yields `0`.
    ///
    /// The 1-based/0-based conversion, in one place, so callers indexing from a
    /// bar number do not each re-derive the off-by-one.
    #[inline]
    #[must_use]
    pub const fn index(self) -> i64 {
        self.0 - 1
    }

    /// Creates a bar number from a 0-based index, the inverse of
    /// [`index`](Self::index).
    #[inline]
    #[must_use]
    pub const fn from_index(index: i64) -> Self {
        Self(index + 1)
    }
}

impl BarCount {
    /// Wraps a signed displacement in bars. Negative counts run backwards.
    #[inline]
    #[must_use]
    pub const fn new(n: i64) -> Self {
        Self(n)
    }

    /// Returns the raw displacement in bars.
    #[inline]
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

// `BarNumber` and `BarCount` are an affine pair, `Affine(BarCount)`. The set is
// complete rather than trimmed to current callers: a half-implemented affine
// pair is a worse API than an unused operator.
//
// Deliberately absent: `BarNumber + BarNumber`, which `Affine` omits for the
// same reason it omits `Beat + Beat` — adding two positions needs an origin.
//
// `BarCount` composes with itself and negates, the same algebra `Additive` and
// `Signed` give `BeatDuration`.

// Scaling stays hand-written. `Scalable` would also generate `Div`, which
// truncates on `i64` — and a fraction of a bar is a `BeatDuration`, not a
// `BarCount`. The omission is the point, so the ability must not be used here.
impl core::ops::Mul<i64> for BarCount {
    type Output = Self;
    #[inline]
    fn mul(self, k: i64) -> Self {
        Self(self.0 * k)
    }
}

impl core::ops::Mul<BarCount> for i64 {
    type Output = BarCount;
    #[inline]
    fn mul(self, count: BarCount) -> BarCount {
        BarCount(self * count.0)
    }
}

// A `BarNumber` is built from an `i64` with `new` and read with `get`: `unit!`
// emits no `From`. The plugin ABIs that carry bar numbers as `i32` get the
// impl below, written out because it is a widening the ABI hands over by
// construction.
impl From<i32> for BarNumber {
    #[inline]
    fn from(n: i32) -> Self {
        Self(n as i64)
    }
}

impl From<BarNumber> for i32 {
    #[inline]
    fn from(b: BarNumber) -> Self {
        b.0 as i32
    }
}
