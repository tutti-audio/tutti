//! The host's parameter writes to an in-process plugin: [`host_edits`].
//!
//! The session and automation write through the plugin's handle
//! ([`AnyThread::set_parameter_value`](tutti_plugin_types::AnyThread::set_parameter_value)),
//! from any thread, with no instance lock: the backend's [`EditPost`] stores a
//! write in the slot of its parameter. A format's own slots
//! ([`edit_slots`](tutti_plugin_types::edit_slots())) carry the writes made
//! through its `Control` on the main thread (its editor, its presets); the two
//! are different writers.
//!
//! **One reader, claimed.** The slots have one reader, in one cell shared by
//! every node of the plugin, so a rebind's successor drains the same slots. The
//! node, at the head of each block, is its only taker: [`EditPort::drain`] claims
//! the cell without waiting, and a refused claim (a load holds it) drains on the
//! next block, losing nothing. A write is applied once the node's call into the
//! plugin that took it has returned ([`EditPort::publish_applied`]).
//!
//! **Pending, not waited for.** A read, a save or a fork never waits for a
//! block: it states the writes no block has applied ([`EditPost::pending`], from
//! the writer's own record) beside its result, and a load replays them.
//! A format's Processor applies what its own slots hold ahead of a block's
//! parameter changes, so a write made through the plugin's `Control` is
//! overtaken by a later host write.
//!
//! **A write wakes the node.** A node the executor parked is not called with
//! no input, and a host write is not an input of its ports, so
//! [`EditPost::write`] raises the node's [`Wake`](tutti_graph::Wake) after the
//! slot holds the value: the block the wake calls finds it.
//!
//! **An epoch is stamped on what a drain keeps.** A state or preset load makes
//! every write before it stale ([`EditPost::reset`]); a write already drained
//! into a chunk that has not gone, or owed to the next, carries the epoch it
//! was drained in and is dropped when the reader's epoch has moved past it
//! ([`EditPort::epoch`]).
//!
//! The writes the plugin's edit controller has not heard of wait in a list,
//! which a main-thread job tells the controller
//! ([`Control::inform_controller`]): one value per parameter, the newest.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

#[cfg(test)]
use tutti_plugin_types::fake::at_frame;
use tutti_plugin_types::{EditReader, EditWriter, UnknownParam, edit_slots};
use tutti_rt::AudioThreadCell;
use tutti_rt::Mutex;
use tutti_units::{Normalized, ParamId};

/// The slots for `ids`: the backend's post and the first node's port.
pub(super) fn host_edits(ids: impl IntoIterator<Item = ParamId>) -> (EditPost, EditPort) {
    let (writer, reader) = edit_slots(ids);
    let shared = Arc::new(Shared {
        state: Mutex::new(PostState {
            writer,
            to_inform: Vec::new(),
            scheduled: false,
        }),
        epoch: reader.epoch_watch(),
        reader: AudioThreadCell::new(reader),
        wake: Mutex::new(None),
        restoring: AtomicBool::new(false),
    });
    (EditPost(Arc::clone(&shared)), EditPort(shared))
}

/// What the post and the ports of one plugin share.
struct Shared {
    state: Mutex<PostState>,
    /// The one reader of the slots: claimed by whoever drains.
    reader: AudioThreadCell<EditReader>,
    /// The slots' epoch, read without the reader: a node whose claim is refused
    /// still learns of a reset.
    epoch: tutti_rt::EpochWatch,
    /// The wake of the node the writes are for, bound when the node is.
    wake: Mutex<Option<tutti_graph::Wake>>,
    /// A state or preset load is running: a drain yields nothing until it ends.
    restoring: AtomicBool,
}

struct PostState {
    writer: EditWriter,
    /// The newest write of each parameter the edit controller was not told of.
    to_inform: Vec<(ParamId, Normalized)>,
    /// A main-thread job to tell the controller is queued.
    scheduled: bool,
}

