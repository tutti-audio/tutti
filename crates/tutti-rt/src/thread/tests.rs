use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::*;

/// Dropping the handle requests the stop and joins: the body saw the stop and
/// had finished its exit work by the time `drop` returned.
#[test]
fn dropping_the_handle_stops_and_joins() {
    let saw_stop = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = {
        let (saw_stop, finished) = (Arc::clone(&saw_stop), Arc::clone(&finished));
        spawn_named("stopper", None, move |token| {
            ready_tx.send(()).unwrap();
            while !token.wait(Duration::from_secs(10)) {}
            saw_stop.store(true, Ordering::SeqCst);
            // Work after the stop is seen, which a drop that did not join
            // would return before.
            std::thread::sleep(Duration::from_millis(50));
            finished.store(true, Ordering::SeqCst);
        })
        .unwrap()
    };
    ready_rx.recv().unwrap();
    drop(handle);
    assert!(saw_stop.load(Ordering::SeqCst), "the body saw the stop");
    assert!(
        finished.load(Ordering::SeqCst),
        "drop returned after the join"
    );
}

/// The thread carries the name it was given (one Linux keeps whole: 15 bytes).
#[test]
fn the_thread_has_the_given_name() {
    const NAME: &str = "tutti-test-name";
    assert!(NAME.len() <= 15, "a longer name is truncated on Linux");
    let handle = spawn_named(NAME, None, |_| {
        std::thread::current().name().map(str::to_owned)
    })
    .unwrap();
    assert_eq!(handle.join().unwrap().as_deref(), Some(NAME));
}

/// A panic in the body comes back from `join` as `Panicked`, and a handle
/// dropped over a panic does not unwind its dropper.
#[test]
fn a_panic_comes_back_from_join_and_not_from_drop() {
    let handle = spawn_named("faulty", None, |_| -> u32 { panic!("boom {}", 7) }).unwrap();
    let token = handle.token();
    while !handle.is_finished() {
        std::thread::yield_now();
    }
    assert!(!token.is_stopped(), "a panic is not a stop");
    let fault = handle.join().unwrap_err();
    assert_eq!(
        fault,
        Panicked {
            name: "faulty".into(),
            message: "boom 7".into()
        }
    );

    let dropped = spawn_named("faulty-dropped", None, |_| -> () { panic!("quiet") }).unwrap();
    while !dropped.is_finished() {
        std::thread::yield_now();
    }
    drop(dropped);
}

/// A body asleep on the token wakes on the stop, long before its timeout.
#[test]
fn a_waiting_body_wakes_on_stop() {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = spawn_named("sleeper", None, move |token| {
        ready_tx.send(()).unwrap();
        let started = Instant::now();
        let stopped = token.wait(Duration::from_secs(10));
        (stopped, started.elapsed())
    })
    .unwrap();
    ready_rx.recv().unwrap();
    handle.token().stop();
    let (stopped, slept) = handle.join().unwrap();
    assert!(stopped, "wait reports the stop");
    assert!(slept < Duration::from_secs(5), "woke after {slept:?}");
}

/// A wait that nobody stops returns `false` after its timeout.
#[test]
fn an_unstopped_wait_times_out() {
    let token = StopToken::new();
    assert!(!token.wait(Duration::from_millis(20)));
    token.stop();
    assert!(
        token.wait(Duration::from_secs(10)),
        "a stopped token never sleeps"
    );
}

/// A detached thread keeps running after the handle is gone, and is not stopped.
#[test]
fn a_detached_thread_is_not_stopped() {
    let (tx, rx) = mpsc::channel();
    let handle = spawn_named("detached", None, move |token| {
        tx.send(token.clone()).unwrap();
        let _ = token.wait(Duration::from_secs(10));
    })
    .unwrap();
    let token = rx.recv().unwrap();
    handle.detach();
    assert!(!token.is_stopped());
    token.stop();
}

/// A priority request is carried out on the thread and its outcome comes back.
#[test]
fn a_requested_priority_reports_its_outcome() {
    let handle = spawn_named("raised", Some(PriorityRequest::Raised), |_| ()).unwrap();
    let outcome = handle.priority().expect("a request has an outcome");
    let (PriorityOutcome::Granted { requested, .. } | PriorityOutcome::Refused { requested, .. }) =
        outcome;
    assert_eq!(*requested, PriorityRequest::Raised);
    let none = spawn_named("plain", None, |_| ()).unwrap();
    assert!(none.priority().is_none());
}

/// `join` waits for the body to return by itself and hands back its value,
/// without requesting the stop.
#[test]
fn join_returns_the_value_and_does_not_stop() {
    let handle = spawn_named("returner", None, |token| {
        // Work that takes a moment, so a join that returned early or a stop
        // from the join would show.
        std::thread::sleep(Duration::from_millis(30));
        (7_u32, token.is_stopped())
    })
    .unwrap();
    let token = handle.token();
    assert_eq!(handle.join().unwrap(), (7, false), "the body ran unstopped");
    assert!(!token.is_stopped(), "join does not request the stop");
}
