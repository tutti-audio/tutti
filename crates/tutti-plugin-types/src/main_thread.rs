//! The main-thread token and the work handed to it.
//!
//! All native plugin formats (VST2, VST3, CLAP, AU) require that the calls
//! other than the real-time audio path (the editor, state, parameter
//! enumeration, timers and the run loop) run on the host's main/UI thread. The
//! rule is in the types: an entry that must run there takes a
//! [`MainThreadMarker`], `tutti_platform`'s proof that the holder is on the OS's main
//! thread, by value, and runs no check of its own.
//!
//! A `Drop` cannot take a token. One that releases something the formats close
//! on the main thread, and finds itself on another, hands the close to its
//! owner's [`MainWork`] through a [`MainWorkHandle`], and the owner runs it
//! on its next main-thread tick.
//!
//! # One collector per owner
//!
//! [`MainWork::new`] makes a collector and the [`MainWorkHandle`] every plugin
//! the owner loads holds a clone of. Nothing is shared between owners: two
//! sessions in one process each run only the work their own plugins sent.
//!
//! The queue is unbounded, so a send never waits and nothing is lost to a full
//! queue. [`MainWork::run`] runs what has arrived, on the main thread, with the
//! token in hand.
//!
//! A send whose collector is gone (its owner already dropped) runs the work at
//! once when the sender is on the main thread; otherwise nothing can run it, and
//! the work is counted in [`MainWork::stranded`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

pub use tutti_platform::MainThreadMarker;

/// Work a thread that is not the main thread hands to it.
///
/// Whatever it holds may be main-thread-affine (a plugin's close), so it is
/// never dropped off the main thread: dropped there without having run, it is
/// forgotten and counted in [`MainWork::stranded`]. This is what holds when a
/// send lands after the collector last drained its queue and the receiver's
/// drop then frees it.
struct Work(Option<Box<dyn FnOnce(MainThreadMarker) + Send>>);

impl Work {
    fn new(work: impl FnOnce(MainThreadMarker) + Send + 'static) -> Self {
        Self(Some(Box::new(work)))
    }

    fn run(mut self, main: MainThreadMarker) {
        if let Some(work) = self.0.take() {
            work(main);
        }
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let Some(work) = self.0.take() else {
            return;
        };
        if MainThreadMarker::new().is_some() {
            drop(work);
        } else {
            STRANDED.fetch_add(1, Ordering::Relaxed);
            std::mem::forget(work);
        }
    }
}

/// Work that found no collector and no main thread to run it. Raw: no unit
/// counts pieces of work.
static STRANDED: AtomicU64 = AtomicU64::new(0);

/// Plugin drops handed to a collector by [`MainWorkHandle::defer_drop`]. Raw:
/// no unit counts drops.
static DEFERRED_DROPS: AtomicU64 = AtomicU64::new(0);

/// The collector of the work other threads hand to the main thread: owned by
/// the owner of the plugins that send it (a session), drained on the main
/// thread.
///
/// ```
/// use tutti_plugin_types::{MainThreadMarker, MainWork};
///
/// let main = MainThreadMarker::new().expect("on the main thread");
/// let (work, handle) = MainWork::new();
/// std::thread::spawn(move || handle.defer(|_main| {})).join().unwrap();
/// assert_eq!(work.run(main), 1);
/// assert_eq!(work.run(main), 0, "and once");
/// ```
#[derive(Debug)]
pub struct MainWork {
    queue: Receiver<Work>,
    pumped: Arc<AtomicBool>,
    /// Set when the collector is dropped: no later send is ever run by it.
    gone: Arc<AtomicBool>,
}

/// The send side of a [`MainWork`]: a plugin's way to hand a close to the main
/// thread from a `Drop` that is not on it. Cheap to clone; every plugin the
/// collector's owner loads holds one.
#[derive(Clone, Debug)]
pub struct MainWorkHandle {
    queue: Sender<Work>,
    pumped: Arc<AtomicBool>,
    gone: Arc<AtomicBool>,
}

impl MainWork {
    /// A collector and the handle that feeds it.
    #[must_use]
    pub fn new() -> (MainWork, MainWorkHandle) {
        let (queue, inbox) = channel();
        let pumped = Arc::new(AtomicBool::new(false));
        let gone = Arc::new(AtomicBool::new(false));
        (
            MainWork {
                queue: inbox,
                pumped: Arc::clone(&pumped),
                gone: Arc::clone(&gone),
            },
            MainWorkHandle {
                queue,
                pumped,
                gone,
            },
        )
    }

