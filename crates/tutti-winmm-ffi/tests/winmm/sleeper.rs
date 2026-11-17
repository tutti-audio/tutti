use std::time::{Duration, Instant};

use tutti_winmm_ffi::{Sleeper, Woken};

#[test]
fn a_wait_ends_at_its_time() {
    let sleeper = Sleeper::new().unwrap();
    let start = Instant::now();
    assert_eq!(
        sleeper.wait(Some(Duration::from_millis(30))).unwrap(),
        Woken::Elapsed
    );
    // A wait never ends before its time; only lateness depends on load.
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(25), "{took:?}");
}

#[test]
fn a_wake_made_before_the_wait_ends_it_at_once() {
    let sleeper = Sleeper::new().unwrap();
    sleeper.wake().unwrap();
    assert_eq!(
        sleeper.wait(Some(Duration::from_secs(30))).unwrap(),
        Woken::Signalled
    );
}

#[test]
fn a_wake_from_another_thread_ends_a_wait_with_no_time() {
    let sleeper = std::sync::Arc::new(Sleeper::new().unwrap());
    let waker = std::sync::Arc::clone(&sleeper);
    let thread = std::thread::spawn(move || {
        waker.wake().unwrap();
    });
    assert_eq!(sleeper.wait(None).unwrap(), Woken::Signalled);
    thread.join().unwrap();
}
