use std::time::Duration;

use tutti_winmm_ffi::{Woken, waitable_timer};

#[test]
fn a_timed_wait_with_no_wake_elapses() {
    let (_wake, mut wait) = waitable_timer().unwrap();
    assert_eq!(
        wait.wait_for(Duration::from_millis(30)).unwrap(),
        Woken::Elapsed
    );
}

#[test]
fn a_wake_made_before_the_wait_ends_it_at_once() {
    let (wake, mut wait) = waitable_timer().unwrap();
    wake.wake().unwrap();
    assert_eq!(
        wait.wait_for(Duration::from_secs(30)).unwrap(),
        Woken::Signalled
    );
}

#[test]
fn a_wake_from_a_clone_on_another_thread_ends_a_wait_with_no_time() {
    let (wake, mut wait) = waitable_timer().unwrap();
    let waker = wake.clone();
    let thread = std::thread::spawn(move || {
        waker.wake().unwrap();
    });
    wait.wait().unwrap();
    thread.join().unwrap();
}

#[test]
fn a_wait_with_no_timeout_ignores_an_armed_timer_and_returns_only_on_a_wake() {
    let (wake, mut wait) = waitable_timer().unwrap();
    // Arm the timer for 1 ms with a wake already made: the wake ends the
    // wait, and the timer then fires and stays signalled (it is auto-reset, and
    // nothing waits on it). `wait` must not return for it.
    wake.wake().unwrap();
    assert_eq!(
        wait.wait_for(Duration::from_millis(1)).unwrap(),
        Woken::Signalled
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        wait.wait().unwrap();
        tx.send(()).unwrap();
    });
    // The wait has not returned for the elapsed timer.
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "wait() returned with no wake"
    );
    wake.wake().unwrap();
    // A limit that only guards a hang.
    rx.recv_timeout(Duration::from_secs(30)).unwrap();
    thread.join().unwrap();
}

#[test]
fn a_wake_made_before_a_wait_is_consumed_by_that_wait() {
    let (wake, mut wait) = waitable_timer().unwrap();
    wake.wake().unwrap();
    assert_eq!(
        wait.wait_for(Duration::from_secs(30)).unwrap(),
        Woken::Signalled
    );
    assert_eq!(
        wait.wait_for(Duration::from_millis(10)).unwrap(),
        Woken::Elapsed
    );
}
