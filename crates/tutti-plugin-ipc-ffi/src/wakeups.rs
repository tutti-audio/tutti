//! The vocabulary of a wait on descriptors: which, for what, and what is ready.
//!
//! A plugin's editor asks its host to call back when a file descriptor is ready
//! (CLAP's `posix-fd-support`, VST3's `Linux::IRunLoop`). These are the typed
//! values of that ask and of the answer; the unix `poll_wait` and `poll_now`
//! take and return them, and no raw bits leave the crate.

/// A plugin file descriptor registration, as the format names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FdId(i32);

impl FdId {
    /// The registration of the descriptor numbered `raw`; `None` for a
    /// negative number, which names no descriptor (`-1` is the format's "no
    /// descriptor", and borrowing it is a fault).
    #[must_use]
    pub const fn new(raw: i32) -> Option<Self> {
        if raw < 0 { None } else { Some(Self(raw)) }
    }

    /// The descriptor's number.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// Which events on a descriptor a plugin asked to hear about, or which of
/// them are ready.
///
/// A set of three: readable, writable, in an error state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interest(u8);

impl Interest {
    /// No event.
    pub const NONE: Self = Self(0);
    /// The descriptor is readable without blocking.
    pub const READ: Self = Self(1 << 0);
    /// The descriptor is writable without blocking.
    pub const WRITE: Self = Self(1 << 1);
    /// The descriptor is in an error state (a hung-up or invalid descriptor
    /// included).
    pub const ERROR: Self = Self(1 << 2);

    /// The events of both.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The events `self` and `other` share.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Whether every event of `other` is in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no event is in the set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// A descriptor a plugin asked the host to wait on, and for what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    /// The descriptor, as the format names it.
    pub fd: FdId,
    /// The events to wait for.
    pub interest: Interest,
}

/// A watched descriptor that is ready, and the events that are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready {
    /// The descriptor.
    pub fd: FdId,
    /// The events ready, among those asked for.
    pub events: Interest,
}

#[cfg(test)]
mod tests {
    use super::FdId;

    /// A negative number names no descriptor: `-1` would fault in a borrow.
    #[test]
    fn an_fd_id_refuses_every_negative_number() {
        assert!(FdId::new(-1).is_none());
        assert!(FdId::new(i32::MIN).is_none());
        assert_eq!(FdId::new(0).map(FdId::get), Some(0));
        assert_eq!(FdId::new(i32::MAX).map(FdId::get), Some(i32::MAX));
    }

    use super::*;

    /// **Interest is a set of three events** with the set algebra.
    #[test]
    fn interest_is_a_set_of_three_events() {
        let rw = Interest::READ.union(Interest::WRITE);
        assert!(rw.contains(Interest::READ) && rw.contains(Interest::WRITE));
        assert!(!rw.contains(Interest::ERROR));
        assert_eq!(rw.intersection(Interest::WRITE), Interest::WRITE);
        assert!(Interest::NONE.is_empty() && !rw.is_empty());
    }
}
