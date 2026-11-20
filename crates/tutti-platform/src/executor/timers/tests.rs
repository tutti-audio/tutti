use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Wake;
use std::time::Duration;

use super::*;

struct Counting(AtomicUsize);

impl Wake for Counting {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn waker() -> Waker {
    Waker::from(Arc::new(Counting(AtomicUsize::new(0))))
}

fn ids(due: &[(u64, Waker)]) -> Vec<u64> {
    due.iter().map(|(id, _)| *id).collect()
}

/// An instant `ms` milliseconds after `base`.
fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

#[test]
fn nothing_is_due_before_the_deadline_and_it_is_due_at_and_after_it() {
    let base = Instant::now();
    let mut due = Due::default();
    let id = due.arm(None, at(base, 100), &waker());
    assert!(due.take_due(at(base, 99)).is_empty());
    assert_eq!(ids(&due.take_due(at(base, 100))), [id]);
    let late = due.arm(None, at(base, 100), &waker());
    assert_eq!(ids(&due.take_due(at(base, 500))), [late]);
}

#[test]
fn a_due_sleep_is_returned_once() {
    let base = Instant::now();
    let mut due = Due::default();
    due.arm(None, base, &waker());
    assert_eq!(due.take_due(base).len(), 1);
    assert!(due.take_due(at(base, 10)).is_empty());
}

#[test]
fn due_sleeps_come_earliest_deadline_first() {
    let base = Instant::now();
    let mut due = Due::default();
    let late = due.arm(None, at(base, 300), &waker());
    let early = due.arm(None, at(base, 100), &waker());
    let middle = due.arm(None, at(base, 200), &waker());
    assert_eq!(ids(&due.take_due(at(base, 250))), [early, middle]);
    assert_eq!(ids(&due.take_due(at(base, 300))), [late]);
}

#[test]
fn a_removed_sleep_is_never_returned() {
    let base = Instant::now();
    let mut due = Due::default();
    let removed = due.arm(None, at(base, 100), &waker());
    let kept = due.arm(None, at(base, 100), &waker());
    due.remove(removed);
    assert_eq!(ids(&due.take_due(at(base, 100))), [kept]);
}

#[test]
fn the_next_deadline_is_the_earliest_one_still_waiting() {
    let base = Instant::now();
    let mut due = Due::default();
    assert_eq!(due.next_deadline(), None);
    let early = due.arm(None, at(base, 100), &waker());
    due.arm(None, at(base, 200), &waker());
    assert_eq!(due.next_deadline(), Some(at(base, 100)));
    due.remove(early);
    assert_eq!(due.next_deadline(), Some(at(base, 200)));
    due.take_due(at(base, 200));
    assert_eq!(due.next_deadline(), None);
}

#[test]
fn arming_a_waiting_id_replaces_its_waker_and_keeps_one_entry() {
    let base = Instant::now();
    let mut due = Due::default();
    let id = due.arm(None, at(base, 100), &waker());
    assert_eq!(due.arm(Some(id), at(base, 100), &waker()), id);
    assert_eq!(due.waiting(), 1);
    assert_eq!(due.take_due(at(base, 100)).len(), 1);
}

#[test]
fn arming_a_fired_id_registers_it_again() {
    let base = Instant::now();
    let mut due = Due::default();
    let id = due.arm(None, at(base, 100), &waker());
    due.take_due(at(base, 100));
    let again = due.arm(Some(id), at(base, 200), &waker());
    assert_ne!(again, id);
    assert_eq!(due.next_deadline(), Some(at(base, 200)));
}

#[test]
fn dropping_a_pending_sleep_frees_its_waker() {
    let timers = Arc::new(Timers::new());
    let counting = Arc::new(Counting(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counting));
    let mut sleep = Sleep::new(&timers, Instant::now() + Duration::from_secs(3600));
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
    assert_eq!(
        Arc::strong_count(&counting),
        3,
        "ours, the waker's, the timer's"
    );
    drop(sleep);
    assert_eq!(Arc::strong_count(&counting), 2);
    drop(waker);
    assert_eq!(Arc::strong_count(&counting), 1);
}

#[test]
fn a_sleep_with_a_past_deadline_resolves_on_its_first_poll() {
    let timers = Arc::new(Timers::new());
    let waker = waker();
    let mut cx = Context::from_waker(&waker);
    let mut sleep = Sleep::new(&timers, Instant::now());
    assert!(Pin::new(&mut sleep).poll(&mut cx).is_ready());
}

#[test]
fn fire_wakes_the_sleeps_due_and_counts_them() {
    let timers = Arc::new(Timers::new());
    let base = Instant::now();
    let counting = Arc::new(Counting(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counting));
    let mut cx = Context::from_waker(&waker);
    let mut early = Sleep::new(&timers, at(base, 3_600_000));
    let mut late = Sleep::new(&timers, at(base, 7_200_000));
    assert!(Pin::new(&mut early).poll(&mut cx).is_pending());
    assert!(Pin::new(&mut late).poll(&mut cx).is_pending());
    assert_eq!(timers.fire(at(base, 1_000)), 0);
    assert_eq!(timers.fire(at(base, 3_600_000)), 1);
    assert_eq!(counting.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        lock(&timers.state).due.next_deadline(),
        Some(at(base, 7_200_000))
    );
}

#[test]
fn a_closed_queue_arms_no_sleep() {
    let timers = Arc::new(Timers::new());
    timers.close();
    let counting = Arc::new(Counting(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counting));
    let mut cx = Context::from_waker(&waker);
    let mut sleep = Sleep::new(&timers, Instant::now() + Duration::from_secs(3600));
    assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
    assert_eq!(lock(&timers.state).due.next_deadline(), None);
    assert_eq!(Arc::strong_count(&counting), 2, "no waker is kept");
}
