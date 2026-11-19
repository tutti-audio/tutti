//! [`Exclusive`]: a value a `Sync` owner holds that is reached only through `&mut`.
//!
//! The shape and names are std's `core::sync::Exclusive` (unstable). A value that is
//! `Send` but not `Sync` (a ring's reader, a parked handle) can sit in a struct that must
//! be `Sync` without a lock: a shared `&Exclusive<T>` gives no access to the `T`, so
//! sharing it shares nothing.

/// A `T` reachable only through `&mut Exclusive<T>` or by value, so `Exclusive<T>` is
/// `Sync` for any `T: Send`.
///
/// It replaces a mutex that would only ever be taken through `&mut` or `into_inner`:
/// there is no lock and no runtime cost.
///
/// ```
/// use std::cell::Cell;
/// use tutti_rt::Exclusive;
///
/// // `Cell` is `Send` but not `Sync`.
/// struct Owner {
///     count: Exclusive<Cell<u32>>,
/// }
/// fn needs_sync<T: Sync>(_: &T) {}
///
/// let mut owner = Owner { count: Exclusive::new(Cell::new(1)) };
/// needs_sync(&owner);
/// owner.count.get_mut().set(2);
/// assert_eq!(owner.count.into_inner().get(), 2);
/// ```
///
/// A value that is not `Send` stays unshareable:
///
/// ```compile_fail
/// use std::rc::Rc;
/// use tutti_rt::Exclusive;
/// fn needs_sync<T: Sync>(_: &T) {}
/// needs_sync(&Exclusive::new(Rc::new(0)));
/// ```
#[derive(Default)]
#[repr(transparent)]
pub struct Exclusive<T: ?Sized> {
    inner: T,
}

// SAFETY: `Sync` lets threads share `&Exclusive<T>`. No method takes `&self` and returns
// anything that reaches `inner`; every path to it takes `&mut self` or `self`, which a
// shared reference cannot produce. So a shared `Exclusive<T>` grants no access to `T`
// at all, and needs only `T: Send` for the `&mut`/by-value moves between threads.
unsafe impl<T: ?Sized + Send> Sync for Exclusive<T> {}

impl<T: Sized> Exclusive<T> {
    /// Wraps `t`.
    #[must_use]
    pub const fn new(t: T) -> Self {
        Self { inner: t }
    }

    /// The value, unwrapped.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: ?Sized> Exclusive<T> {
    /// The value, through the unique borrow of its holder.
    #[must_use]
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> From<T> for Exclusive<T> {
    fn from(t: T) -> Self {
        Self::new(t)
    }
}

impl<T: ?Sized> core::fmt::Debug for Exclusive<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The value is not reachable through `&self`.
        f.debug_struct("Exclusive").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::Exclusive;
    use std::cell::Cell;

    /// `Cell<u32>` is `Send` and not `Sync`; wrapped, it is `Sync`, a shared borrow
    /// crosses threads, and the value is reached through `get_mut` and `into_inner`.
    #[test]
    fn a_send_value_is_shared_by_its_sync_holder_and_reached_through_mut() {
        fn sync<T: Sync>(_: &T) {}
        let mut e = Exclusive::new(Cell::new(1u32));
        sync(&e);
        e.get_mut().set(7);
        let shared = &e;
        std::thread::scope(|s| {
            s.spawn(move || sync(shared));
        });
        assert_eq!(e.into_inner().get(), 7);
    }

    #[test]
    fn debug_shows_no_value() {
        let e = Exclusive::new(42u8);
        assert_eq!(format!("{e:?}"), "Exclusive { .. }");
    }
}
