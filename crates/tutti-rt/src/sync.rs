//! The synchronisation primitives the crate's lock-free types are written
//! against: `loom`'s under `--cfg loom`, `std`'s otherwise. This is the one
//! place the choice is made; every file imports from here, so a model checks
//! the code that ships.

#[cfg(loom)]
mod imp {
    pub(crate) use loom::cell::UnsafeCell;
    pub(crate) use loom::hint::spin_loop;
    pub(crate) use loom::sync::atomic::{
        AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
    };
    pub(crate) use loom::sync::{Arc, Mutex, MutexGuard};
    pub(crate) use loom::thread::{Thread, ThreadId, current, park, yield_now};
}

#[cfg(not(loom))]
mod imp {
    pub(crate) use std::hint::spin_loop;
    pub(crate) use std::sync::atomic::{
        AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
    };
    pub(crate) use std::sync::{Arc, Mutex, MutexGuard};
    pub(crate) use std::thread::{Thread, ThreadId, current, park, yield_now};

    /// `loom`'s `UnsafeCell` API over `std`'s, so a type is written once: an
    /// access is a closure over the raw pointer.
    #[derive(Debug)]
    pub(crate) struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self(std::cell::UnsafeCell::new(value))
        }

        pub(crate) fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
            f(self.0.get())
        }

        pub(crate) fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            f(self.0.get())
        }
    }
}

pub(crate) use imp::*;