/// The control side: any thread posts, one at a time. It is not for the audio
/// thread: it takes a lock.
#[derive(Clone)]
pub(super) struct EditPost(Arc<Shared>);

impl std::fmt::Debug for EditPost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditPost").finish_non_exhaustive()
    }
}

impl EditPost {
    /// Stores `value` in the slot of `id`.
    ///
    /// # Errors
    ///
    /// [`UnknownParam`] when the plugin did not declare `id`.
    pub(super) fn write(&self, id: ParamId, value: Normalized) -> Result<(), UnknownParam> {
        let mut state = self.0.state.lock();
        state.writer.write(id, value)?;
        match state.to_inform.iter_mut().find(|(known, _)| *known == id) {
            Some(entry) => entry.1 = value,
            None => state.to_inform.push((id, value)),
        }
        // After the slot holds the value: a block between the two would take
        // the wake and find nothing.
        if let Some(wake) = &*self.0.wake.lock() {
            wake.notify();
        }
        Ok(())
    }

    /// The writes no block has applied: the newest value of each parameter
    /// ([`EditWriter::pending`]), read without asking the node.
    pub(super) fn pending(&self) -> Vec<(ParamId, Normalized)> {
        self.0.state.lock().writer.pending()
    }

    /// Makes every write made so far stale ([`EditWriter::reset`]), and forgets
    /// the ones the controller was not told of.
    pub(super) fn reset(&self) {
        let mut state = self.0.state.lock();
        state.writer.reset();
        state.to_inform.clear();
    }

    /// A state or preset load begins: every write made so far is stale
    /// ([`reset`](Self::reset)), and the writes made until the returned guard
    /// drops are held in their slots, so they apply after the load and not
    /// before the plugin's own `set_state`.
    ///
    /// Takes the reader's claim to do it, so no drain straddles the reset:
    /// `Pending` while a node holds the claim for a drain, and the caller asks
    /// again. A drain that finds the load flag set yields nothing.
    pub(super) fn restoring(&self) -> Poll<Restoring> {
        let Some(reader) = self.0.reader.try_borrow_mut() else {
            return Poll::Pending;
        };
        // Under the claim, whose release publishes the flag to every later
        // drain: a drain reads it only once it holds the claim.
        self.0.restoring.store(true, Ordering::Relaxed);
        self.reset();
        drop(reader);
        Poll::Ready(Restoring(Arc::clone(&self.0)))
    }

    /// Whether a main-thread job should be queued to tell the controller: there
    /// is something to tell and no job is queued. Takes the claim on the job.
    pub(super) fn claim_job(&self) -> bool {
        let mut state = self.0.state.lock();
        if state.scheduled || state.to_inform.is_empty() {
            return false;
        }
        state.scheduled = true;
        true
    }

    /// A claim on the job that cannot be queued (no main thread to run it) is
    /// given back.
    pub(super) fn release_job(&self) {
        self.0.state.lock().scheduled = false;
    }

    /// What a job could not tell (the instance was busy) is owed again: a
    /// value written meanwhile is newer, and stays.
    pub(super) fn give_back(&self, told: Vec<(ParamId, Normalized)>) {
        let mut state = self.0.state.lock();
        for (id, value) in told {
            if !state.to_inform.iter().any(|(known, _)| *known == id) {
                state.to_inform.push((id, value));
            }
        }
    }

    /// The job runs: what it tells the controller, and its claim is released.
    pub(super) fn take_inform(&self) -> Vec<(ParamId, Normalized)> {
        let mut state = self.0.state.lock();
        state.scheduled = false;
        std::mem::take(&mut state.to_inform)
    }
}

/// A load in progress ([`EditPost::restoring`]); the writes held end with it.
pub(super) struct Restoring(Arc<Shared>);

impl Drop for Restoring {
    fn drop(&mut self) {
        self.0.restoring.store(false, Ordering::Release);
    }
}

