//! The streamer's drivers, as a typestate: a streamer is [`Built`], then
//! either [`Running`] as a future on an executor or `Manual`, stepped on the
//! caller's thread.
//!
//! The two drivers own the same streamer-local state (`Local`: the stream
//! table and the scratch), so a streamer is one or the other for its whole
//! life, and the type says which. Handing the state to the future consumes the
//! `Built`; stepping by hand needs a `Manual`, which has no future to race.
//! What was two runtime flags, two asserts and a `should_panic` test is a
//! method that exists on one type and not the other.
//!
//! The future's body and the command handlers live in sibling modules.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tutti_platform::{Executor, PriorityRequest};
use tutti_rt::{Bell, Bells, Listener};
use tutti_units::SampleRate;

use super::bell::StreamerSender;
use super::cache::LruCache;
use super::command::StreamerCommand;
use super::config::BufferConfig;
use super::decode::Seams;
use super::handlers::{Handles, SessionRate};
use super::loop_body::streamer_future;
use super::metrics::Metrics;
#[cfg(any(test, feature = "testing"))]
use super::step::StepOutcome;
use super::step::StreamerCycle;
use crate::Error;
use crossbeam_channel::Receiver;
use tutti_rt::RingFloat;

/// A streamer that is built and drives nothing yet: its queue, its shared
/// handles and the state a driver will own.
///
/// Consumed by [`start`](Self::start) (a future on an executor) or `manual` (the
/// caller). Dropping it drops the queue's one receiver, so a sender is told the
/// streamer is gone.
pub(super) struct Built<F: RingFloat> {
    /// The command queue's sending end, cloned for every sender.
    tx: StreamerSender<F>,
    /// Its receiving end, until a driver takes it.
    rx: Receiver<StreamerCommand<F>>,
    shared: Handles<F>,
    /// The waiting end of the streamer's bell, for the future that runs the loop.
    listener: Listener,
    config: BufferConfig,
}

impl<F: RingFloat> Built<F> {
    /// Build the queue and the shared handles without spawning anything.
    ///
    /// `channel_capacity` bounds the command queue in messages (at least
    /// one); a send to a full queue waits. `sample_rate` is the
    /// *session* rate, against which each file's own rate becomes the stream's
    /// [`SrcRatio`](tutti_units::SrcRatio). `bells` makes the streamer's bell.
    pub(super) fn new(
        channel_capacity: usize,
        sample_rate: SampleRate,
        config: BufferConfig,
        seams: Seams,
        bells: &Bells,
    ) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(channel_capacity.max(1));
        #[cfg(not(test))]
        let _ = seams;
        let (bell, listener) = bells.bell();
        let tx = StreamerSender::new(tx, bell.clone());

        let shared = Handles {
            cache: Arc::new(LruCache::new(
                config.cache_max_entries,
                config.cache_max_bytes,
            )),
            metrics: Arc::new(Metrics::new()),
            bell,
            session_rate: SessionRate::new(sample_rate),
            #[cfg(test)]
            seams,
        };

