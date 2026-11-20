//! One streamer cycle, synchronously.
//!
//! The streamer's work is a loop over one step after its commands: refill the
//! rings (following each reader to where it plays). It is not asynchronous: it
//! walks the stream table, decodes from a file, and writes into a ring. What
//! the thread adds is only the sleeping between cycles, on the
//! [`Bell`](super::bell::Bell). (What a *command* does to the table is the pure
//! [`step`](super::record::step), not this.)
//!
//! This module is that work with the sleeping removed. [`StreamerCycle::step`]
//! drains the command queue, runs the refill, and reports what the loop should
//! do next as a [`StepOutcome`]. The loop in [`loop_body`](super::loop_body) is
//! then the sleeping layer and nothing else.
//!
//! # Why the split earns its name
//!
//! A test that has to observe a streamer-driven stream otherwise has only wall
//! clock to work with: send a command, sleep, look, sleep again. That is not a
//! stylistic complaint: every such test carries a timeout that is really a
//! guess about the machine it runs on. Driving the same steps directly makes
//! the question "has the streamer done the work" answerable by *counting*
//! rather than by waiting: `step` returns only after the work is done.
//!
//! It is the same seam the production loop uses, not a parallel test path:
//! `streamer_loop` is written in terms of `StreamerCycle::step`, so a
//! divergence between what tests drive and what ships is not representable.

use super::StreamerSender;
use super::command::StreamerCommand;
use super::config::BufferConfig;
use super::handlers::{Handles, Local, follow_steering, handle_command};
use super::io::refill::{FILL_THRESHOLD, MIN_CHUNK, refill_all, refill_all_parallel};
use super::record::{Record, Streams};
use tutti_rt::RingFloat;

/// What the loop should do after one streamer cycle.
///
/// "Keep going" and "there is nothing to do until rung" must be
/// distinguishable: spinning either one costs a core. The step has no opinion
/// on *how long* to sleep (the loop sleeps until a source rings the bell, or
/// the backstop passes), only on whether sleeping is safe, which is why this
/// carries no duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepOutcome {
    /// No stream is live: none is open, or each is still opening or faulted.
    /// Nothing to refill; sleep.
    Idle,
    /// Every live ring is at or above its refill threshold. The work ran, and
    /// none of it was urgent: sleep.
    Healthy,
    /// At least one ring is below its refill threshold and this cycle moved
    /// frames into one. Come straight back; there is more to do.
    Busy,
    /// A ring is below its refill threshold and this cycle could put nothing
    /// into it.
    ///
    /// The threshold is a fraction of ring *capacity*, and capacity is sized
    /// from the whole file, so a stream reading its last seconds can never
    /// reach it however many cycles run. Reporting that as `Busy` makes the
    /// loop skip its sleep and re-run immediately, which spins a thread flat
    /// out for the whole tail of every clip. (A voice does not ring for it
    /// either: `needs_refill` is the one
    /// predicate, and it calls a window that holds all the mapping writes
    /// satisfied.)
    ///
    /// It is also what a step-driven caller needs: "the streamer has done all it
    /// can" is a *fixed point*, and a caller that waited for `Healthy` instead
    /// would wait forever near the end of a file.
    Stalled,
}

/// The streamer's per-cycle state and the step over it.
///
/// Owns [`Local`] (streamer-thread-local: the stream table, scratch) and
/// borrows nothing. Both the thread's loop and a test driver build one
/// and call [`step`](Self::step) repeatedly; the only difference between them is
/// what happens *between* the calls.
pub(crate) struct StreamerCycle<F: RingFloat> {
    local: Local<F>,
    config: BufferConfig,
}

impl<F: RingFloat> StreamerCycle<F> {
    /// The stream table (tests).
    #[cfg(test)]
    pub(crate) fn streams(&self) -> &Streams<F> {
        &self.local.streams
    }

    /// A cycle whose scratch is pre-sized for one `config.chunk_size` refill.
    ///
    /// `reports` is the streamer's own queue, which its decode thread answers
    /// on.
    pub(crate) fn new(config: BufferConfig, reports: StreamerSender<F>) -> Self {
        Self {
            local: Local::new(config.chunk_size, reports),
            config,
        }
    }