/// The audio side: the node's claim on the reader. Wait-free, and holds no lock.
#[derive(Clone)]
pub(super) struct EditPort(Arc<Shared>);

impl EditPort {
    /// Hands `apply` each parameter written since the last drain, with its
    /// latest value and the epoch the drain observed. A refused claim or a load
    /// in progress hands nothing: the slots keep what they hold for the next
    /// block.
    pub(super) fn drain(&self, apply: impl FnMut(ParamId, Normalized, u32)) {
        if let Some(mut reader) = self.0.reader.try_borrow_mut()
            && !self.0.restoring.load(Ordering::Acquire)
        {
            reader.drain_in_epoch(apply);
        }
    }

    /// Publishes that the plugin's call that took the writes drained so far has
    /// returned ([`EditReader::publish_applied`]): the writes are no longer
    /// pending. Called by the node after its `process` ran, never at drain time;
    /// a refused claim leaves it to the next block. Wait-free.
    pub(super) fn publish_applied(&self) {
        if let Some(mut reader) = self.0.reader.try_borrow_mut() {
            reader.publish_applied();
        }
    }

    /// The slots' epoch now, whoever holds the reader: what a write drained
    /// earlier is compared with before it is sent. Wait-free.
    pub(super) fn epoch(&self) -> u32 {
        self.0.epoch.get()
    }

    /// Binds the wake a write raises: the node's, as it is bound.
    pub(super) fn bind_wake(&self, wake: tutti_graph::Wake) {
        *self.0.wake.lock() = Some(wake);
    }

    /// The control side's end of the same slots.
    pub(super) fn post(&self) -> EditPost {
        EditPost(Arc::clone(&self.0))
    }
}

