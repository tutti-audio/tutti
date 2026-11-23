//! The editor's end of the rings to its executor.

use std::sync::Arc;

use crate::edges::tap::TapEnd;
use tutti_units::At;

use super::CommitError;
use crate::exec::command::{CommandId, CommandTx, ScheduleError, Scheduled};
use crate::exec::{Channels, Commit, Crossfade, FADE_CAPACITY, QUEUE_CAPACITY};
use crate::model::gain::EdgeFade;
use crate::model::spec::EventInputPort;
use crate::{EdgeId, Executor};
use tutti_buffers::Float;
use tutti_rt::PushError;

/// The control end of the rings to the paired executor, and the counts that
/// keep every push within them.
///
/// At most [`QUEUE_CAPACITY`] commits are out (sent and not yet handed back),
/// and the crossfades they start hold at most [`FADE_CAPACITY`] slots on the
/// fade-return ring, so a push this link makes after its check never fails.
pub(super) struct Link<F: Float> {
    channels: Channels<F>,
    commands: CommandTx,
    /// Commits sent and not yet handed back.
    out: usize,
    /// Crossfades sent (started or waiting) and not yet handed back: the
    /// fade-return ring's slots in use.
    fades_out: usize,
    /// Commits sent, ever: the sequence number of the last one.
    sent: u64,
    /// Commits handed back so far: the executor applied each one before it
    /// did, and every commit is handed back once.
    acked: u64,
    /// Taps left out of the plan, each with the commit (by `sent`) that
    /// leaves them out once one was sent; their latch is raised once the
    /// executor has applied it.
    ending_taps: Vec<(Option<u64>, Arc<TapEnd>)>,
    /// Items the executor's apply could not return, summed from each list as
    /// its box or crossfade came back.
    leaked: u64,
    /// What the executor's `unreported` counter read at the last
    /// [`take_unreported`](Self::take_unreported).
    unreported_seen: u64,
}

impl<F: Float> Link<F> {
    pub(super) fn new(channels: Channels<F>, commands: CommandTx) -> Self {
        Self {
            channels,
            commands,
            out: 0,
            fades_out: 0,
            sent: 0,
            acked: 0,
            ending_taps: Vec::new(),
            leaked: 0,
            unreported_seen: 0,
        }
    }

    /// `Backpressure` when [`QUEUE_CAPACITY`] commits are out.
    pub(super) fn check_room(&self) -> Result<(), CommitError> {
        if self.out >= QUEUE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        Ok(())
    }

    /// `Backpressure` when `fades` more crossfades would take more than
    /// [`FADE_CAPACITY`] slots on the fade-return ring.
    pub(super) fn check_fade_room(&self, fades: usize) -> Result<(), CommitError> {
        if self.fades_out + fades > FADE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        Ok(())
    }

    /// The `seq` of the newest commit whose main output crossfade the executor has
    /// rendered through, 0 before any.
    pub(super) fn main_output_fade_ended(&self) -> u64 {
        self.channels.main_output_fade_ended.get().0
    }

    /// The sequence number the next commit sends with.
    pub(super) fn next_seq(&self) -> u64 {
        self.sent + 1
    }

    /// Sends `commit`, which starts `fades` crossfades: the caller checked
    /// [`check_room`](Self::check_room) and
    /// [`check_fade_room`](Self::check_fade_room). A tap ending since the
    /// last commit is left out of this one.
    pub(super) fn send(&mut self, commit: Box<Commit<F>>, fades: usize) {
        self.fades_out += fades;
        debug_assert!(self.fades_out <= FADE_CAPACITY);
        self.push(commit);
        let sent = self.sent;
        for (at, _) in &mut self.ending_taps {
            at.get_or_insert(sent);
        }
    }

    /// Sends a re-prepare's first half: the caller checked
    /// [`check_room`](Self::check_room).
    pub(super) fn send_suspend(&mut self, suspend: Box<Commit<F>>) {
        self.push(suspend);
    }

