//! [`DeviceEpochs`]: which device sits behind each event input of a
//! [`MidiIngestNode`](crate::MidiIngestNode), as the control side says and the
//! node acknowledges.
//!
//! An input's **epoch** counts the comings and goings of its devices: each
//! [`arrive`](DeviceEpochs::arrive) and each [`depart`](DeviceEpochs::depart)
//! moves it on, and it is odd while a device is there and even while none is. A
//! node built [`following`](crate::MidiIngestNode::following) the epochs
//! compares each input's epoch every block. A change ends what the device
//! before it started: the notes it held and the pedals and bends it left are
//! released downstream at the block's first frame, and its half-sent RPN and
//! MPE state are dropped, so none is carried into the next device; an input
//! whose device went hears nothing until another arrives.
//!
//! The node answers with the epoch it has handled once nothing it owes is left
//! to send ([`is_settled`](DeviceEpochs::is_settled)), so the control side can
//! take a node out of the graph after its device's notes have been released,
//! not before.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What the control side and the node say to each other about the devices
/// behind the node's event inputs: cheap to clone, one cell pair per input.
#[derive(Clone, Debug)]
pub struct DeviceEpochs {
    cells: Arc<[Cell]>,
}

#[derive(Debug)]
struct Cell {
    /// The control side's epoch: odd while a device is there.
    epoch: AtomicU64,
    /// The epoch the node has handled and owes nothing for.
    settled: AtomicU64,
}

impl DeviceEpochs {
    /// Epochs for `inputs` event inputs, none with a device.
    #[must_use]
    pub fn new(inputs: NonZeroUsize) -> Self {
        Self {
            cells: (0..inputs.get())
                .map(|_| Cell {
                    epoch: AtomicU64::new(0),
                    settled: AtomicU64::new(0),
                })
                .collect(),
        }
    }

    /// How many event inputs there are.
    #[must_use]
    pub fn inputs(&self) -> usize {
        self.cells.len()
    }

    /// A device takes `input`: what the one before held is released and the
    /// input starts empty, with the device present. Control side.
    ///
    /// # Panics
    ///
    /// If `input` is not one of the epochs' inputs.
    pub fn arrive(&self, input: usize) {
        let cell = &self.cells[input];
        // Even (no device): one step to odd. Odd (a device that never left):
        // two, so the node still sees a change.
        let step = if cell.epoch.load(Ordering::Relaxed).is_multiple_of(2) {
            1
        } else {
            2
        };
        // Release pairs with the node's acquire load.
        cell.epoch.fetch_add(step, Ordering::Release);
    }

    /// The device behind `input` goes: the node releases what it held and then
    /// hears nothing from the input. A call with no device there changes
    /// nothing. Control side.
    ///
    /// # Panics
    ///
    /// If `input` is not one of the epochs' inputs.
    pub fn depart(&self, input: usize) {
        let cell = &self.cells[input];
        if cell.epoch.load(Ordering::Relaxed) % 2 == 1 {
            cell.epoch.fetch_add(1, Ordering::Release);
        }
    }

    /// Whether a device is behind `input` as the control side last said.
    ///
    /// # Panics
    ///
    /// If `input` is not one of the epochs' inputs.
    #[must_use]
    pub fn is_present(&self, input: usize) -> bool {
        self.cells[input].epoch.load(Ordering::Relaxed) % 2 == 1
    }

    /// Whether the node has handled `input`'s latest change and has nothing
    /// left to release for it: what a node about to be taken out of the graph
    /// waits for.
    ///
    /// # Panics
    ///
    /// If `input` is not one of the epochs' inputs.
    #[must_use]
    pub fn is_settled(&self, input: usize) -> bool {
        let cell = &self.cells[input];
        cell.settled.load(Ordering::Acquire) == cell.epoch.load(Ordering::Acquire)
    }

    /// The epoch of `input`, for the node's block.
    pub(crate) fn epoch(&self, input: usize) -> u64 {
        self.cells[input].epoch.load(Ordering::Acquire)
    }

    /// The node has handled `epoch` of `input` and owes nothing for it.
    pub(crate) fn settle(&self, input: usize, epoch: u64) {
        self.cells[input].settled.store(epoch, Ordering::Release);
    }
}
