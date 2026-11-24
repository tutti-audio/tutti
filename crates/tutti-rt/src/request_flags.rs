//! [`AtomicRequestFlags`]: what a callee raises on any thread, a control thread takes.
//!
//! A plugin calls its host from whatever thread it likes (`request_restart`,
//! `latency.changed`, `restartComponent`). The host's callback must do nothing
//! but record that, so each kind of call is one flag of one atomic set: raising
//! is a single `fetch_or`, wait-free, and never allocates or locks. The control
//! thread takes the whole set at its idle pass.
//!
//! # The protocol
//!
//! - **Raise:** `fetch_or(flag, Release)`. Raising a flag already set changes
//!   nothing: a request is a level, not a count.
//! - **Take:** an `Acquire` load, and only when it reads a non-empty set a
//!   `swap(0, Acquire)`, so the common case (no request) is one load and no
//!   read-modify-write. Every flag raised before the swap in the set's
//!   modification order is returned by this take or an earlier one; a raise
//!   after it, or after a load that read zero, stays for the next. No flag is
//!   lost and none is returned twice, because the swap and the `fetch_or` are
//!   both read-modify-writes on one location.
//!
//! The `Release`/`Acquire` pair orders what the raiser wrote before raising
//! (the plugin's new state) before what the taker reads after taking. There is
//! no store-then-load pair on either side, so no `SeqCst` fence is involved.
//!
//! The model in `tests/request_flags_loom.rs` runs this code.

use crate::sync::{AtomicU32, Ordering};

/// Flags in an [`AtomicRequestFlags`].
pub(crate) const REQUEST_FLAGS: u8 = 32;

tutti_units::ranged! {
    /// One kind of request: a position in an [`AtomicRequestFlags`], `0..32`.
    #[derive(Eq, Hash)]
    RequestFlag(u8), RequestFlagOutOfRange, "a request flag below 32", |raw| raw < REQUEST_FLAGS
}

impl RequestFlag {
    const fn mask(self) -> u32 {
        1 << self.0
    }
}

/// The flags one [`AtomicRequestFlags::take`] returned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestFlags(u32);

impl RequestFlags {
    /// No request.
    pub const NONE: Self = Self(0);

    /// Whether `flag` is set.
    #[must_use]
    pub const fn contains(self, flag: RequestFlag) -> bool {
        self.0 & flag.mask() != 0
    }

    /// Whether no flag is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Up to 32 kinds of request, raised on any thread and taken on one.
///
/// ```
/// use tutti_rt::{RequestFlag, AtomicRequestFlags};
///
/// const RESTART: RequestFlag = RequestFlag::of(0);
/// let flags = AtomicRequestFlags::new();
/// flags.raise(RESTART);
/// flags.raise(RESTART);
/// assert!(flags.take().contains(RESTART));
/// assert!(flags.take().is_empty(), "taken once");
/// ```
#[derive(Debug, Default)]
pub struct AtomicRequestFlags(AtomicU32);

impl AtomicRequestFlags {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// Records `flag`. Any thread; wait-free; never allocates or locks.
    pub fn raise(&self, flag: RequestFlag) {
        self.0.fetch_or(flag.mask(), Ordering::Release);
    }

    /// Returns every flag raised since the last take, and clears them.
    ///
    /// The one consuming thread's call: two takers would split the flags
    /// between them. The swap needs no `Release`: a take publishes nothing.
    #[must_use = "the flags are cleared; dropping them drops the requests"]
    pub fn take(&self) -> RequestFlags {
        if self.0.load(Ordering::Acquire) == 0 {
            return RequestFlags::NONE;
        }
        RequestFlags(self.0.swap(0, Ordering::Acquire))
    }
}
