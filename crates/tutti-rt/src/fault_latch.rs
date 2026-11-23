//! [`FaultLatch`]: a flag a producer raises when it stops for good, with the
//! [`Cause`] it stopped for, read by a consumer that must see every item pushed
//! before the raise before it believes the stop.
//!
//! A live source (a capture callback) pushes frames into a ring and, when its
//! device goes away, raises a flag. The consumer pulls from the ring and asks
//! whether the source has failed. Asked in the wrong order the answer is wrong
//! silently: a consumer that finds the ring empty and *then* reads the flag can
//! see a flag raised after frames that landed between the two reads, and
//! report "failed, nothing left" with frames still in the ring: the take loses
//! its last block with no error. Asked in the right order the flag read can
//! only be stale in the safe direction.
//!
//! # The protocol
//!
//! - **Raise** ([`FaultLatch::raise`]): a `Release` compare-exchange from "not
//!   raised" to the cause's code. The producer pushes its items first, then
//!   raises. The first cause stays: a raise on a raised latch changes nothing,
//!   so a later failure never overwrites the one that stopped the producer.
//! - **Drain** ([`FaultLatch::drain`]): an `Acquire` load of the flag **first**,
//!   then the pop. The verdict is [`Drained::Failed`] only if the flag was
//!   raised *and* the pop found nothing. The pop that follows a raised flag
//!   sees every item pushed before the raise (the `Acquire` pairs with the
//!   `Release`), so an empty pop after a raised flag is a drained ring. A flag
//!   raised after the load is read on the next call; the pop may then have
//!   found an item or not, and either is right.
//!
//! There is no fence: it is one load and one read-modify-write on one word, a
//! release/acquire pair, not a store-load pair. The cause rides on that word
//! (`0` is not raised, any other value a [`Cause::code`]), so reading the cause
//! is the same `Acquire` load.
//!
//! The model in `tests/fault_latch_loom.rs` runs this code, with a loom ring
//! standing in for the one the items travel through.

use crate::sync::{AtomicU32, Ordering};
use core::num::NonZeroU32;

/// Why a producer stopped: a value that packs into a [`NonZeroU32`] and back,
/// so a [`FaultLatch`] carries it in its one atomic word. `0` is the latch's
/// "not raised"; every non-zero code is the cause's own, so a cause can pack a
/// kind and a payload (an errno) into it.
///
/// **The law:** `from_code(c.code()) == Some(c)` for every `c`. A cause that
/// breaks it reads back as no cause: [`FaultLatch::cause`] answers `None` and a
/// drain answers [`Drained::Empty`] while the latch is raised, a hidden failure.
pub trait Cause: Copy {
    /// The code this cause is stored as.
    fn code(self) -> NonZeroU32;
    /// The cause a code stands for; `None` for a code no cause of this type
    /// packs to.
    fn from_code(code: NonZeroU32) -> Option<Self>;
}

/// A latch that only says that the producer stopped: code `1`.
impl Cause for () {
    fn code(self) -> NonZeroU32 {
        NonZeroU32::MIN
    }

    fn from_code(_: NonZeroU32) -> Option<Self> {
        Some(())
    }
}

/// What one [`FaultLatch::drain`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained<T, C = ()> {
    /// The pop produced an item.
    Item(T),
    /// The pop found nothing and the latch was not raised: the producer is
    /// late, not gone.
    Empty,
    /// The latch was raised before the pop and the pop found nothing: the
    /// producer is gone, for this cause, and everything it pushed has been
    /// taken.
    Failed(C),
}

/// A flag that a producer raises when it stops for good, carrying the first
/// [`Cause`] it was raised with.
#[derive(Debug)]
pub struct FaultLatch<C: Cause = ()> {
    raised: AtomicU32,
    cause: core::marker::PhantomData<fn() -> C>,
}

impl<C: Cause> Default for FaultLatch<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Cause> FaultLatch<C> {
    /// A latch that is not raised.
    #[must_use]
    pub fn new() -> Self {
        Self {
            raised: AtomicU32::new(0),
            cause: core::marker::PhantomData,
        }
    }

    /// Raises the latch for `cause`: the producer will push nothing more. Push
    /// every item first; what is pushed after the raise may be missed by a
    /// drain that saw it. The first cause stays: on a raised latch this
    /// changes nothing to the cause, and every raiser's pushes are published by
    /// its own raise. One compare-exchange (and on a raised latch one
    /// value-preserving RMW); never waits.
    pub fn raise(&self, cause: C) {
        // `Release` on success pairs with the reader's `Acquire`. A raise that
        // finds the latch already raised keeps the first cause and still joins
        // the word's modification order with a value-preserving `Release`
        // RMW, so a reader whose load reads it sees this producer's pushes too.
        if self
            .raised
            .compare_exchange(0, cause.code().get(), Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            self.raised.fetch_or(0, Ordering::Release);
        }
    }

