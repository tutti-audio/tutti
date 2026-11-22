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
//! - the control side, before a read, a save or a fork: [`EditPost::flush`]
//!   claims the cell, waiting, and applies what no block has taken through the
//!   plugin's own `Control`, so what it observes holds every write made before
//!   it. A format's Processor applies what its own slots hold ahead of a
//!   block's parameter changes, so a write made after a flush wins over it.
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

use tutti_plugin_types::{
    Control, EditReader, EditWriter, MainThreadMarker, UnknownParam, edit_slots,
};
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
        Ok(())
    }

    /// Makes every write made so far stale ([`EditWriter::reset`]), and forgets
    /// the ones the controller was not told of.
    pub(super) fn reset(&self) {
        let mut state = self.0.state.lock();
        state.writer.reset();
        state.to_inform.clear();
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

    /// Takes what no block has taken, claiming the reader (waiting for a node
    /// that holds it for the length of a drain).
    fn take_undrained(&self) -> Vec<(ParamId, Normalized)> {
        let mut taken = Vec::new();
        loop {
            if let Some(mut reader) = self.0.reader.try_borrow_mut() {
                reader.drain(|id, value| taken.push((id, value)));
                return taken;
            }
            std::thread::yield_now();
        }
    }

    /// Applies the writes no block has taken through the plugin's own
    /// `Control` ([`Control::set_parameter`]), so a read or a save made now sees
    /// them. Each is consumed here or by a block, never both. Control thread,
    /// main thread.
    pub(super) fn flush(&self, main: MainThreadMarker, control: &mut dyn Control) {
        for (id, value) in self.take_undrained() {
            // A write the plugin refuses (read-only) is refused here as a block
            // would refuse it.
            let _ = control.set_parameter(main, id, value);
        }
    }
}

/// The audio side: the node's claim on the reader. Wait-free, and holds no lock.
#[derive(Clone)]
pub(super) struct EditPort(Arc<Shared>);

impl EditPort {
    /// Hands `apply` each parameter written since the last drain, with its
    /// latest value and the epoch the drain observed. A refused claim (the
    /// control side flushes) hands nothing: the slots keep what they hold for
    /// the next block.
    pub(super) fn drain(&self, apply: impl FnMut(ParamId, Normalized, u32)) {
        if let Some(mut reader) = self.0.reader.try_borrow_mut() {
            reader.drain_in_epoch(apply);
        }
    }

    /// The slots' epoch now, whoever holds the reader: what a write drained
    /// earlier is compared with before it is sent. Wait-free.
    pub(super) fn epoch(&self) -> u32 {
        self.0.epoch.get()
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

    /// **A flush applies what no block has taken, through the plugin's own
    /// `Control`, and a block does not take it again.**
    #[test]
    fn a_flush_consumes_what_a_block_would_have() {
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
        post.flush(tutti_fixtures::main_thread(), &mut controller);
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
        assert_eq!(drained(&port), [], "the flush consumed it");
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
            params.write(param, 0, value.get(), epoch);
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