        Self {
            tx,
            rx,
            shared,
            listener,
            config,
        }
    }

    /// A cloneable sender onto the streamer's command queue.
    ///
    /// Valid before a driver exists: commands sent then simply queue until it
    /// drains them. Control threads only: a send waits while the queue is
    /// full.
    pub(super) fn command_sender(&self) -> StreamerSender<F> {
        self.tx.clone()
    }

    /// The shared handles, for the control side's own reads.
    pub(super) fn shared(&self) -> Handles<F> {
        self.shared.clone()
    }

    /// Spawn the streamer future on the executor, which owns the cycle state and
    /// the queue's receiver from here on.
    ///
    /// # Errors
    ///
    /// [`Error::Spawn`] when the executor refuses the future (a threaded one that
    /// cannot start the thread). Every stream and every refill depends on it, so a
    /// failure here (the OS out of threads) would otherwise degrade into an engine
    /// that accepts commands and stays mute; it is the caller's to see instead.
    pub(super) fn start(self, executor: &Executor) -> Result<Running, Error> {
        let stop = Arc::new(AtomicBool::new(false));
        let bell = self.shared.bell.clone();
        let cycle = StreamerCycle::new(self.config, self.tx.clone());
        // The future owns the queue's one receiver, so however it ends (a panic
        // included) its drop tells every sender the streamer is gone instead of
        // letting them queue into nothing; and its table's drop tells every live
        // stream (`Faulted(StreamerGone)`).
        let future = streamer_future(
            self.rx,
            self.shared,
            cycle,
            self.listener,
            Arc::clone(&stop),
        );
        // Disk streaming should stay ahead of background work, not compete
        // with the audio threads (Ardour's streamer sets no CPU priority,
        // LinuxSampler's disk thread runs at nice -10): a raised, never
        // real-time priority, nice -10 on Linux and ABOVE_NORMAL on Windows. Only a
        // threaded executor applies it; a polled or host executor runs the
        // streamer at the caller's or the host's priority.
        executor
            .spawn_dedicated("tutti-streamer", Some(PriorityRequest::Raised), future)
            .map_err(Error::Spawn)?;
        Ok(Running { stop, bell })
    }

    #[cfg(any(test, feature = "testing"))]
    /// The caller drives: its cycle is stepped by `Manual::step`, and the decodes the
    /// executor holds run in the same step.
    pub(super) fn manual(self) -> Manual<F> {
        Manual {
            rx: Some(self.rx),
            cycle: StreamerCycle::new(self.config, self.tx),
            shared: self.shared,
        }
    }
}

/// The streamer driven as a future on an executor: what
/// [`DiskStreamer::start`](crate::DiskStreamer::start) builds.
///
/// `Drop` raises the stop flag and rings the bell; the future ends the next time the
/// executor polls it, and never blocks the dropper.
pub struct Running {
    /// Read by the future after each ring.
    stop: Arc<AtomicBool>,
    /// What a stop rings.
    bell: Bell,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running")
            .field("stopping", &self.stop.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Running {
    /// Asks the future to end, and wakes it to see it. Shutdown is one mechanism: a
    /// flag, and a ring. The future reads it after each ring, so a cycle already
    /// running finishes first. Its receiver is gone afterwards, so a sender still
    /// holding the queue, a waiting one included, is told the streamer is gone.
    /// Called by `Drop`, and idempotent.
    pub(super) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.bell.ring();
    }

    /// Rings the bell with no command: the streamer takes a cycle.
    pub(super) fn wake(&self) {
        self.bell.ring();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(any(test, feature = "testing"))]
/// The streamer driven by the caller, a cycle at a time: what
/// `DiskStreamer::manual` builds. Requires the `testing` feature.
///
/// There is no future to race, so a test can say "after this many cycles the ring
/// holds audio"; it is the same `StreamerCycle::step` the future runs.
pub struct Manual<F: RingFloat> {
    rx: Option<Receiver<StreamerCommand<F>>>,
    cycle: StreamerCycle<F>,
    shared: Handles<F>,
}

#[cfg(any(test, feature = "testing"))]
impl<F: RingFloat> std::fmt::Debug for Manual<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manual").finish_non_exhaustive()
    }
}

#[cfg(any(test, feature = "testing"))]
impl<F: RingFloat> Manual<F> {
    /// One cycle on the calling thread: drain every queued command, poll the
    /// decodes in flight, then apply directions and refills. Returns once that work
    /// is done, reporting what the future *would* have done next.
    pub(super) fn step(&mut self) -> StepOutcome {
        let queue = self.rx.as_ref();
        self.cycle
            .step(&self.shared, || queue.and_then(|rx| rx.try_recv().ok()))
    }

    /// How many streams the streamer holds (tests).
    #[cfg(test)]
    pub(super) fn open_streams(&self) -> usize {
        self.cycle.streams().len()
    }

