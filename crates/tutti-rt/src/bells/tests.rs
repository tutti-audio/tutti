use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use super::*;

/// A guard against a hang, never a speed claim.
const HANG: Duration = Duration::from_secs(60);

/// A waker that sends on a channel.
struct Notify(mpsc::Sender<()>);

impl Wake for Notify {
    fn wake(self: std::sync::Arc<Self>) {
        let _ = self.0.send(());
    }
}

/// A waker that counts its wakes.
#[derive(Default)]
struct Count(AtomicUsize);

impl Wake for Count {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn poll_once(listener: &mut Listener, waker: &Waker) -> Poll<()> {
    pin!(listener.rung()).poll(&mut Context::from_waker(waker))
}

/// Polls the future on this thread, parking between polls: a minimal executor
/// whose waker sends on a channel.
fn block_on(listener: &mut Listener) {
    let (tx, rx) = mpsc::channel();
    let waker = Waker::from(std::sync::Arc::new(Notify(tx)));
    while poll_once(listener, &waker).is_pending() {
        rx.recv_timeout(HANG).expect("the bell's waker was woken");
    }
}

#[test]
fn a_ring_from_another_thread_wakes_a_waiting_future() {
    let executor = tutti_platform::Executor::new();
    let bells = Bells::new(&executor).unwrap();
    let (bell, mut listener) = bells.bell();
    let ringer = std::thread::spawn(move || {
        bell.ring();
        bell
    });
    block_on(&mut listener);
    drop(ringer.join().unwrap());
}

#[test]
fn a_future_resolves_once_for_two_rings_and_waits_for_the_next() {
    let executor = tutti_platform::Executor::new();
    let bells = Bells::new(&executor).unwrap();
    let (bell, mut listener) = bells.bell();
    let (tx, _rx) = mpsc::channel();
    let waker = Waker::from(std::sync::Arc::new(Notify(tx)));
    bell.ring();
    bell.clone().ring();
    assert!(poll_once(&mut listener, &waker).is_ready());
    assert!(poll_once(&mut listener, &waker).is_pending());
    bell.ring();
    assert!(poll_once(&mut listener, &waker).is_ready());
}

#[test]
fn tick_does_nothing_on_a_native_target() {
    let executor = tutti_platform::Executor::new();
    let bells = Bells::new(&executor).unwrap();
    let (bell, mut listener) = bells.bell();
    let count = std::sync::Arc::new(Count::default());
    let waker = Waker::from(std::sync::Arc::clone(&count));
    assert!(poll_once(&mut listener, &waker).is_pending());
    // Nothing rang, so nothing is woken by the tick task or by this call.
    bells.tick();
    assert_eq!(count.0.load(Ordering::SeqCst), 0);
    drop(bell);
}

#[test]
fn dropping_the_bells_ends_the_tick_task() {
    let executor = tutti_platform::Executor::new();
    let bells = Bells::new(&executor).unwrap();
    assert!(bells.tick.is_running());
    // The task holds a clone of the shared state until its body returns.
    let shared = std::sync::Arc::clone(&bells.shared);
    drop(bells);
    assert_eq!(
        std::sync::Arc::strong_count(&shared),
        1,
        "the drop waited for the task, which released its clone"
    );
}

#[test]
fn a_pass_wakes_a_rung_bell_and_not_an_unrung_one() {
    let (shared, _parker) = Bells::shared();
    let bells = Bells {
        shared,
        tick: tick::Tick::none(),
    };
    let (rung_bell, mut rung) = bells.bell();
    let (_quiet_bell, mut quiet) = bells.bell();
    let (rung_count, quiet_count) = (
        std::sync::Arc::new(Count::default()),
        std::sync::Arc::new(Count::default()),
    );
    assert!(poll_once(&mut rung, &Waker::from(std::sync::Arc::clone(&rung_count))).is_pending());
    assert!(
        poll_once(
            &mut quiet,
            &Waker::from(std::sync::Arc::clone(&quiet_count))
        )
        .is_pending()
    );
    rung_bell.ring();
    bells.shared.pass();
    assert_eq!(rung_count.0.load(Ordering::SeqCst), 1);
    assert_eq!(quiet_count.0.load(Ordering::SeqCst), 0);
    // The pass took the waker and left the flag: nothing is pending, and the
    // listener still resolves.
    assert!(!bells.shared.pending());
    assert!(poll_once(&mut rung, Waker::noop()).is_ready());
}

#[test]
fn a_ring_then_the_drop_wakes_the_stored_waker_exactly_once() {
    let executor = tutti_platform::Executor::new();
    let bells = Bells::new(&executor).unwrap();
    let (bell, mut listener) = bells.bell();
    let count = std::sync::Arc::new(Count::default());
    let waker = Waker::from(std::sync::Arc::clone(&count));
    assert!(poll_once(&mut listener, &waker).is_pending());
    bell.ring();
    // The drop waits for the tick task, whose last pass has run by then.
    drop(bells);
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    drop(bell);
}

#[test]
fn the_drop_releases_an_unrung_listeners_waker() {
    let executor = tutti_platform::Executor::new();
    let bells = Bells::new(&executor).unwrap();
    let (bell, mut listener) = bells.bell();
    let count = std::sync::Arc::new(Count::default());
    let waker = Waker::from(std::sync::Arc::clone(&count));
    assert!(poll_once(&mut listener, &waker).is_pending());
    drop(waker);
    assert_eq!(std::sync::Arc::strong_count(&count), 2, "the bell holds it");
    drop(bells);
    assert_eq!(
        std::sync::Arc::strong_count(&count),
        1,
        "the drop released it"
    );
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        0,
        "an unrung bell is not woken"
    );
    drop(bell);
}

#[test]
fn a_poll_after_the_drop_stores_no_waker() {
    let executor = tutti_platform::Executor::new();
    let bells = Bells::new(&executor).unwrap();
    let (bell, mut listener) = bells.bell();
    drop(bells);
    let count = std::sync::Arc::new(Count::default());
    let waker = Waker::from(std::sync::Arc::clone(&count));
    assert!(poll_once(&mut listener, &waker).is_pending());
    drop(waker);
    assert_eq!(std::sync::Arc::strong_count(&count), 1);
    // A ring seen before the drop still resolves a later poll.
    bell.ring();
    assert!(poll_once(&mut listener, Waker::noop()).is_ready());
}

#[test]
fn dropping_the_executor_while_the_bells_live_returns_and_ends_the_tick() {
    let executor = tutti_platform::Executor::new();
    let bells = Bells::new(&executor).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    // The executor's drop stops and unparks the tick's thread; the guard only
    // catches a drop that never returns.
    let dropper = std::thread::spawn(move || {
        drop(executor);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(HANG)
        .expect("the executor's drop returned with the bells alive");
    dropper.join().unwrap();
    assert!(
        !bells.tick.is_running(),
        "the stopped executor ended the tick task"
    );
}

/// **Bells dropped on a thread that drives an executor detach their tick instead of
/// panicking**: a dedicated future builds and drops a set, and its task ends with no
/// panic.
#[test]
fn bells_dropped_on_a_driving_thread_detach_their_tick() {
    let executor = tutti_platform::Executor::new();
    let other = tutti_platform::Executor::new();
    let task = executor
        .spawn_dedicated("drops-bells", None, async move {
            let bells = Bells::new(&other).expect("the bells' tick task");
            drop(bells);
            true
        })
        .unwrap();
    let (out, joined) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = out.send(tutti_platform::join(task));
    });
    assert_eq!(
        joined
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap(),
        Some(true),
        "the drop returns without a panic"
    );
}
