//! Building a session fits a small thread stack.
//!
//! A debug build puts every `Session` a build step returns in its own stack slot, so the
//! size of a session is multiplied by the depth of the build. A `Session` is a few KiB:
//! the executor's transport history and the engine's and graph's executors are boxed, so
//! a build's frames stay small. The test builds and drops on a 512 KiB stack.

use tutti_session::Session;
use tutti_units::SampleRate;

/// The stack, in bytes, the build runs on: far under a 2 MiB test thread's.
const STACK: usize = 512 * 1024;

/// **Building an offline and a detached session, and dropping them, fits 512 KiB of
/// stack.** The thread is the test's own bound on the build's frame, not a setting that
/// makes a large one pass: it overflows (and aborts the run) if a `Session` grows back
/// to a size that its build steps multiply past it.
#[test]
fn build_and_drop_fit_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            let offline = Session::builder()
                .build()
                .expect("an offline session builds");
            drop(offline);
            let detached = Session::builder()
                .build_detached(SampleRate::SR_48K)
                .expect("a detached session builds");
            drop(detached);
        })
        .expect("the thread starts")
        .join()
        .expect("the build finished on its stack");
}
