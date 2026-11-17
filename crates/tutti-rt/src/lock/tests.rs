use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use super::LockExt;

/// A lock whose holder panicked is poisoned, and `lock_recover` still answers
/// its guard, holding the last store made before the panic.
#[test]
fn a_poisoned_lock_answers_its_guard_with_the_last_store() {
    let m = Mutex::new(1);
    let held = catch_unwind(AssertUnwindSafe(|| {
        let mut g = m.lock_recover();
        *g = 2;
        panic!("holder panics");
    }));
    assert!(held.is_err());
    assert!(m.is_poisoned());
    assert_eq!(*m.lock_recover(), 2);
}