    /// Cycles until one reports anything but [`Busy`](StepOutcome::Busy), at most `budget`
    /// of them; the last outcome. The first cycle runs alone, so a command that
    /// opens a ring is applied before `budget` is spent on it.
    pub(crate) fn step_until_idle(
        &mut self,
        shared: &Handles<F>,
        mut drain: impl FnMut() -> Option<StreamerCommand<F>>,
        budget: usize,
    ) -> StepOutcome {
        let mut outcome = self.step(shared, &mut drain);
        for _ in 1..budget {
            if outcome != StepOutcome::Busy {
                break;
            }
            outcome = self.step(shared, &mut drain);
        }
        outcome
    }

    /// How many cycles refill every ring once from empty: each cycle fills a ring
    /// by at least [`MIN_CHUNK`] frames.
    pub(crate) fn fill_steps(&self) -> usize {
        let rings = self
            .local
            .streams
            .records()
            .iter()
            .map(|record| record.out.frames().div_ceil(MIN_CHUNK))
            .max()
            .unwrap_or(0);
        rings + 2
    }

    /// Run one cycle: drain every queued command, free every ring's retired
    /// controls, then — when any channel is streaming — refill the rings.
    ///
    /// A loop or direction change is taken before the refill, so the refill
    /// writes the new mapping this cycle.
    ///
    /// `drain` is called until it yields `None`; commands are applied
    /// non-blockingly, so a step never waits for one to arrive.
    pub(crate) fn step(
        &mut self,
        shared: &Handles<F>,
        mut drain: impl FnMut() -> Option<StreamerCommand<F>>,
    ) -> StepOutcome {
        while let Some(cmd) = drain() {
            handle_command(cmd, shared, &self.config, &mut self.local);
        }
        // One chunk of every whole-file decode in flight. Its answer is applied by
        // the next cycle's drain, so while any runs this cycle reports more to do.
        let decoding = self.local.lane.poll();
        let outcome = self.refill(shared);
        if decoding { StepOutcome::Busy } else { outcome }
    }

    fn refill(&mut self, shared: &Handles<F>) -> StepOutcome {
        // The loop and direction each live stream's control side asks for, so
        // the refill below writes the new mapping this cycle.
        follow_steering(&self.config, &mut self.local);

        // Every ring's retired controls, whether or not this cycle publishes
        // one: the streamer is their only publisher, and its cycle is the
        // control-side tick they get. A lock and a length check per ring
        // when nothing is retired.
        for record in self.local.streams.records() {
            record.out.ring().reclaim_control();
        }

        // Idle is "no stream is live", not "no stream is open": a table
        // holding only an opening or a faulted record has nothing to refill.
        if !self.local.streams.records().iter().any(Record::is_live) {
            return StepOutcome::Idle;
        }

        // Frames resident across every streaming ring, sampled before the
        // refill so the outcome can say whether the refill actually achieved
        // anything. See [`StepOutcome::Stalled`].
        let buffered_before = total_buffered(&self.local.streams);

        if self.config.parallel_io && self.local.streams.len() >= 3 {
            refill_all_parallel(
                self.local.streams.records_mut(),
                &shared.metrics,
                self.config.chunk_size,
            );
        } else {
            refill_all(
                self.local.streams.records_mut(),
                &shared.metrics,
                self.config.chunk_size,
                &mut self.local.interleave_buffer,
            );
        }

        self.local.streams.reap();

        if buffers_healthy(&self.local.streams) {
            StepOutcome::Healthy
        } else if total_buffered(&self.local.streams) > buffered_before {
            StepOutcome::Busy
        } else {
            StepOutcome::Stalled
        }
    }
}

/// Frames currently resident across every live stream's ring.
///
/// Summed rather than compared per stream because the question it answers is
/// about the cycle as a whole: did *any* refill land? A per-stream comparison
/// would report a stall for a stream at EOF even while its neighbours were
/// still filling, and the loop must keep running for those.
fn total_buffered<F: RingFloat>(streams: &Streams<F>) -> usize {
    streams
        .records()
        .iter()
        .filter(|r| r.is_live())
        .map(|r| r.out.buffered().get())
        .sum()
}

/// True when no stream needs a refill: the loop can safely sleep until rung
/// instead of spinning.
fn buffers_healthy<F: RingFloat>(streams: &Streams<F>) -> bool {
    // Every live stream must be at or above its refill threshold.
    streams
        .records()
        .iter()
        .filter(|r| r.is_live())
        .all(|r| r.fill() >= FILL_THRESHOLD)
}

