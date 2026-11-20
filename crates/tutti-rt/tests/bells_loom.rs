//! A `loom` model of [`Bells`]: the shipped ring, tick step and wait poll,
//! whose atomics, fences, mutexes and park are loom's under the flag (see
//! `src/bells.rs`'s `sync` module).
//!
//! ```text
//! cargo xtask loom bells_loom   # sets the loom cfg flag and the preemption bound
//! ```
//!
//! The tick thread's body is `Ticker::tick_once` in a loop, run here as a
//! loom thread. The waiting side polls a `Listener` future and parks between
//! polls; its `Waker` unparks it. Loom reports a lost wake as a deadlock (the
//! waiter parked with the flag set and its waker never woken):
//!
//! - `a_ring_is_never_a_lost_wake`: one ring on the audio side, the tick and
//!   one wait poll in every interleaving;
//! - `two_rings_resolve_once_and_the_tick_parks`: two rings before the wait
//!   resolves it once (a further poll is `Pending`), and the tick parks (the
//!   model ends only if it does);
//! - `a_rung_bell_with_no_waiter_lets_the_tick_park`: nobody waits, the tick
//!   parks rather than spins, and a later wait resolves at once.

#![cfg(loom)]

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use loom::thread;
use tutti_rt::{Bells, Listener};

// The waiter is a spawned thread: loom's park on the model's main thread trips
// an assertion of its own (`Notify::wait`).

/// Unparks the thread that polls.
struct Unpark(thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn poll_once(listener: &mut Listener, waker: &Waker) -> Poll<()> {
    pin!(listener.rung()).poll(&mut Context::from_waker(waker))
}

/// Polls until ready, parking between polls, then returns the listener.
fn wait(mut listener: Listener) -> Listener {
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    while poll_once(&mut listener, &waker).is_pending() {
        thread::park();
    }
    listener
}

#[test]
fn a_ring_is_never_a_lost_wake() {
    loom::model(|| {
        let (bells, mut ticker) = Bells::stepped();
        let (bell, listener) = bells.bell();
        let tick = thread::spawn(move || while ticker.tick_once() {});
        let waiter = thread::spawn(move || wait(listener));
        bell.ring();
        drop(waiter.join().unwrap());
        bells.shut();
        tick.join().unwrap();
        drop(bell);
    });
}

#[test]
fn two_rings_resolve_once_and_the_tick_parks() {
    loom::model(|| {
        let (bells, mut ticker) = Bells::stepped();
        let (bell, listener) = bells.bell();
        let tick = thread::spawn(move || while ticker.tick_once() {});
        bell.ring();
        bell.ring();
        let waiter = thread::spawn(move || {
            let mut listener = wait(listener);
            // The one resolve took both rings.
            let waker = Waker::from(Arc::new(Unpark(thread::current())));
            assert!(poll_once(&mut listener, &waker).is_pending());
        });
        waiter.join().unwrap();
        // `shut` ends the tick thread only if it parked rather than spun.
        bells.shut();
        tick.join().unwrap();
        drop(bell);
    });
}

#[test]
fn a_rung_bell_with_no_waiter_lets_the_tick_park() {
    loom::model(|| {
        let (bells, mut ticker) = Bells::stepped();
        let (bell, listener) = bells.bell();
        let tick = thread::spawn(move || while ticker.tick_once() {});
        bell.ring();
        bells.shut();
        tick.join().unwrap();
        // A later wait resolves at once: the tick left the flag.
        let waiter = thread::spawn(move || wait(listener));
        drop(waiter.join().unwrap());
        drop(bell);
    });
}
