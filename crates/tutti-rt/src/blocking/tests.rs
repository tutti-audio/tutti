use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use super::{Condvar, Mutex, RwLock};
use crate::RtScope;

fn panic_text(result: std::thread::Result<()>) -> String {
    let payload = result.expect_err("the call panicked");
    payload
        .downcast_ref::<String>()
        .cloned()
        .expect("a formatted panic message")
}

/// A lock taken inside a block panics in a debug build, naming the call and the rule.
#[cfg(debug_assertions)]
#[test]
fn locking_inside_a_scope_panics_naming_the_lock_call() {
    let m = Mutex::new(0);
    let _block = RtScope::enter();
    let text = panic_text(catch_unwind(AssertUnwindSafe(|| drop(m.lock()))));
    assert!(text.contains("Mutex::lock on the audio thread"), "{text}");
    assert!(text.contains("never waits on a lock"), "{text}");
}

/// A wait inside a block panics in a debug build, before it releases anything.
#[cfg(debug_assertions)]
#[test]
fn waiting_on_a_condvar_inside_a_scope_panics() {
    let m = Mutex::new(false);
    let cv = Condvar::new();
    let guard = m.lock();
    let _block = RtScope::enter();
    let text = panic_text(catch_unwind(AssertUnwindSafe(|| {
        drop(cv.wait_timeout(guard, Duration::ZERO));
    })));
    assert!(text.contains("Condvar::wait_timeout"), "{text}");
}

/// A try-lock never waits, so a block may take one: it answers the guard when
/// free and `None` when held.
#[test]
fn try_lock_inside_a_scope_answers_without_panicking() {
    let m = Mutex::new(1);
    let _block = RtScope::enter();
    {
        let held = m.try_lock().expect("the lock is free");
        assert_eq!(*held, 1);
        assert!(m.try_lock().is_none(), "a held lock is not taken twice");
    }
    assert!(m.try_lock().is_some());
}

/// Outside a block a lock whose holder panicked still answers its guard, with
/// the last store made before the panic.
#[test]
fn a_poisoned_lock_outside_a_scope_answers_its_guard_with_the_last_store() {
    let m = Mutex::new(1);
    let held = catch_unwind(AssertUnwindSafe(|| {
        let mut g = m.lock();
        *g = 2;
        panic!("holder panics");
    }));
    assert!(held.is_err());
    assert!(m.0.is_poisoned());
    assert_eq!(*m.lock(), 2);
    assert_eq!(*m.try_lock().expect("free"), 2);
    assert_eq!(m.into_inner(), 2);
}

/// Outside a block a wait returns once its condition is false.
#[test]
fn a_condvar_wait_outside_a_scope_returns_when_the_condition_clears() {
    let m = Mutex::new(false);
    let cv = Condvar::new();
    std::thread::scope(|s| {
        s.spawn(|| {
            *m.lock() = true;
            cv.notify_all();
        });
        let guard = cv.wait_while(m.lock(), |done| !*done);
        assert!(*guard);
    });
}

/// A read or a write taken inside a block panics in a debug build, each naming its call.
#[cfg(debug_assertions)]
#[test]
fn reading_or_writing_an_rwlock_inside_a_scope_panics_naming_the_call() {
    let l = RwLock::new(0);
    let _block = RtScope::enter();
    let read = panic_text(catch_unwind(AssertUnwindSafe(|| drop(l.read()))));
    assert!(read.contains("RwLock::read on the audio thread"), "{read}");
    let write = panic_text(catch_unwind(AssertUnwindSafe(|| drop(l.write()))));
    assert!(
        write.contains("RwLock::write on the audio thread"),
        "{write}"
    );
}

/// A poisoned reader-writer lock answers its guards outside a block, holding
/// what the panicking writer stored.
#[test]
fn a_poisoned_rwlock_outside_a_scope_answers_its_guards_with_the_last_store() {
    let l = RwLock::new(0);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let mut g = l.write();
        *g = 7;
        panic!("poison the lock");
    }));
    assert!(l.0.is_poisoned());
    assert_eq!(*l.read(), 7);
    *l.write() = 8;
    assert_eq!(*l.read(), 8);
}