#[cfg(test)]
mod tests {
    use crate::testing::loop_frames;
    use std::path::PathBuf;

    use tutti_units::SampleRate;

    use super::super::command::StreamId;
    use super::super::loops::{Arrangement, RingMap};
    use super::super::record::{Cmd, step};
    use super::*;

    /// `Idle` means no stream is **live**, not that no stream is open: a table
    /// holding only a stream that is still opening has nothing to refill, so
    /// the loop sleeps. Once the stream is answered
    /// (`Cmd::Opened`) it is live and the cycle refills it.
    #[test]
    fn a_cycle_is_idle_until_a_stream_is_live() {
        let config = BufferConfig::default();
        let shared = Handles::detached(&config, SampleRate::SR_48K);
        let (reports, _queue) = crossbeam_channel::bounded(8);
        let mut cycle = StreamerCycle::new(config, StreamerSender::from(reports));
        assert_eq!(cycle.step(&shared, || None), StepOutcome::Idle, "no stream");

        let id = StreamId::of(1);
        let (record, _ring, _pos) =
            super::super::record::tests::record(id, PathBuf::from("t.wav"), 1024, 1u16);
        let _ = step(&mut cycle.local.streams, Cmd::Open(Box::new(record)));
        assert_eq!(
            cycle.step(&shared, || None),
            StepOutcome::Idle,
            "a stream still opening is not live"
        );

        let _ = step(&mut cycle.local.streams, Cmd::Opened(id));
        assert_ne!(
            cycle.step(&shared, || None),
            StepOutcome::Idle,
            "a live stream is refilled"
        );
    }

    /// A `RingControl` the reader held across the streamer's last publish is
    /// freed by the streamer's next cycle, with no further publish, whether or
    /// not a stream is live (this one's reader was never delivered).
    #[test]
    fn a_cycle_frees_a_control_the_reader_held_across_its_replacement() {
        let config = BufferConfig::default();
        let shared = Handles::detached(&config, SampleRate::SR_48K);
        let (reports, _queue) = crossbeam_channel::bounded(8);
        let reports = StreamerSender::from(reports);
        let mut cycle = StreamerCycle::new(config, reports);
        let id = StreamId::of(1);
        let (record, ring, _pos) =
            super::super::record::tests::record(id, PathBuf::from("t.wav"), 1024, 1u16);
        let _ = step(&mut cycle.local.streams, Cmd::Open(Box::new(record)));

        let mut control = ring.control_reader();
        {
            // A block in progress across the publish.
            let _held = control.lend();
            ring.publish_map(RingMap::plain(
                Arrangement::plain(tutti_units::Samples::new(64)),
                1,
            ));
        }
        assert_eq!(ring.retired_controls(), 1, "pinned until a scan");
        // The stream is still `Opening` (its reader never delivered), so the
        // cycle refills nothing, but it still sweeps the ring's controls.
        let _ = cycle.step(&shared, || None);
        assert_eq!(ring.retired_controls(), 0, "freed by the cycle");
    }

    /// The same sweep frees a `RingControl` held across a *loop* publish
    /// (the applied loop replaces the control without a new map): the value
    /// the reader held is retired by the replacement and freed by the next
    /// cycle, with nothing published since.
    #[test]
    fn a_cycle_frees_a_control_the_reader_held_across_a_loop_publish() {
        let config = BufferConfig::default();
        let shared = Handles::detached(&config, SampleRate::SR_48K);
        let (reports, _queue) = crossbeam_channel::bounded(8);
        let reports = StreamerSender::from(reports);
        let mut cycle = StreamerCycle::new(config, reports);
        let (record, ring, _pos) = super::super::record::tests::record(
            StreamId::of(1),
            PathBuf::from("t.wav"),
            1024,
            1u16,
        );
        let _ = step(&mut cycle.local.streams, Cmd::Open(Box::new(record)));

        let mut control = ring.control_reader();
        let applied = Some(loop_frames(0, 64, 0));
        {
            let held = control.lend();
            assert_eq!(held.loop_, None);
            ring.publish_loop(applied);
        }
        assert_eq!(ring.applied().loop_, applied, "the loop is published");
        assert_eq!(ring.retired_controls(), 1, "pinned until a scan");
        let _ = cycle.step(&shared, || None);
        assert_eq!(ring.retired_controls(), 0, "freed by the cycle");
        assert_eq!(control.lend().loop_, applied, "the reader sees the new one");
    }
}
