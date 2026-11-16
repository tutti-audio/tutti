//! [`RequestWord`]: what a callee raises on any thread, a control thread takes.
//!
//! A plugin calls its host from whatever thread it likes (`request_restart`,
//! `latency.changed`, `restartComponent`). The host's callback must do nothing
//! but record that, so each kind of call is one bit of one atomic word: raising
//! is a single `fetch_or`, wait-free, and never allocates or locks. The control
//! thread takes the whole word at its idle pass.
//!
//! # The protocol
//!
//! - **Raise:** `fetch_or(bit, Release)`. Raising a bit already set changes
//!   nothing: a request is a level, not a count.
//! - **Take:** an `Acquire` load, and only when it reads a non-zero word a
//!   `swap(0, Acquire)`, so the common case (no request) is one load and no
//!   read-modify-write. Every bit raised before the swap in the word's
//!   modification order is returned by this take or an earlier one; a raise
//!   after it, or after a load that read zero, stays for the next. No bit is
//!   lost and none is returned twice, because the swap and the `fetch_or` are
//!   both read-modify-writes on one location.
//!
//! The `Release`/`Acquire` pair orders what the raiser wrote before raising
//! (the plugin's new state) before what the taker reads after taking. There is
//! no store-then-load pair on either side, so no `SeqCst` fence is involved.
//!
//! The model in `tests/request_word_loom.rs` runs this code.

use sync::{AtomicU32, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU32, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU32, Ordering};
}

/// Bits in a [`RequestWord`].
pub(crate) const REQUEST_BITS: u8 = 32;

tutti_types::ranged! {
    /// One kind of request: a position in a [`RequestWord`], `0..32`.
    #[derive(Eq, Hash)]
    RequestBit(u8), RequestBitOutOfRange, "a request bit below 32", |raw| raw < REQUEST_BITS
}

impl RequestBit {
    const fn mask(self) -> u32 {
        1 << self.0
    }
}

/// The bits one [`RequestWord::take`] returned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestBits(u32);

impl RequestBits {
    /// No request.
    pub const NONE: Self = Self(0);

    /// Whether `bit` is set.
    #[must_use]
    pub const fn contains(self, bit: RequestBit) -> bool {
        self.0 & bit.mask() != 0
    }

    /// Whether no bit is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The set bits, lowest first.
    pub fn iter(self) -> impl Iterator<Item = RequestBit> {
        let mut rest = self.0;
        std::iter::from_fn(move || {
            if rest == 0 {
                return None;
            }
            let index = rest.trailing_zeros() as u8;
            rest &= rest - 1;
            Some(RequestBit(index))
        })
    }
}

/// Up to 32 kinds of request, raised on any thread and taken on one.
///
/// ```
/// use tutti_rt::{RequestBit, RequestWord};
///
/// const RESTART: RequestBit = RequestBit::of(0);
/// let word = RequestWord::new();
/// word.raise(RESTART);
/// word.raise(RESTART);
/// assert!(word.take().contains(RESTART));
/// assert!(word.take().is_empty(), "taken once");
/// ```
#[derive(Debug, Default)]
pub struct RequestWord(AtomicU32);

impl RequestWord {
    /// An empty word.
    #[must_use]
    pub fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// Records `bit`. Any thread; wait-free; never allocates or locks.
    pub fn raise(&self, bit: RequestBit) {
        self.0.fetch_or(bit.mask(), Ordering::Release);
    }

    /// Returns every bit raised since the last take, and clears them.
    ///
    /// The one consuming thread's call: two takers would split the bits
    /// between them.
    #[must_use = "the bits are cleared; dropping them drops the requests"]
    pub fn take(&self) -> RequestBits {
        if self.0.load(Ordering::Acquire) == 0 {
            return RequestBits::NONE;
        }
        RequestBits(self.0.swap(0, Ordering::Acquire))
    }
}
