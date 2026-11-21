//! What the `extern "C"` host callbacks share: the abort guard of the thread
//! pool's `request_exec`, the plugin's C strings, and the `ctx` pointers. The
//! panic guard every other callback runs in is
//! [`ffi_guard`](tutti_plugin_types::ffi_guard).

use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Run `f`; if it panics, abort the process.
///
/// For `clap.thread-pool`'s `request_exec`: a `false` answer tells the plugin
/// to run all its tasks itself, but a panic can come after some already ran
/// (or while a pool participant is still inside the job), which would run a
/// task twice or leave one racing. Nothing before the pool runs can panic;
/// a panic after it is not recoverable, so it aborts. It prints nothing: the
/// panic hook has already written the panic's message to stderr.
pub(crate) fn or_abort<R>(f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            std::mem::forget(payload);
            std::process::abort()
        }
    }
}

/// A nul-terminated string a plugin passed, copied lossily; empty for null.
///
/// # Safety
/// `ptr` is null or points to a nul-terminated string readable for the call.
pub(crate) unsafe fn lossy(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: the caller's contract.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

/// The `T` a callback's `ctx` names, shared.
///
/// # Safety
/// `ctx` is the pointer to a live `T` that the call which built the C object
/// stored (`with_input`, `with_raw`, ...), and nothing mutates the `T` while
/// the plugin runs inside that call.
pub(crate) unsafe fn ctx_ref<'a, T>(ctx: *mut c_void) -> &'a T {
    // SAFETY: the contract.
    unsafe { &*ctx.cast::<T>() }
}

/// The `T` a callback's `ctx` names, exclusively.
///
/// # Safety
/// As [`ctx_ref`], and no other reference to the `T` is live meanwhile.
pub(crate) unsafe fn ctx_mut<'a, T>(ctx: *mut c_void) -> &'a mut T {
    // SAFETY: the contract.
    unsafe { &mut *ctx.cast::<T>() }
}