    /// Drop the queue's receiver, as a streamer future that ended does: every
    /// sender is told the streamer is gone (tests).
    #[cfg(test)]
    pub(super) fn gone(&mut self) {
        self.rx = None;
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;
    use tutti_platform::Instant;

    use super::*;
    use crate::streamer::command::StreamId;
    use crate::streamer::stream::{PortClosed, StreamerPort};

    /// Both drivers are `Sync` (a host may hold a `DiskStreamer` as a shared resource), with
    /// the hand-stepped cycle held plainly: a compile-time check.
    const _: () = {
        fn sync<T: Sync>() {}
        let _ = sync::<Running>;
        let _ = sync::<Manual<f32>>;
    };

    fn built(capacity: usize, bells: &Bells) -> Built<f32> {
        Built::new(
            capacity,
            SampleRate::SR_48K,
            BufferConfig::default(),
            Seams::default(),
            bells,
        )
    }

    /// Polls `done` until it holds or `bound` passes; whether it held.
    fn within(bound: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < bound {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        done()
    }

    /// A sent command wakes the idle streamer future: it is drained, and nothing
    /// else would wake it (the future awaits its bell and has no timer).
    #[test]
    fn a_send_wakes_an_idle_streamer() {
        let bells = Bells::new().expect("a tick thread");
        let built = built(8, &bells);
        let tx = built.command_sender();
        let streamer = built.start(&Executor::threaded()).expect("the future starts");

        for stream in [StreamId::of(90), StreamId::of(91)] {
            tx.try_send(StreamerCommand::Close { stream })
                .expect("room in an empty queue");
            assert!(
                within(Duration::from_secs(10), || tx.is_empty()),
                "command {stream} was never drained"
            );
        }
        drop(streamer);
    }

    /// Dropping a streamer ends its future: the queue's receiver goes with it, so a
    /// sender is told the streamer is gone, and the drop itself never waits.
    #[test]
    fn a_dropped_streamer_is_gone_to_its_senders() {
        let bells = Bells::new().expect("a tick thread");
        let built = built(8, &bells);
        let tx = built.command_sender();
        let streamer = built.start(&Executor::threaded()).expect("the future starts");
        drop(streamer);
        // Probed without waiting, so a queue that never disconnects fails
        // at the bound rather than filling and blocking the probe.
        assert!(
            within(Duration::from_secs(10), || matches!(
                tx.try_send(StreamerCommand::Close {
                    stream: StreamId::of(1)
                }),
                Err(crossbeam_channel::TrySendError::Disconnected(_))
            )),
            "the future ended but the queue still accepts commands"
        );
        assert_eq!(
            StreamerPort::new(tx).send(StreamerCommand::Close {
                stream: StreamId::of(1)
            }),
            Err(PortClosed)
        );
    }

    /// A streamer dropped before it started is gone to every sender.
    #[test]
    fn a_streamer_dropped_before_it_started_is_gone() {
        let bells = Bells::new().expect("a tick thread");
        let unstarted = built(8, &bells);
        let port = StreamerPort::new(unstarted.command_sender());
        drop(unstarted);
        assert_eq!(
            port.send(StreamerCommand::Close {
                stream: StreamId::of(1)
            }),
            Err(PortClosed),
        );
    }

    /// A send waiting on a full queue returns once the streamer is gone,
    /// reporting it gone, rather than waiting for good.
    #[test]
    fn a_waiting_send_is_released_when_the_streamer_is_gone() {
        let bells = Bells::new().expect("a tick thread");
        let streamer = built(1, &bells);
        let port = StreamerPort::new(streamer.command_sender());
        assert_eq!(
            port.send(StreamerCommand::Close {
                stream: StreamId::of(1)
            }),
            Ok(())
        );
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = {
            let port = port.clone();
            thread::spawn(move || {
                let _ = done_tx.send(port.send(StreamerCommand::Close {
                    stream: StreamId::of(1),
                }));
            })
        };
        // The queue is full and nothing drains it: the send is waiting.
        thread::sleep(Duration::from_millis(20));
        assert!(
            done_rx.try_recv().is_err(),
            "a send into a full queue waits"
        );
        drop(streamer);
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(10)),
            Ok(Err(PortClosed)),
            "the waiting send must return once the streamer is gone, and say so"
        );
        waiter.join().unwrap();
    }
}
