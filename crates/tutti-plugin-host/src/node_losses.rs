//! What a plugin node loses on the audio thread that is not a parameter point
//! or a MIDI input event: the blocks it did not render, the entries the plugin
//! refused, the MIDI out and harmony it had no room for.

use tutti_rt::{Tally, TallyWriter, tally};
use tutti_units::Blocks;

/// One count the audio thread adds to and any thread reads.
#[derive(Debug)]
pub(crate) struct Count {
    tally: Tally<u64>,
    writer: TallyWriter<u64>,
}

impl Default for Count {
    fn default() -> Self {
        let tally = tally();
        Self {
            writer: tally.writer(),
            tally,
        }
    }
}

impl Count {
    /// `n` more. RT-safe: one relaxed add, none for zero.
    pub(crate) fn add(&self, n: u64) {
        if n > 0 {
            self.writer.add(n);
        }
    }

    /// The total so far.
    pub(crate) fn get(&self) -> u64 {
        self.tally.get()
    }
}

/// The counts behind [`BlockLosses`] and the node's lost MIDI out and
/// harmony, shared between the node and its controls.
#[derive(Debug, Default)]
pub(crate) struct LossCounts {
    pub(crate) unlent: Count,
    pub(crate) failed: Count,
    pub(crate) nonfinite: Count,
    pub(crate) entries_refused: Count,
    pub(crate) inputs_refused: Count,
    pub(crate) midi_out: Count,
    pub(crate) harmony: Count,
}

impl LossCounts {
    pub(crate) fn blocks(&self) -> BlockLosses {
        BlockLosses {
            unlent: Blocks::new(self.unlent.get()),
            failed: Blocks::new(self.failed.get()),
            nonfinite: Blocks::new(self.nonfinite.get()),
            entries_refused: Blocks::new(self.entries_refused.get()),
            inputs_refused: self.inputs_refused.get(),
        }
    }
}

/// What a plugin node did not render, or the plugin did not take, since it was
/// bound ([`NodeControls::block_losses`](crate::NodeControls::block_losses)).
/// Counted in blocks and entries, not events: the MIDI such a block loses is
/// in [`dropped_midi_in`](crate::NodeControls::dropped_midi_in). A node whose
/// Processor renders in chunks ([`Processor::chunk`](tutti_plugin_types::Processor::chunk))
/// cuts a block into pieces, and each piece is a block here: `unlent`, `failed`
/// and `nonfinite` count the pieces of such a node.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockLosses {
    /// Blocks rendered silent because the realtime end held no Processor
    /// (recalled for a rate change, or the plugin refused its rate).
    pub unlent: Blocks,
    /// Blocks rendered silent because the Processor failed them.
    pub failed: Blocks,
    /// Blocks the plugin wrote a NaN or an infinity in, passed on with those
    /// samples zeroed.
    pub nonfinite: Blocks,
    /// Audio entries the plugin refused in blocks that otherwise ran (a closed
    /// or dead plugin, a missing entry, pins grown past the buffer): each a
    /// block or a piece of one that rendered silence, not a failed block.
    pub entries_refused: Blocks,
    /// Input messages the plugin's host refused, in the host's unit: a MIDI 1.0
    /// message or a whole SysEx message, not the node's events.
    pub inputs_refused: u64,
}

impl std::ops::Add for BlockLosses {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            unlent: self.unlent + rhs.unlent,
            failed: self.failed + rhs.failed,
            nonfinite: self.nonfinite + rhs.nonfinite,
            entries_refused: self.entries_refused + rhs.entries_refused,
            inputs_refused: self.inputs_refused.saturating_add(rhs.inputs_refused),
        }
    }
}

impl std::ops::AddAssign for BlockLosses {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}
