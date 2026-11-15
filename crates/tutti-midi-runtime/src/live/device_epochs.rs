//! [`DeviceEpochs`]: which device sits behind the event input of a
//! [`MidiIngestNode`](crate::MidiIngestNode), as the control side says and the
//! node acknowledges.
//!
//! The input's **epoch** counts the comings and goings of its devices: each
//! [`arrive`](DeviceEpochs::arrive) and each [`depart`](DeviceEpochs::depart)
//! moves it on, and it is odd while a device is there and even while none is. A
//! node built [`following`](crate::MidiIngestNode::following) the epochs
//! compares the epoch every block. A change ends what the device before it
//! started: the notes it held and the pedals and bends it left are released
//! downstream at the block's first frame, and its half-sent RPN and MPE state
//! are dropped, so none is carried into the next device; an input whose device
//! went hears nothing until another arrives.
//!
//! The node answers with the epoch it has handled once its release has been
//! pushed to the node's ports and nothing is left owing
//! ([`is_settled`](DeviceEpochs::is_settled)), so the control side can take a
//! node out of the graph after its device's notes have been released, not
//! before. A node that is never processed never settles.
//!
//! **One control-side writer.** `arrive` and `depart` read the epoch and then
//! add to it, which is not one atomic step: they are called from one thread at a
//! time (the session's control thread). The node is the one other party and only
//! stores what it has settled.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What the control side and the node say to each other about the device
/// behind the node's event input: cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct DeviceEpochs {
    cell: Arc<Cell>,
}

#[derive(Debug, Default)]
struct Cell {
    /// The control side's epoch: odd while a device is there.
    epoch: AtomicU64,
    /// The epoch the node has handled and owes nothing for.
    settled: AtomicU64,
}

impl DeviceEpochs {
    /// Epochs of an input with no device.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A device takes the input: what the one before held is released and the
    /// input starts empty, with the device present. Control side.
    pub fn arrive(&self) {
        // Even (no device): one step to odd. Odd (a device that never left):
        // two, so the node still sees a change.
        let step = if self.cell.epoch.load(Ordering::Relaxed).is_multiple_of(2) {
            1
        } else {
            2
        };
        // Release pairs with the node's acquire load.
        self.cell.epoch.fetch_add(step, Ordering::Release);
    }

    /// The device behind the input goes: the node releases what it held and then
    /// hears nothing from the input. A call with no device there changes
    /// nothing. Control side.
    pub fn depart(&self) {
        if self.is_present() {
            self.cell.epoch.fetch_add(1, Ordering::Release);
        }
    }

    /// Whether a device is behind the input as the control side last said.
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.cell.epoch.load(Ordering::Relaxed) % 2 == 1
    }

    /// Whether the node has handled the input's latest change and pushed what
    /// it owed for it: what a node about to be taken out of the graph waits for.
    /// After a [`depart`](Self::depart) it is false until a block has run.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.cell.settled.load(Ordering::Acquire) == self.cell.epoch.load(Ordering::Acquire)
    }

    /// The epoch, for the node's block.
    pub(crate) fn epoch(&self) -> u64 {
        self.cell.epoch.load(Ordering::Acquire)
    }

    /// The node has handled `epoch` and pushed what it owed for it.
    pub(crate) fn settle(&self, epoch: u64) {
        self.cell.settled.store(epoch, Ordering::Release);
    }
}
