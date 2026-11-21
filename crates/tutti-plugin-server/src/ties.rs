//! What the main thread and the render thread wake each other with.
//!
//! Nothing on either side sleeps for a guessed time: a thread that has nothing
//! to do waits on the event that gives it something.
//!
//! - **The render thread's wake** ([`Ties::wake`]) is a [`Bell`] the main thread
//!   rings after it queues a command, lends or recalls the Processor, or stops
//!   the thread. The render thread waits on it together with the audio
//!   connection.
//! - **The main thread's settle** ([`Settle`]) is a count the render thread
//!   raises ([`Notify::bump`]) after it takes a command, trades the Processor or
//!   hands something back, with a `tutti_rt` [`Unparker`]. The main thread reads
//!   the count, looks at what it waits for, and waits for the count to move
//!   ([`Settle::wait_past`]), so a change that lands between its look and its
//!   wait is not missed.
//! - **The main loop's bell** ([`Ties::bell`]) is the plugin's: it ends the
//!   loop's wait when a plugin calls the host from a thread of its own.
//!
//! A wait that must end is bounded by a [`Patience`], a deadline that is a
//! function of the `now` it is asked at.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tutti_platform::Instant;

use tutti_rt::{LockExt, Parker, Tally, Unparker, parker, tally};

use crate::main_loop::Bell;

/// How long a wait is given: a deadline, as a value of the clock it is asked
/// at. It reads no clock, so its arithmetic is tested with made-up instants.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Patience {
    until: Option<Instant>,
}

impl Patience {
    /// A deadline `limit` after `now`; a limit that overflows the clock never
    /// ends.
    pub(crate) fn new(now: Instant, limit: Duration) -> Self {
        Self {
            until: now.checked_add(limit),
        }
    }

    /// What is left at `now`, or `None` once the deadline is reached.
    pub(crate) fn left(self, now: Instant) -> Option<Duration> {
        match self.until {
            Some(until) => until.checked_duration_since(now).filter(|d| !d.is_zero()),
            None => Some(Duration::MAX),
        }
    }
}

/// The main thread's end of the render thread's progress: see the module docs.
#[derive(Clone)]
pub(crate) struct Settle {
    progress: Arc<AtomicU64>,
    parker: Arc<Mutex<Parker>>,
}

/// The render thread's end of its progress.
pub(crate) struct Notify {
    progress: Arc<AtomicU64>,
    unparker: Unparker,
}

impl Notify {
    /// Tells the main thread the render thread has done something: raises the
    /// count and wakes a main thread that waits. Allocates nothing, takes no
    /// lock and never waits.
    pub(crate) fn bump(&self) {
        self.progress.fetch_add(1, Ordering::Release);
        self.unparker.unpark();
    }
}

impl Settle {
    /// The count now: read before looking at what is waited for.
    pub(crate) fn seen(&self) -> u64 {
        self.progress.load(Ordering::Acquire)
    }

    /// Waits until the count is past `seen`, or `patience` is out. It may
    /// return with the count unmoved (a spurious wake): the caller looks again
    /// and asks `patience`.
    pub(crate) fn wait_past(&self, seen: u64, patience: Patience) {
        let Some(left) = patience.left(Instant::now()) else {
            return;
        };
        self.parker
            .lock_recover()
            .park_timeout(|| self.progress.load(Ordering::Acquire) != seen, left);
    }
}

/// Everything the main thread holds to wake the render thread and be woken by
/// it, and what the plugins it serves ring: made once, on the main thread, with
/// the session.
#[derive(Clone)]
pub(crate) struct Ties {
    /// What a plugin's call on any thread rings.
    pub(crate) bell: Bell,
    /// What ends the render thread's wait.
    pub(crate) wake: Bell,
    /// The render thread's progress.
    pub(crate) settle: Settle,
    /// Set when the host process has ended: the main loop then shuts down
    /// as it does when the control connection ends.
    pub(crate) host_gone: Arc<AtomicBool>,
    /// Commands for the render thread that the main thread could not queue
    /// ([`RenderLink::tell`](crate::render::RenderLink::tell)).
    pub(crate) commands_refused: Tally<u64>,
    /// One-way edits (an automation state, a track's info, a parameter write)
    /// the plugin refused: the wire has no reply to carry the refusal.
    pub(crate) edits_refused: Tally<u64>,
}

/// The render thread's half of [`Ties`].
pub(crate) struct RenderTies {
    /// What the render thread waits on, with its connection.
    pub(crate) wake: Bell,
    /// How it tells the main thread.
    pub(crate) notify: Notify,
}

/// A main thread's ties and the render thread's.
///
/// # Errors
///
/// The OS refused a bell.
pub(crate) fn ties() -> std::io::Result<(Ties, RenderTies)> {
    let (wake, bell) = (Bell::new()?, Bell::new()?);
    let progress = Arc::new(AtomicU64::new(0));
    let (unparker, parker) = parker();
    Ok((
        Ties {
            bell,
            wake: wake.clone(),
            settle: Settle {
                progress: Arc::clone(&progress),
                parker: Arc::new(Mutex::new(parker)),
            },
            host_gone: Arc::default(),
            commands_refused: tally(),
            edits_refused: tally(),
        },
        RenderTies {
            wake,
            notify: Notify { progress, unparker },
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_millis(40);

    /// **A patience is out exactly at its deadline**, whatever the clock read
    /// in between.
    #[test]
    fn a_patience_is_out_at_its_deadline_and_not_before() {
        let t0 = Instant::now();
        let patience = Patience::new(t0, LIMIT);
        assert_eq!(patience.left(t0), Some(LIMIT));
        assert_eq!(
            patience.left(t0 + LIMIT / 4),
            Some(LIMIT - LIMIT / 4),
            "the time left shrinks with the clock"
        );
        assert!(
            patience
                .left(t0 + LIMIT - Duration::from_nanos(1))
                .is_some()
        );
        assert_eq!(patience.left(t0 + LIMIT), None);
        assert_eq!(patience.left(t0 + LIMIT * 9), None, "late is still out");
    }

    /// A limit of zero is out at once: the wait that only looks.
    #[test]
    fn a_zero_patience_is_out_at_once() {
        let t0 = Instant::now();
        assert_eq!(Patience::new(t0, Duration::ZERO).left(t0), None);
    }

    /// **A bump from another thread ends a wait on the count**, and a bump
    /// before the wait ends it at once: the wait's limit outlasts the test
    /// harness's, so a wait that never wakes fails.
    #[test]
    fn a_bump_ends_a_wait_past_the_count_it_was_read_at() {
        let (ties, render) = super::ties().expect("bells");
        let seen = ties.settle.seen();
        let waiter = std::thread::spawn({
            let settle = ties.settle.clone();
            move || {
                settle.wait_past(
                    seen,
                    Patience::new(Instant::now(), Duration::from_secs(600)),
                );
                settle.seen()
            }
        });
        render.notify.bump();
        assert!(waiter.join().expect("the waiter") > seen);

        let (ties, render) = super::ties().expect("bells");
        let seen = ties.settle.seen();
        render.notify.bump();
        ties.settle.wait_past(
            seen,
            Patience::new(Instant::now(), Duration::from_secs(600)),
        );
        assert!(ties.settle.seen() > seen);
    }
}