    /// Lowers the latch, for a producer that starts again.
    pub fn clear(&self) {
        self.raised.store(0, Ordering::Release);
    }

    /// The cause the latch was first raised with, or `None` while it is not
    /// raised (or holds a code `C` does not pack to). For a reader with no ring
    /// to drain; a consumer that pulls items uses [`drain`](Self::drain), which
    /// orders the read before its pop.
    #[must_use]
    pub fn cause(&self) -> Option<C> {
        NonZeroU32::new(self.raised.load(Ordering::Acquire)).and_then(C::from_code)
    }

    /// Whether the latch is raised.
    #[must_use]
    pub fn is_raised(&self) -> bool {
        self.raised.load(Ordering::Acquire) != 0
    }

    /// Reads the latch, **then** runs `pop`, and says what the pair means.
    ///
    /// See the module docs above for why the order is the whole point.
    pub fn drain<T>(&self, pop: impl FnOnce() -> Option<T>) -> Drained<T, C> {
        let code = self.raised.load(Ordering::Acquire);
        let cause = NonZeroU32::new(code).and_then(C::from_code);
        debug_assert!(
            code == 0 || cause.is_some(),
            "a raised latch holds code {code:#x}, which its Cause does not read back"
        );
        match pop() {
            Some(item) => Drained::Item(item),
            None => cause.map_or(Drained::Empty, Drained::Failed),
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{Cause, Drained, FaultLatch};
    use core::num::NonZeroU32;

    /// A cause with a payload packed into the code: a kind byte and a 24-bit
    /// number.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Stop {
        Died,
        WaitFailed(u32),
    }

    impl Cause for Stop {
        fn code(self) -> NonZeroU32 {
            let raw = match self {
                Stop::Died => 1 << 24,
                Stop::WaitFailed(errno) => (2 << 24) | (errno & 0x00FF_FFFF),
            };
            NonZeroU32::new(raw).expect("a kind byte above 0")
        }

        fn from_code(code: NonZeroU32) -> Option<Self> {
            let raw = code.get();
            match raw >> 24 {
                1 => Some(Stop::Died),
                2 => Some(Stop::WaitFailed(raw & 0x00FF_FFFF)),
                _ => None,
            }
        }
    }

    #[test]
    fn the_example_cause_keeps_the_law() {
        for c in [
            Stop::Died,
            Stop::WaitFailed(0),
            Stop::WaitFailed(0x00FF_FFFF),
            Stop::WaitFailed(32),
        ] {
            assert_eq!(Stop::from_code(c.code()), Some(c));
        }
        assert_eq!(<() as Cause>::from_code(().code()), Some(()));
    }

    #[test]
    fn the_first_cause_stays_until_cleared() {
        let latch = FaultLatch::<Stop>::new();
        assert_eq!(latch.cause(), None);
        assert!(!latch.is_raised());
        latch.raise(Stop::WaitFailed(32));
        latch.raise(Stop::Died);
        assert_eq!(
            latch.cause(),
            Some(Stop::WaitFailed(32)),
            "a later raise changes nothing"
        );
        assert!(latch.is_raised());
        latch.clear();
        assert_eq!(latch.cause(), None);
        latch.raise(Stop::Died);
        assert_eq!(
            latch.cause(),
            Some(Stop::Died),
            "a cleared latch takes a new cause"
        );
    }

    #[test]
    fn a_drained_raised_latch_fails_with_its_cause() {
        let latch = FaultLatch::<Stop>::new();
        assert_eq!(latch.drain(|| None::<u8>), Drained::Empty);
        latch.raise(Stop::WaitFailed(0x00AB_CDEF));
        assert_eq!(latch.drain(|| Some(1u8)), Drained::Item(1));
        assert_eq!(
            latch.drain(|| None::<u8>),
            Drained::Failed(Stop::WaitFailed(0x00AB_CDEF)),
            "the payload survives the word"
        );
    }

    #[test]
    fn a_unit_latch_reports_failed_with_unit() {
        let latch = FaultLatch::<()>::new();
        latch.raise(());
        assert_eq!(latch.drain(|| None::<u8>), Drained::Failed(()));
    }
}