    fn push(&mut self, commit: Box<Commit<F>>) {
        // Full cannot happen: the credit count keeps the queue below capacity.
        // With the executor gone nothing applies the commit, and it counts as
        // sent all the same, so the credits run out as they would behind a
        // stalled one.
        if let Err(refused) = self.channels.to_executor.push(commit) {
            assert_eq!(
                refused.error,
                PushError::Disconnected,
                "the queue has a free slot for every credit"
            );
        }
        self.sent += 1;
        self.out += 1;
    }

    /// The next commit the executor handed back, applied.
    pub(super) fn pop_returned(&mut self) -> Option<Box<Commit<F>>> {
        let done = self.channels.returned.pop()?;
        self.out -= 1;
        self.acked += 1;
        self.leaked = self.leaked.saturating_add(done.refused() as u64);
        Some(done)
    }

    /// Raises the latch of every ending tap whose last commit the executor
    /// has applied. The ring that carried the commit back orders the tap's
    /// last writes before this raise.
    pub(super) fn end_applied_taps(&mut self) {
        let acked = self.acked;
        self.ending_taps.retain(|(at, end)| match at {
            Some(sent) if *sent <= acked => {
                end.raise();
                false
            }
            _ => true,
        });
    }

    /// Ends a tap the executor holds the writer of: its latch is raised once
    /// the next commit, which leaves it out, is applied.
    pub(super) fn end_tap(&mut self, end: Arc<TapEnd>) {
        self.ending_taps.push((None, end));
    }

    /// The next crossfade the executor handed back.
    pub(super) fn pop_faded(&mut self) -> Option<Box<Crossfade<F>>> {
        let x = self.channels.faded.pop()?;
        self.fades_out -= 1;
        self.leaked = self.leaked.saturating_add(x.refused() as u64);
        Some(x)
    }

    /// The next edge whose declick fade ended, by the kind that did.
    pub(super) fn pop_edge_fade(&mut self) -> Option<(EdgeId, EdgeFade)> {
        self.channels.edge_fades.pop()
    }

    /// The next port that lost events.
    pub(super) fn pop_loss(&mut self) -> Option<crate::EventLoss> {
        self.channels.losses.pop()
    }

    /// Refusals since the last call that no loss names.
    pub(super) fn take_unreported(&mut self) -> u64 {
        self.channels
            .unreported
            .take_since(&mut self.unreported_seen)
    }

    /// Items the executor's apply leaked, since the editor was built.
    pub(super) fn leaked(&self) -> u64 {
        self.leaked
    }

    /// The executor's play clock, as it last published it.
    pub(super) fn play_reading(&self) -> crate::PlayReading {
        self.channels.play.read()
    }

    /// Commits sent and not yet handed back.
    pub(super) fn in_flight(&self) -> usize {
        self.out
    }

    /// Commits the executor has applied: handed back, every one.
    pub(super) fn acked(&self) -> u64 {
        self.acked
    }

    /// Crossfades sent and not yet handed back.
    pub(super) fn fades_in_flight(&self) -> usize {
        self.fades_out
    }

    /// Whether `executor` holds the other end of these rings.
    pub(super) fn is_paired_with(&self, executor: &Executor<F>) -> bool {
        self.commands.same_pair(executor.commands())
    }

    /// Sends one checked command, stamped with the last commit sent.
    pub(super) fn schedule(
        &mut self,
        at: At,
        to: EventInputPort,
        payload: Scheduled,
    ) -> Result<CommandId, ScheduleError> {
        self.commands.send(self.sent, at, to, payload)
    }

    /// Takes back command `id`, if it has not landed.
    pub(super) fn cancel(&mut self, id: CommandId) -> Result<(), ScheduleError> {
        self.commands.cancel(id)
    }

    /// Takes back every command that has not landed.
    pub(super) fn cancel_all(&mut self) -> Result<(), ScheduleError> {
        self.commands.cancel_all()
    }

    /// Scheduled commands sent and not yet delivered.
    pub(super) fn commands_outstanding(&self) -> usize {
        self.commands.outstanding() as usize
    }
}

impl<F: Float> std::fmt::Debug for Link<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("out", &self.out)
            .field("fades_out", &self.fades_out)
            .field("sent", &self.sent)
            .field("acked", &self.acked)
            .finish_non_exhaustive()
    }
}