    /// Runs the work handed over so far, on the calling (main) thread, and says
    /// how many ran. Work that defers more work leaves it for the next call.
    pub fn run(&self, main: MainThreadMarker) -> usize {
        self.pumped.store(true, Ordering::Release);
        let arrived: Vec<Work> = self.queue.try_iter().collect();
        let ran = arrived.len();
        for work in arrived {
            work.run(main);
        }
        ran
    }

    /// How many pieces of work, process-wide since the process began, were sent
    /// to a collector that was already gone from a thread that is not the main
    /// thread, so nothing ran them: each is a plugin close that never happened.
    /// A diagnostic only; it changes no behaviour. The owner of the plugins
    /// should have closed them on the main thread (`Session::close`).
    #[must_use]
    pub fn stranded() -> u64 {
        STRANDED.load(Ordering::Relaxed)
    }

    /// How many plugin drops, process-wide since the process began, a thread
    /// that is not the plugin's own handed to a collector
    /// ([`MainWorkHandle::defer_drop`]), whichever format the plugin is. A
    /// diagnostic only; it changes no behaviour. The owner of the plugins should
    /// have dropped them on the main thread, so an app reads it after closing a
    /// session, and a nonzero count is worth a look.
    #[must_use]
    pub fn deferred_drops() -> u64 {
        DEFERRED_DROPS.load(Ordering::Relaxed)
    }
}

impl Drop for MainWork {
    /// Work still queued when the collector goes runs now on the main thread; off
    /// it nothing can run it, and each piece is counted in [`MainWork::stranded`].
    fn drop(&mut self) {
        self.gone.store(true, Ordering::Release);
        let pending: Vec<Work> = self.queue.try_iter().collect();
        if pending.is_empty() {
            return;
        }
        if let Some(main) = MainThreadMarker::new() {
            for work in pending {
                work.run(main);
            }
        } else {
            STRANDED.fetch_add(pending.len() as u64, Ordering::Relaxed);
            // As in `MainWorkHandle::defer`: dropping a piece of work here would drop
            // what it holds off the main thread, which defers again to this
            // collector. The plugins leak instead.
            std::mem::forget(pending);
        }
    }
}

impl MainWorkHandle {
    /// Whether the collector has run at least once. A collector nobody runs
    /// (its owner never reaches the main thread) never answers `true`, so a
    /// caller that would wait for deferred work declines to wait.
    #[must_use]
    pub fn is_pumped(&self) -> bool {
        self.pumped.load(Ordering::Acquire)
    }

    /// Hands `work` to the collector's main thread, to run at its next
    /// [`MainWork::run`]. Never waits and never loses the work to a full queue.
    ///
    /// If the collector is gone, the work runs now when the caller is on the
    /// main thread, and otherwise is counted in [`MainWork::stranded`]. Work
    /// that queues itself again with this must use [`queue`](Self::queue): with
    /// the collector gone, this runs it inline, and it would nest without end.
    ///
    /// # Never from an audio thread
    ///
    /// This allocates (the box and the queue's node). Callable from any
    /// control-side or pool thread, **not from the audio callback**. The callers
    /// keep to that by an invariant of the graph, not by a check here: a unit
    /// leaves the plan on the control side. The executor moves a retired,
    /// replaced or crossfaded-out unit into the commit box it sends back
    /// (`tutti-graph`'s `exec.rs`: `c.retired.push` in `apply_pending`, the
    /// `faded` ring for a crossfade's outgoing unit), `Editor::collect` drops it
    /// on the control thread, and a `NodeBox`, `Crossfade` or `Commit` dropped
    /// inside the audio scope panics in a debug build (`RtScope::check_outside`).
    /// When a session ends, the stream's owner thread is joined before the driver
    /// releases the last `Arc` of the callback state (`tutti-cpal`'s
    /// `TuttiDriver` field order, `owner.rs`'s join), so the executor and its
    /// units drop on the control thread. So the `RenderHandle` of a plugin node
    /// (in its `Processor`) is never dropped on the audio thread, and its
    /// deferral here never runs there. The in-process VST2 suite pins it:
    /// `tutti-plugin-host/tests/node/vst2_in_process_retire.rs`.
    pub fn defer(&self, work: impl FnOnce(MainThreadMarker) + Send + 'static) {
        let Err(work) = self.queue(work) else {
            return;
        };
        match MainThreadMarker::new() {
            Some(main) => work(main),
            None => {
                STRANDED.fetch_add(1, Ordering::Relaxed);
                // Dropping the work would drop what it holds on this thread, and a
                // plugin dropped off the main thread defers its close again: here, to
                // the same gone collector. The plugin leaks instead.
                std::mem::forget(work);
            }
        }
    }

