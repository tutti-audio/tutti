//! [`DeviceGeneration`]: which device sits behind the event input of a
//! [`MidiInputNode`](crate::MidiInputNode), as the control side says and the
//! node acknowledges.
//!
//! The input's **generation** counts the comings and goings of its devices: each
//! [`arrive`](GenerationWriter::arrive) and each [`depart`](GenerationWriter::depart)
//! moves it on, and it is odd while a device is there and even while none is. A
//! node built [`following`](crate::MidiInputNode::following) the generation
//! compares the generation every block. A change ends what the device before it
//! started: the notes it held and the pedals and bends it left are released
//! downstream at the block's first frame, and its half-sent RPN and MPE state
//! are dropped, so none is carried into the next device; an input whose device
//! went hears nothing until another arrives.
//!
//! The node answers with the generation it has handled once its release has been
//! pushed to the node's ports and nothing is left owing
//! ([`is_idle`](DeviceGeneration::is_idle)), so the control side can take a
//! node out of the graph after its device's notes have been released, not
//! before. A node that is never processed never goes idle.
//!
//! **One control-side writer.** [`DeviceGeneration::new`] returns the node's half
//! ([`DeviceGeneration`], cheap to clone, which reads) with the one [`GenerationWriter`]
//! (not `Clone`, `&mut self` to move the generation): `arrive` and `depart` read the
//! generation and then store the next, so no two of them can overlap. The node only
//! stores what it has handled.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The node's half of what the control side and the node say to each other
/// about the device behind the node's event input: cheap to clone, and it only
/// reads the generation.
#[derive(Clone, Debug)]
pub struct DeviceGeneration {
    cell: Arc<Cell>,
}

#[derive(Debug, Default)]
struct Cell {
    /// The control side's generation: odd while a device is there.
    generation: AtomicU64,
    /// The generation the node has handled and owes nothing for.
    handled: AtomicU64,
}

/// The one writer of an input's generation: the control side's half of
/// [`DeviceGeneration`]. Not `Clone`.
#[derive(Debug)]
#[must_use = "the writer is how a device arrives and departs"]
pub struct GenerationWriter {
    cell: Arc<Cell>,
}

impl DeviceGeneration {
    /// Generations of an input with no device: the writer and the node's half.
    pub fn new() -> (GenerationWriter, Self) {
        let cell = Arc::new(Cell::default());
        (
            GenerationWriter {
                cell: Arc::clone(&cell),
            },
            Self { cell },
        )
    }

    /// Whether a device is behind the input as the control side last said.
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.cell.generation.load(Ordering::Relaxed) % 2 == 1
    }

    /// Whether the node has handled the input's latest change and pushed what
    /// it owed for it: what a node about to be taken out of the graph waits for.
    /// After a [`depart`](GenerationWriter::depart) it is false until a block has run.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.cell.handled.load(Ordering::Acquire) == self.cell.generation.load(Ordering::Acquire)
    }

    /// The generation, for the node's block.
    pub(crate) fn generation(&self) -> u64 {
        self.cell.generation.load(Ordering::Acquire)
    }

    /// The node has handled `generation` and pushed what it owed for it.
    pub(crate) fn mark_handled(&self, generation: u64) {
        self.cell.handled.store(generation, Ordering::Release);
    }
}

impl GenerationWriter {
    /// A device takes the input: what the one before held is released and the
    /// input starts empty, with the device present. Control side.
    pub fn arrive(&mut self) {
        let generation = self.cell.generation.load(Ordering::Relaxed);
        // Even (no device): one step to odd. Odd (a device that never left):
        // two, so the node still sees a change.
        let step = if generation.is_multiple_of(2) { 1 } else { 2 };
        // Release pairs with the node's acquire load.
        self.cell
            .generation
            .store(generation + step, Ordering::Release);
    }

    /// The device behind the input goes: the node releases what it held and then
    /// hears nothing from the input. A call with no device there changes
    /// nothing. Control side.
    pub fn depart(&mut self) {
        let generation = self.cell.generation.load(Ordering::Relaxed);
        if !generation.is_multiple_of(2) {
            self.cell
                .generation
                .store(generation + 1, Ordering::Release);
        }
    }
}