impl std::fmt::Debug for EditPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditPort").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> ParamId {
        ParamId::new(n)
    }

    fn drained(port: &EditPort) -> Vec<(ParamId, f64)> {
        let mut out = Vec::new();
        port.drain(|id, value, _| out.push((id, value.get())));
        out
    }

    /// **Two ports of one plugin share the slots, and a write is given up
    /// once**: a successor node drains what the node it replaces had not, and
    /// not what it had.
    #[test]
    fn two_nodes_of_one_plugin_share_one_reader() {
        let (post, first) = host_edits([id(1), id(2)]);
        let second = first.clone();
        post.write(id(1), Normalized::of(0.25)).unwrap();
        assert_eq!(drained(&first), [(id(1), 0.25)]);
        post.write(id(2), Normalized::of(0.5)).unwrap();
        assert_eq!(drained(&second), [(id(2), 0.5)]);
        assert_eq!(drained(&first), []);
    }

    /// **A refused claim loses nothing**: while the control side holds the
    /// reader a drain yields nothing, and the next one yields the write.
    #[test]
    fn a_drain_refused_the_claim_leaves_the_write_for_the_next_block() {
        let (post, port) = host_edits([id(1)]);
        post.write(id(1), Normalized::of(0.75)).unwrap();
        let held = post.0.reader.try_borrow_mut().expect("free");
        assert_eq!(drained(&port), [], "the control side holds the reader");
        drop(held);
        assert_eq!(drained(&port), [(id(1), 0.75)]);
    }

    /// **A write is pending from the call until the node's call into the plugin
    /// has returned, whatever a drain took in between, and a reset ends it.**
    /// No block is waited for: the post answers from its own record.
    #[test]
    fn a_write_is_pending_until_the_nodes_call_returned() {
        let (post, port) = host_edits([id(1), id(2)]);
        assert!(post.pending().is_empty());
        post.write(id(1), Normalized::of(0.25)).unwrap();
        post.write(id(1), Normalized::of(0.5)).unwrap();
        post.write(id(2), Normalized::of(0.75)).unwrap();
        assert_eq!(
            post.pending(),
            [(id(1), Normalized::of(0.5)), (id(2), Normalized::of(0.75))],
            "the newest of each"
        );
        assert_eq!(drained(&port).len(), 2);
        assert_eq!(post.pending().len(), 2, "taken by a drain, not yet applied");
        port.publish_applied();
        assert!(post.pending().is_empty(), "the call returned");
        post.write(id(2), Normalized::of(0.125)).unwrap();
        post.reset();
        assert!(
            post.pending().is_empty(),
            "a load replaced what it was written against"
        );
    }

    /// **A write made while a load runs is applied after it, and one made
    /// before it is not.** A node's drain during the load yields nothing; the
    /// first drain after it yields only the write the load did not predate.
    #[test]
    fn a_write_made_during_a_load_waits_for_its_end() {
        let (post, port) = host_edits([id(1), id(2)]);
        post.write(id(1), Normalized::of(0.25)).unwrap();
        let Poll::Ready(restoring) = post.restoring() else {
            panic!("no node holds the reader");
        };
        post.write(id(2), Normalized::of(0.5)).unwrap();
        assert_eq!(drained(&port), [], "the load is running");
        drop(restoring);
        assert_eq!(
            drained(&port),
            [(id(2), 0.5)],
            "the write made before the load is stale, the one made during it applies"
        );
    }

    /// **No drain straddles a load's reset.** A node that holds the reader
    /// across the gate keeps the load `Pending`; once it lets go the load
    /// begins, and a write made after that is not applied before the load ends.
    #[test]
    fn a_load_waits_for_a_drain_that_holds_the_reader() {
        let (post, port) = host_edits([id(1)]);
        post.write(id(1), Normalized::of(0.25)).unwrap();
        let drain = post.0.reader.try_borrow_mut().expect("free");
        assert!(post.restoring().is_pending(), "a drain is in progress");
        assert!(
            !post.0.restoring.load(Ordering::Acquire),
            "the load has not begun"
        );
        drop(drain);
        let Poll::Ready(restoring) = post.restoring() else {
            panic!("the drain ended");
        };
        post.write(id(1), Normalized::of(0.75)).unwrap();
        assert_eq!(drained(&port), [], "held until the load ends");
        drop(restoring);
        assert_eq!(drained(&port), [(id(1), 0.75)]);
    }

    /// **A node whose claim is refused still voids a stale write**: the epoch is
    /// read from the word the reset bumps, not through the reader the control
    /// side holds, so a write drained into a chunk before a load is dropped even
    /// in a block that cannot claim the reader.
    #[test]
    fn a_stale_write_is_voided_while_the_control_side_holds_the_reader() {
        use crate::{DeliveryCounters, ParamDelivery};
        let (post, port) = host_edits([id(1)]);
        let mut params = ParamDelivery::new([id(1)], Arc::<DeliveryCounters>::default());
        params.prepare(64);
        params.begin();
        post.write(id(1), Normalized::of(0.75)).unwrap();
        port.drain(|param, value, epoch| {
            params.write(param, at_frame(0), value.get(), epoch);
        });
        post.reset();
        let held = post.0.reader.try_borrow_mut().expect("free");
        super::super::client::void_stale_edits(Some(&port), &mut params);
        drop(held);
        assert!(
            params.chunk().queues.iter().all(|q| q.points.is_empty()),
            "the stale write was left in the chunk"
        );
    }

    /// **A reset moves the epoch past what a drain observed**, and the writes
    /// before it are not yielded again.
    #[test]
    fn a_reset_voids_what_a_drain_kept() {
        let (post, port) = host_edits([id(1)]);
        post.write(id(1), Normalized::of(0.25)).unwrap();
        let mut observed = None;
        port.drain(|_, _, epoch| observed = Some(epoch));
        let observed = observed.expect("drained");
        post.reset();
        assert!(tutti_plugin_types::epoch_before(observed, port.epoch()));
    }
}
