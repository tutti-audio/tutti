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

use tutti_rt::{Tally, TallyWriter, tally};

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
    /// Plugin drops [`MainWorkHandle::defer_drop`] handed to this collector.
    deferred_drops: Tally<u64>,
}

/// The send side of a [`MainWork`]: a plugin's way to hand a close to the main
/// thread from a `Drop` that is not on it. Cheap to clone; every plugin the
/// collector's owner loads holds one.
#[derive(Clone, Debug)]
pub struct MainWorkHandle {
    queue: Sender<Work>,
    pumped: Arc<AtomicBool>,
    /// Adds to [`MainWork::deferred_drops`].
    deferred_drops: TallyWriter<u64>,
}

impl MainWork {
    /// A collector and the handle that feeds it.
    #[must_use]
    pub fn new() -> (MainWork, MainWorkHandle) {
        let (queue, inbox) = channel();
        let pumped = Arc::new(AtomicBool::new(false));
        let deferred_drops = tally();
        (
            MainWork {
                queue: inbox,
                pumped: Arc::clone(&pumped),
                deferred_drops: deferred_drops.clone(),
            },
            MainWorkHandle {
                queue,
                pumped,
                deferred_drops: deferred_drops.writer(),
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

    /// How many plugin drops a thread that is not the plugin's own handed to
    /// this collector ([`MainWorkHandle::defer_drop`]) since it was made,
    /// whichever format the plugin is. A diagnostic only; it changes no
    /// behaviour. The owner of the plugins should have dropped them on the main
    /// thread, and a nonzero count is worth a look.
    #[must_use]
    pub fn deferred_drops(&self) -> u64 {
        self.deferred_drops.get()
    }
}

impl Drop for MainWork {
    /// Work still queued when the collector goes runs now on the main thread; off
    /// it nothing can run it, and each piece is counted in [`MainWork::stranded`].
    fn drop(&mut self) {
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
    /// main thread, and otherwise is counted in [`MainWork::stranded`].
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
        let Err(gone) = self.queue.send(Work::new(work)) else {
            return;
        };
        match MainThreadMarker::new() {
            Some(main) => gone.0.run(main),
            None => {
                STRANDED.fetch_add(1, Ordering::Relaxed);
                // Dropping the work would drop what it holds on this thread, and a
                // plugin dropped off the main thread defers its close again: here, to
                // the same gone collector. The plugin leaks instead.
                std::mem::forget(gone.0);
            }
        }
    }

    /// [`defer`](Self::defer) for the close of a plugin dropped off its own
    /// thread, counted in [`MainWork::deferred_drops`]. The same rules apply,
    /// including "never from an audio thread".
    pub fn defer_drop(&self, work: impl FnOnce(MainThreadMarker) + Send + 'static) {
        self.deferred_drops.add(1);
        self.defer(work);
    }
}

#[cfg(test)]
mod tests {
    use super::{MainWork, Work};

    /// **Only a plugin drop counts as a deferred drop, and only on its own
    /// collector**: `defer_drop` adds one each, plain `defer` adds none, and a
    /// second collector in the process counts nothing of the first's.
    #[test]
    fn a_deferred_drop_is_counted_by_its_own_collector_only() {
        let main = tutti_fixtures::main_thread();
        let (work, handle) = MainWork::new();
        let (other, _other_handle) = MainWork::new();
        handle.defer(|_main| {});
        assert_eq!(work.deferred_drops(), 0, "plain work");
        handle.defer_drop(|_main| {});
        handle.defer_drop(|_main| {});
        assert_eq!(work.deferred_drops(), 2, "two drops");
        assert_eq!(other.deferred_drops(), 0, "another collector's count");
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
