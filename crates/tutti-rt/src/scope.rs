//! [`RtScope`] — a per-thread marker for "this code is inside an audio
//! callback".
//!
//! A *marker*, not an owner: it records that the current thread is running a
//! block right now, for as long as the returned guard lives. It deliberately
//! does not pin one thread as "the" audio thread — `AudioThreadCell`'s docs
//! explain why that stricter check is wrong (CoreAudio migrates its callback
//! between threads). What it answers is the question a destructor needs:
//! "am I about to free memory inside a block?" Hence the name: it marks a
//! *scope* (from [`RtScope::enter`] to its guard's drop), not a thread.
//!
//! Cheap enough to set every block: a `const`-initialised thread-local `Cell`,
//! so entering neither allocates nor registers a destructor.

use std::cell::Cell;

// The sanitizer seam: with the `rtsan` feature on a unix target the module drives
// RealtimeSanitizer's region, otherwise it is zero-sized and does nothing.
#[cfg(all(feature = "rtsan", unix))]
#[path = "scope/region_rtsan.rs"]
mod region;
#[cfg(not(all(feature = "rtsan", unix)))]
#[path = "scope/region_off.rs"]
mod region;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// A per-thread marker that says "this code is inside an audio callback".
///
/// A *marker*, not an owner: it records that the current thread is running a
/// block, for as long as the [`RtScopeGuard`] from [`enter`](Self::enter)
/// lives. It does not pin one thread as "the" audio thread, because some hosts
/// (CoreAudio) move their callback between threads. What it answers is the
/// question a destructor needs: "am I about to free memory inside a block?"
/// [`check_outside`](Self::check_outside) is that question asked from
/// a `Drop` impl.
///
/// Cheap enough to set every block: entering is a `const`-initialised
/// thread-local counter, so it neither allocates nor registers a destructor.
///
/// A render prepared for realtime runs inside the scope; one prepared offline
/// does not, because an offline render may block and allocate.
///
/// With the `rtsan` feature (a unix target; see the crate docs) each guard also
/// holds LLVM's RealtimeSanitizer region, so a C `malloc`, a pthread lock or file
/// I/O inside the scope aborts with the interceptor's stack. Without it the guard
/// is the counter alone.
///
/// # Examples
///
/// ```
/// use tutti_rt::RtScope;
///
/// // `check_outside` panics, in a debug build, only inside a block.
/// let outside = || std::panic::catch_unwind(|| RtScope::check_outside("a value")).is_ok();
/// assert!(outside());
/// {
///     let _block = RtScope::enter();
///     assert_eq!(outside(), !cfg!(debug_assertions));
/// }
/// assert!(outside());
/// ```
#[derive(Debug)]
pub struct RtScope;

/// Marks the current thread as running audio until dropped.
///
/// Returned by [`RtScope::enter`]. Guards nest: the thread stays marked
/// until the last one drops. `!Send`, so it is dropped on the thread that
/// made it.
#[must_use = "the thread is marked only while the guard lives"]
#[derive(Debug)]
pub struct RtScopeGuard {
    // `!Send`: the mark is per thread, so the guard must be dropped on the
    // thread that made it.
    _not_send: core::marker::PhantomData<*const ()>,
    // Ends the sanitizer's region when the guard drops; zero-sized without `rtsan`.
    _region: region::Region,
}

impl RtScope {
    /// Marks the current thread as inside a block until the guard drops.
    ///
    /// An executor calls this at the top of every block. Allocation-free.
    pub fn enter() -> RtScopeGuard {
        DEPTH.with(|d| d.set(d.get() + 1));
        RtScopeGuard {
            _not_send: core::marker::PhantomData,
            _region: region::Region::enter(),
        }
    }

    /// Whether the current thread is inside an [`enter`](Self::enter) scope.
    ///
    /// Allocation-free and never panics, so a global allocator may call it on
    /// every `alloc` and `dealloc`, including during thread teardown, where it
    /// answers `false`. A checking allocator refuses an allocation while it is
    /// `true`.
    #[must_use]
    pub fn is_inside() -> bool {
        DEPTH.try_with(|d| d.get() > 0).unwrap_or(false)
    }

    /// Panics, in debug builds, if the current thread is marked.
    ///
    /// For a `Drop` impl whose value must be freed on the control thread.
    /// `what` names the value in the message. Does nothing in release builds.
    ///
    /// # Panics
    ///
    /// In debug builds, when called inside an [`enter`](Self::enter) scope,
    /// unless the thread is already unwinding (so a first panic is not turned
    /// into an abort).
    pub fn check_outside(what: &str) {
        Self::forbid(
            what,
            "dropped on the audio thread: it must be freed on the control thread",
        );
    }

    /// Panics, in debug builds, if the current thread is marked: a blocking
    /// call named `what` is about to wait. The one check behind
    /// [`Mutex`](crate::Mutex) and [`Condvar`](crate::Condvar).
    pub(crate) fn check_not_blocking(what: &str) {
        Self::forbid(what, "on the audio thread: a block never waits on a lock");
    }

    /// The check both rules share: `what` followed by `rule`, in a debug build,
    /// inside a scope, unless the thread is already unwinding.
    fn forbid(what: &str, rule: &str) {
        if cfg!(debug_assertions) && Self::is_inside() && !std::thread::panicking() {
            // Formatting and printing the panic allocates; the sanitizer stays quiet
            // until the unwind drops this guard, so the message is what prints.
            let _quiet = region::Quiet::new();
            panic!("{what} {rule}");
        }
    }
}

impl Drop for RtScopeGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// A field whose drop asserts, in debug builds, that its owner is dropped
/// outside an audio block: [`RtScope::check_outside`] named for `Owner`.
///
/// A value that must be freed on the control thread holds one instead of
/// writing a `Drop` impl, so it can still be taken apart by a move.
/// Zero-sized, and `Send` and `Sync` whatever `Owner` is.
///
/// ```
/// use tutti_rt::{ControlDrop, RtScope};
///
/// struct Buffers {
///     _control: ControlDrop<Buffers>,
///     frames: Vec<f32>,
/// }
///
/// let buffers = Buffers { _control: ControlDrop::new(), frames: vec![0.0; 64] };
/// let _block = RtScope::enter();
/// let dropped = std::panic::catch_unwind(move || drop(buffers));
/// assert_eq!(dropped.is_err(), cfg!(debug_assertions));
/// ```
pub struct ControlDrop<Owner: ?Sized> {
    _owner: core::marker::PhantomData<fn() -> Owner>,
}

impl<Owner: ?Sized> ControlDrop<Owner> {
    /// The guard for one `Owner`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            _owner: core::marker::PhantomData,
        }
    }
}

impl<Owner: ?Sized> Default for ControlDrop<Owner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Owner: ?Sized> core::fmt::Debug for ControlDrop<Owner> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ControlDrop")
    }
}

impl<Owner: ?Sized> Drop for ControlDrop<Owner> {
    fn drop(&mut self) {
        RtScope::check_outside(core::any::type_name::<Owner>());
    }
}

#[cfg(test)]
mod tests;
