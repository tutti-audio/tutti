//! A `loom` model of [`RenderCount`]: the shipped code, whose atomics and fence
//! are loom's under the loom cfg (see `src/render_count.rs`'s `sync` module).
//!
//! ```text
//! cargo test -p tutti-rt --release --test render_count_loom   (with the loom cfg flag set)
//! ```
//!
//! The clearer stands for a withdrawal: it clears a callback slot (an atomic
//! written `Relaxed`, as the AU's own property write is to this code), asks
//! [`quiescent`](RenderCount::quiescent), and frees the state the slot named
//! only when it answers `true`. The renderer stands for a render: it enters,
//! reads the slot, and uses the state only if the slot still names it. The
//! state is a `loom::cell::UnsafeCell`, so loom reports any use that is not
//! ordered against the free. Over every interleaving:
//!
//! - **a render that starts after a quiescent check sees the cleared slot**, and
//!   so never uses the state the clearer freed (the first test);
//! - **a render the check counted keeps the state alive**: the clearer that sees
//!   it does not free, and the render's use is ordered before any later free
//!   (the first test);
//! - **the model can fail**: a clearer that frees without asking is a data race
//!   loom reports (the second test).

#![cfg(loom)]

use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;
use tutti_rt::RenderCount;

/// What the callback slot names: used by a render, freed by a clearer.
struct State(UnsafeCell<u32>);

// SAFETY (test): shared across threads only through the count's protocol, which
// is what the model checks: loom flags every access pair not ordered by
// happens-before.
unsafe impl Sync for State {}

impl State {
    fn used(&self) -> u32 {
        // SAFETY (test): a shared access loom checks.
        self.0.with(|p| unsafe { *p })
    }
    fn free(&self) {
        // SAFETY (test): an exclusive access loom checks.
        self.0.with_mut(|p| unsafe { *p = 0 });
    }
}

/// One render: enters, reads the slot, and uses the state when it is named.
fn render(count: &RenderCount, slot: &AtomicUsize, state: &State) {
    let _in_flight = count.enter();
    if slot.load(Ordering::Relaxed) == 1 {
        assert_eq!(state.used(), 5);
    }
}

#[test]
fn a_render_after_a_quiescent_check_sees_the_cleared_slot() {
    loom::model(|| {
        let count = Arc::new(RenderCount::new());
        let slot = Arc::new(AtomicUsize::new(1));
        let state = Arc::new(State(UnsafeCell::new(5)));

        let renderer = {
            let (count, slot, state) = (count.clone(), slot.clone(), state.clone());
            thread::spawn(move || render(&count, &slot, &state))
        };

        slot.store(0, Ordering::Relaxed);
        if count.quiescent() {
            state.free();
        }
        renderer.join().unwrap();
    });
}

#[test]
#[should_panic]
fn a_clearer_that_frees_without_asking_is_a_race() {
    loom::model(|| {
        let count = Arc::new(RenderCount::new());
        let slot = Arc::new(AtomicUsize::new(1));
        let state = Arc::new(State(UnsafeCell::new(5)));

        let renderer = {
            let (count, slot, state) = (count.clone(), slot.clone(), state.clone());
            thread::spawn(move || render(&count, &slot, &state))
        };

        slot.store(0, Ordering::Relaxed);
        state.free();
        renderer.join().unwrap();
    });
}
