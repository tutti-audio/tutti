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
    // The wake was consumed by that wait.
    assert_eq!(
        wait.wait_for(Duration::from_millis(10)).unwrap(),
        Woken::Elapsed
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
