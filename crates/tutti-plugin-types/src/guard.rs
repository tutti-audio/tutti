//! The panic guard every callback the host exposes to a plugin runs its body in.
//!
//! A panic that unwinds out of an `extern "C"` or COM shim into a plugin's
//! frames aborts the process or is undefined behaviour, which takes the host
//! (in-process) or the plugin server with it. A callback answers its "not
//! handled" value instead.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};

/// Panics [`ffi_guard`] has contained since the process began.
static CONTAINED: AtomicU64 = AtomicU64::new(0);

/// `[main-thread]` host callbacks refused off the main thread since the process began.
static REFUSED_OFF_THREAD: AtomicU64 = AtomicU64::new(0);

/// Counts one `[main-thread]` host callback refused because a plugin made it
/// from another thread. For a format's host-callback shims; wait-free, so the
/// audio thread may call it.
pub fn count_refused_off_thread_call() {
    REFUSED_OFF_THREAD.fetch_add(1, Ordering::Relaxed);
}

/// How many `[main-thread]` host callbacks a plugin made from another thread (or
/// from the main thread while it holds the audio claim), process-wide since the
/// process began, like [`contained_panics`]. Each was refused and answered its
/// fallback, so a nonzero count is a plugin that breaks its format's thread rules.
#[must_use]
pub fn refused_off_thread_calls() -> u64 {
    REFUSED_OFF_THREAD.load(Ordering::Relaxed)
}

/// Runs `f`; if it panics, returns `fallback` (the callback's "not handled"
/// value) and counts the panic in [`contained_panics`].
///
/// Callable on the audio thread: the no-panic path allocates nothing. The panic
/// path is a bug path, not a real-time one: the default panic hook has already
/// formatted the message and written it to stderr before the guard sees the
/// panic, and the payload is leaked (forgotten, since its `Drop` could itself
/// panic outside the `catch_unwind`). The guard itself only increments a counter
/// and logs nothing. Under `panic = "abort"` the guard does nothing and a panic
/// aborts.
///
/// ```
/// let answered = tutti_plugin_types::ffi_guard(-1, || -> i32 { panic!("a callback body") });
/// assert_eq!(answered, -1);
/// assert_eq!(tutti_plugin_types::ffi_guard(-1, || 7), 7);
/// ```
pub fn ffi_guard<R>(fallback: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            CONTAINED.fetch_add(1, Ordering::Relaxed);
            fallback
        }
    }
}

/// How many panics [`ffi_guard`] has contained, process-wide since the process
/// began. Each is a host callback that answered its fallback instead of
/// unwinding into a plugin, so a nonzero count is a bug in the host.
///
/// A diagnostic only; it changes no behaviour. An app reads it after a session
/// (or a test run) to learn whether any callback panicked.
#[must_use]
pub fn contained_panics() -> u64 {
    CONTAINED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{contained_panics, ffi_guard};

    #[test]
    fn a_panicking_body_answers_the_fallback_and_is_counted_once() {
        let before = contained_panics();
        let answered: i32 = ffi_guard(-1, || panic!("a callback body"));
        assert_eq!(answered, -1);
        assert_eq!(contained_panics(), before + 1);
    }

    #[test]
    fn a_refused_off_thread_call_is_counted_once() {
        let before = super::refused_off_thread_calls();
        super::count_refused_off_thread_call();
        assert_eq!(super::refused_off_thread_calls(), before + 1);
    }

    #[test]
    fn a_returning_body_keeps_its_value_and_counts_nothing() {
        let before = contained_panics();
        assert_eq!(ffi_guard(0_i32, || 41 + 1), 42);
        assert_eq!(contained_panics(), before);
    }
}
