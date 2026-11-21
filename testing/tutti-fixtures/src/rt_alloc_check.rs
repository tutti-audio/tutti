//! An allocator that fails a test which allocates or frees inside an audio
//! block.
//!
//! [`assert_no_alloc::AllocDisabler`] checks only inside an explicit
//! `assert_no_alloc(|| ..)` closure. [`RtAllocCheck`] keeps that, and also
//! refuses every allocation, reallocation and free made while the thread is
//! inside a [`tutti_rt::RtScope`], which every graph block and commit apply
//! enters. A test that renders is then an allocation check with no closure.
//! The binary names the static, because a `#[global_allocator]` must stay in
//! the binary it checks:
//!
//! ```ignore
//! #[global_allocator]
//! static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;
//! ```
//!
//! A refusal writes the thread's name (the test's, under the harness) and a
//! backtrace to stderr, then aborts the process: a panic cannot leave an
//! allocator, because unwinding out of `GlobalAlloc` is undefined behaviour.
//! The report itself allocates, so the check is off on the reporting thread.
//! A thread already unwinding is not checked, so a test's own panic is not
//! turned into an abort.

use assert_no_alloc::AllocDisabler;
use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::io::Write;
use tutti_rt::RtScope;

thread_local! {
    static REPORTING: Cell<bool> = const { Cell::new(false) };
}

/// Refuses the call when this thread is inside an audio block.
fn check(what: &str, layout: Layout) {
    // `try_with`: a call during thread teardown finds no slot, and is let through.
    let reporting = REPORTING.try_with(Cell::get).unwrap_or(true);
    if reporting || !RtScope::is_inside() || std::thread::panicking() {
        return;
    }
    let _ = REPORTING.try_with(|r| r.set(true));
    let thread = std::thread::current();
    let mut err = std::io::stderr().lock();
    let _ = writeln!(
        err,
        "{what} of {} bytes inside an audio block (thread {:?}); allocate and free on the control thread\n{}",
        layout.size(),
        thread.name().unwrap_or("<unnamed>"),
        std::backtrace::Backtrace::force_capture(),
    );
    let _ = err.flush();
    std::process::abort();
}

/// The `assert_no_alloc` allocator that also refuses any allocation or free
/// inside a [`tutti_rt::RtScope`].
#[derive(Debug)]
pub struct RtAllocCheck;

// SAFETY: every method forwards to `AllocDisabler`, which forwards to `System`
// unchanged; the checks are the only addition. `realloc` and `alloc_zeroed` are
// the trait's defaults over `alloc` and `dealloc`, so they are checked too.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for RtAllocCheck {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        check("allocation", layout);
        // SAFETY: the caller's contract is `AllocDisabler`'s.
        unsafe { AllocDisabler.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        check("free", layout);
        // SAFETY: the caller's contract is `AllocDisabler`'s.
        unsafe { AllocDisabler.dealloc(ptr, layout) }
    }
}
