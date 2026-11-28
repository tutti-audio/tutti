//! Which way a byte port goes, and the bit of an [`EndpointId`](tutti_midi_device::EndpointId)
//! that says so.

/// Which way a port goes. Inputs and outputs are numbered separately by a byte API, and a
/// browser may give both one id string, so neither a position nor a name alone says which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    /// A port that sends MIDI to this engine.
    Input,
    /// A port this engine sends MIDI to.
    Output,
}

/// The top bit of a port's id: set for an output.
pub(super) const OUTPUT_BIT: u64 = 1 << 63;

impl Side {
    /// This side's bits of an id: none for an input, [`OUTPUT_BIT`] for an output.
    pub(super) fn bit(self) -> u64 {
        match self {
            Self::Input => 0,
            Self::Output => OUTPUT_BIT,
        }
    }
}
