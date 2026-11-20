use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use super::os::Deadline;
use super::*;
use crate::Instant;

/// Dropping the handle requests the stop and joins: the body had seen the stop
/// by the time `drop` returned. A drop that did not join returns before the
/// body wakes, so this catches it with high probability, not deterministically.
#[test]
fn dropping_the_handle_stops_and_joins() {
    let saw_stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = {
        let saw_stop = Arc::clone(&saw_stop);
        spawn_named("stopper", None, move |token| {
            ready_tx.send(()).unwrap();
            while !token.wait(Duration::from_secs(10)) {}
            saw_stop.store(true, Ordering::SeqCst);
        })
        .unwrap()
    };
    ready_rx.recv().unwrap();
    drop(handle);
    assert!(
        saw_stop.load(Ordering::SeqCst),
        "drop returned before the body saw the stop"
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

/// A body asleep on the token wakes on the stop and reports it, where a wait
/// that slept out its timeout would report no stop.
#[test]
fn a_waiting_body_wakes_on_stop() {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = spawn_named("sleeper", None, move |token| {
        ready_tx.send(()).unwrap();
        token.wait(Duration::from_secs(10))
    })
    .unwrap();
    ready_rx.recv().unwrap();
    handle.token().stop();
    assert!(handle.join().unwrap(), "wait reports the stop");
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
#[cfg_attr(
    miri,
    ignore = "calls the OS scheduler (pthread_getschedparam, setpriority), which miri does not support"
)]
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
    let handle = spawn_named("returner", None, |token| (7_u32, token.is_stopped())).unwrap();
    let token = handle.token();
    assert_eq!(handle.join().unwrap(), (7, false), "the body ran unstopped");
    assert!(!token.is_stopped(), "join does not request the stop");
}

/// A body that returns on its stop ends within the bound and its value comes back.
#[test]
fn stop_within_returns_the_value_of_a_body_that_heeds_the_stop() {
    let handle = spawn_named("heeds", None, |stop| {
        while !stop.wait(Duration::from_secs(60)) {}
        7
    })
    .expect("spawn");
    assert_eq!(
        handle.stop_within(Duration::from_secs(30)).ok(),
        Some(Ok(7))
    );
}

/// A body still blocked at the bound comes back as its handle, stopped and still joinable.
#[test]
fn stop_within_hands_back_a_body_that_has_not_ended() {
    let (release, released) = mpsc::channel::<()>();
    let handle = spawn_named("stuck", None, move |_| {
        let _ = released.recv();
        9
    })
    .expect("spawn");
    let handle = handle
        .stop_within(Duration::from_millis(20))
        .expect_err("the body ignores the stop and is still blocked");
    assert!(handle.token().is_stopped(), "the stop stays requested");
    release.send(()).expect("the body is waiting");
    assert_eq!(
        handle.join(),
        Ok(9),
        "the handed-back handle still owns the thread"
    );
}

/// A body that panics on its stop comes back as `Panicked`, not as a timeout.
#[test]
fn stop_within_returns_a_panic_as_panicked() {
    let handle = spawn_named("panics", None, |stop| {
        while !stop.wait(Duration::from_secs(60)) {}
        panic!("on stop");
    })
    .expect("spawn");
    let outcome: Result<(), Panicked> = handle
        .stop_within(Duration::from_secs(30))
        .expect("the body ended");
    assert_eq!(
        outcome,
        Err(Panicked {
            name: "panics".to_owned(),
            message: "on stop".to_owned()
        })
    );
}

/// A deadline has the time left until it and nothing from its instant on,
/// at constructed instants.
#[test]
fn a_deadline_has_the_time_left_until_its_instant() {
    let t0 = Instant::now();
    let deadline = Deadline::after(t0, Duration::from_secs(10));
    assert_eq!(deadline.left(t0), Some(Duration::from_secs(10)));
    assert_eq!(
        deadline.left(t0 + Duration::from_secs(4)),
        Some(Duration::from_secs(6))
    );
    assert_eq!(deadline.left(t0 + Duration::from_secs(10)), None);
    assert_eq!(deadline.left(t0 + Duration::from_secs(11)), None);
    assert_eq!(Deadline::after(t0, Duration::ZERO).left(t0), None);
}

/// A timeout the clock cannot represent never ends.
#[test]
fn an_overflowing_deadline_never_ends() {
    let t0 = Instant::now();
    let deadline = Deadline::after(t0, Duration::MAX);
    assert_eq!(
        deadline.left(t0 + Duration::from_secs(1_000_000)),
        Some(Duration::MAX)
    );
}
