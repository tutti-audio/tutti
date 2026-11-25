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
//! every node of the plugin, so a rebind's successor drains the same slots. Two
//! takers consume them, and each write goes to exactly one:
//!
//! - the node, at the head of each block: [`EditPort::drain`] claims the cell
//!   without waiting, and a refused claim (the control side holds it) drains on
//!   the next block, losing nothing;
//! - the control side, before a read, a save or a fork: [`EditPost::settle`]
//!   claims the cell without waiting and applies what no block has taken
//!   through the plugin's own `Control`; a claim a node holds is
//!   [`Pending`](std::task::Poll::Pending), and the caller asks again. The
//!   write is the plugin's once the reader's sweep has reached the
//!   [`PendingWrites`](crate::PendingWrites) the caller took, so what it observes holds every
//!   write made before it. A format's Processor applies what its own slots
//!   hold ahead of a block's parameter changes, so a write made after a settle
//!   wins over it.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::Poll;

#[cfg(test)]
use tutti_plugin_types::fake::at_frame;
use tutti_plugin_types::{
    Control, EditReader, EditWriter, MainThreadMarker, UnknownParam, edit_slots,
};
use tutti_rt::Mutex;
use tutti_rt::{AudioThreadCell, Generation};
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
        refused: AtomicU64::new(0),
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
    /// Writes the plugin did not take: refused when applied.
    refused: AtomicU64,
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

    /// How many writes have been made: what a caller takes at its call, and
    /// [`settle`](Self::settle) waits for the plugin to have.
    pub(super) fn pending(&self) -> crate::PendingWrites {
        crate::PendingWrites::new(self.0.state.lock().writer.written())
    }

    /// Counts `n` writes the plugin did not take.
    fn refuse(&self, n: u64) {
        self.0.refused.fetch_add(n, Ordering::Relaxed);
    }

    /// The writes the plugin did not take, so far.
    pub(super) fn refused(&self) -> u64 {
        self.0.refused.load(Ordering::Relaxed)
    }

    /// Makes every write made so far stale ([`EditWriter::reset`]), and forgets
    /// the ones the controller was not told of.
    pub(super) fn reset(&self) {
        let mut state = self.0.state.lock();
        state.writer.reset();
        state.to_inform.clear();
    }

    /// Applies every write made so far through the plugin's own `Control`,
    /// waiting for a node's drain to let go of the reader. The claim's holder
    /// is a bounded audio-thread drain, so the wait is at most one drain: the
    /// mark is fixed when it starts, and later writes do not extend it. For a
    /// synchronous caller on the main thread with no later tick to ask again
    /// on (a save, a fork); the rest [`settle`](Self::settle) and are asked
    /// again. It does not wait for a value a Controller queued for its
    /// Processor's next block: with no block stepping, nothing would end that
    /// wait.
    pub(super) fn settle_now(&self, main: MainThreadMarker, control: &mut dyn Control) {
        let host = self.pending().host();
        while self.sweep(main, control, host).is_pending() {
            std::thread::yield_now();
        }
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

    /// Applies the writes no block has taken through the plugin's own
    /// `Control` ([`Control::set_parameter`]), so a read or a save made now sees
    /// them. `Ready` once every write up to `mark` is the plugin's, whether this
    /// call or a block took it; `Pending` while a node holds the reader for a
    /// drain, a write overlapped this sweep, or the plugin's Controller holds a
    /// value for the Processor's next block: ask again. A write the plugin
    /// refuses is counted ([`refused`](Self::refused)). Control thread, main
    /// thread.
    pub(super) fn settle(
        &self,
        main: MainThreadMarker,
        control: &mut dyn Control,
        pending: &mut crate::PendingWrites,
    ) -> Poll<()> {
        if self.sweep(main, control, pending.host()).is_pending() {
            return Poll::Pending;
        }
        // The host's writes are the Control's. A value it queued for its
        // Processor is the plugin's once the call that took it has returned;
        // the mark is the queue as it stood when the sweep finished, and later
        // writes do not move it. Wake the node that runs that block. With no
        // block stepping (an undriven graph), the caller's next one delivers it.
        if let Some(upto) = pending.queue_mark(|| control.queued())
            && !control.delivered(upto)
        {
            if let Some(wake) = &*self.0.wake.lock() {
                wake.notify();
            }
            return Poll::Pending;
        }
        Poll::Ready(())
    }

    /// The host's writes up to `mark` are the plugin's `Control`'s: taken from
    /// the slots and applied through [`Control::set_parameter`], by this call
    /// or a block's drain. `Pending` while a node holds the reader or a write
    /// overlapped the sweep. A value the Controller then queues for its
    /// Processor is not waited for here ([`settle`](Self::settle) does).
    fn sweep(
        &self,
        main: MainThreadMarker,
        control: &mut dyn Control,
        mark: Generation,
    ) -> Poll<()> {
        let Some(mut reader) = self.0.reader.try_borrow_mut() else {
            return Poll::Pending;
        };
        let mut refused = 0;
        reader.drain(|id, value| {
            if control.set_parameter(main, id, value).is_err() {
                refused += 1;
            }
        });
        self.refuse(refused);
        if reader.swept().has_reached(mark) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
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
    /// latest value and the epoch the drain observed. A refused claim (the
    /// control side settles) or a load in progress hands nothing: the slots keep
    /// what they hold for the next block.
    pub(super) fn drain(&self, apply: impl FnMut(ParamId, Normalized, u32)) {
        if let Some(mut reader) = self.0.reader.try_borrow_mut()
            && !self.0.restoring.load(Ordering::Acquire)
        {
            reader.drain_in_epoch(apply);
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
    use tutti_plugin_types::fake::{Call, FakePlugin};
    use tutti_plugin_types::{Loaded, MaxBlock, Prepare, Samples};
    use tutti_units::SampleRate;

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

    /// **A settle applies what no block has taken, through the plugin's own
    /// `Control`, and a block does not take it again.**
    #[test]
    fn a_settle_consumes_what_a_block_would_have() {
        let (fake, loaded) = FakePlugin::new();
        fake.declare(&[(id(1), false), (id(2), false)]);
        let prepare = Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::new(Samples::new(64)).expect("a block"),
        );
        let Ok((_processor, mut controller)) =
            loaded.activate(tutti_fixtures::main_thread(), prepare)
        else {
            panic!("the fake activates");
        };
        let (post, port) = host_edits([id(1), id(2)]);
        post.write(id(1), Normalized::of(0.25)).unwrap();
        assert_eq!(drained(&port), [(id(1), 0.25)], "a block took the first");
        post.write(id(2), Normalized::of(0.5)).unwrap();
        let mut pending = post.pending();
        // The fake's Controller queues the write to its Processor, which has not
        // run a block: the plugin has the write when one does.
        assert!(
            post.settle(tutti_fixtures::main_thread(), &mut controller, &mut pending)
                .is_pending()
        );
        let set: Vec<_> = fake
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::SetParameter(_)))
            .collect();
        assert_eq!(
            set,
            [Call::SetParameter(id(2))],
            "only the one no block took"
        );
        assert_eq!(drained(&port), [], "the settle consumed it");
    }

    /// **A settle does not wait for a node that holds the reader, and answers
    /// `Ready` once the node's drain has taken the write**: the write is the
    /// plugin's when the sweep has reached the mark, by whichever taker swept.
    #[test]
    fn a_settle_is_pending_while_a_node_drains_and_ready_once_swept() {
        let (_fake, loaded) = FakePlugin::new();
        let prepare = Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::new(Samples::new(64)).expect("a block"),
        );
        let Ok((_processor, mut controller)) =
            loaded.activate(tutti_fixtures::main_thread(), prepare)
        else {
            panic!("the fake activates");
        };
        let main = tutti_fixtures::main_thread();
        let (post, port) = host_edits([id(1)]);
        post.write(id(1), Normalized::of(0.5)).unwrap();
        let mut pending = post.pending();
        let held = post.0.reader.try_borrow_mut().expect("free");
        assert!(
            post.settle(main, &mut controller, &mut pending)
                .is_pending(),
            "the reader is held"
        );
        drop(held);
        assert_eq!(drained(&port), [(id(1), 0.5)], "a block took the write");
        assert!(
            post.settle(main, &mut controller, &mut pending).is_ready(),
            "a block's sweep counts"
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

    /// **A write the plugin refuses when it is applied is counted.**
    #[test]
    fn a_refused_write_is_counted() {
        let (_fake, loaded) = FakePlugin::new();
        let prepare = Prepare::new(
            SampleRate::SR_48K,
            MaxBlock::new(Samples::new(64)).expect("a block"),
        );
        let Ok((_processor, mut controller)) =
            loaded.activate(tutti_fixtures::main_thread(), prepare)
        else {
            panic!("the fake activates");
        };
        // Declared to the slots, not to the fake: its policy refuses the id.
        let (post, _port) = host_edits([id(7)]);
        post.write(id(7), Normalized::of(0.5)).unwrap();
        let mut pending = post.pending();
        assert!(
            post.settle(tutti_fixtures::main_thread(), &mut controller, &mut pending)
                .is_ready()
        );
        assert_eq!(post.refused(), 1);
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