    /// Hands `work` to the collector's main thread, to run at its next
    /// [`MainWork::run`], or hands it back when the collector is gone: it never
    /// runs here and is never counted stranded, so the caller decides what a
    /// gone collector means to it. Never waits. The one queueing path:
    /// [`defer`](Self::defer) is this plus its fallback.
    ///
    /// A send that loses a race with the collector's drop is not handed back
    /// (the closure is already boxed in the queue); it is dealt with as `defer`
    /// always dealt with a gone collector, and the next call sees the flag.
    ///
    /// Same rules as `defer`: never from an audio thread.
    ///
    /// # Errors
    ///
    /// `Err(work)` when the collector is gone.
    pub fn queue<W>(&self, work: W) -> Result<(), W>
    where
        W: FnOnce(MainThreadMarker) + Send + 'static,
    {
        if self.gone.load(Ordering::Acquire) {
            return Err(work);
        }
        let Err(lost) = self.queue.send(Work::new(work)) else {
            return Ok(());
        };
        match MainThreadMarker::new() {
            Some(main) => lost.0.run(main),
            None => {
                STRANDED.fetch_add(1, Ordering::Relaxed);
                std::mem::forget(lost.0);
            }
        }
        Ok(())
    }

    /// [`defer`](Self::defer) for the close of a plugin dropped off its own
    /// thread, counted in [`MainWork::deferred_drops`]. The same rules apply,
    /// including "never from an audio thread".
    pub fn defer_drop(&self, work: impl FnOnce(MainThreadMarker) + Send + 'static) {
        DEFERRED_DROPS.fetch_add(1, Ordering::Relaxed);
        self.defer(work);
    }
}

#[cfg(test)]
mod tests {
    use super::{MainWork, Work};

    /// **A gone collector hands the work back and runs nothing**; a live one
    /// takes it, and the collector runs it.
    #[test]
    fn a_gone_collector_hands_the_work_back() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let main = tutti_fixtures::main_thread();
        let ran = std::sync::Arc::new(AtomicU32::new(0));
        let job = |ran: &std::sync::Arc<AtomicU32>| {
            let ran = std::sync::Arc::clone(ran);
            move |_main| {
                ran.fetch_add(1, Ordering::Relaxed);
            }
        };
        let (work, handle) = MainWork::new();
        assert!(handle.queue(job(&ran)).is_ok(), "a live collector takes it");
        assert_eq!(work.run(main), 1);
        drop(work);
        let Err(back) = handle.queue(job(&ran)) else {
            panic!("a gone collector took work");
        };
        assert_eq!(ran.load(Ordering::Relaxed), 1, "it ran nothing");
        back(main);
        assert_eq!(
            ran.load(Ordering::Relaxed),
            2,
            "and the work came back whole"
        );
    }

    /// **Only a plugin drop counts as a deferred drop**: `defer_drop` adds one
    /// each, plain `defer` adds none, and the collector runs both.
    #[test]
    fn a_deferred_drop_is_counted_and_other_deferred_work_is_not() {
        let main = tutti_fixtures::main_thread();
        let (work, handle) = MainWork::new();
        let before = MainWork::deferred_drops();
        handle.defer(|_main| {});
        assert_eq!(MainWork::deferred_drops(), before, "plain work");
        handle.defer_drop(|_main| {});
        handle.defer_drop(|_main| {});
        assert_eq!(MainWork::deferred_drops(), before + 2, "two drops");
        assert_eq!(work.run(main), 3, "the collector runs all three");
    }

    /// **Work dropped off the main thread without running is forgotten and
    /// counted**, never dropped: a send that lands after the collector's last
    /// drain is freed by the receiver's drop, which reaches this guard. (The
    /// interleaving itself cannot be made deterministic: it is a send racing the
    /// collector's drop. The guard every queued item carries is what is pinned.)
    /// The count is process-wide; nextest isolates each test in a process of its own.
    #[test]
    fn work_dropped_off_the_main_thread_is_forgotten_and_counted() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Holds(Arc<AtomicBool>);
        impl Drop for Holds {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let held = Holds(Arc::clone(&dropped));
        let before = MainWork::stranded();
        drop(Work::new(move |_main| {
            let _ = &held;
        }));
        assert!(
            !dropped.load(Ordering::SeqCst),
            "what it holds is not dropped here"
        );
        assert_eq!(MainWork::stranded(), before + 1);
    }
}
