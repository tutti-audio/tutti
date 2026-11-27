//! [`ToMemory`]: a bounce's output held as planes.

use tutti_buffers::{ChannelLayout, ChannelSet};
use tutti_export::BufferTarget;

/// Planes in memory: [`Bounced::main_output`](crate::Bounced::main_output) is their
/// [`Rendered`](tutti_export::Rendered), one plane per channel at the render's rate. Nothing
/// is encoded, resampled, dithered or normalized, and a bounce into memory has no way to ask
/// for any of them.
///
/// A bounce knows its span, so what it holds is bounded by its frame limit
/// ([`tutti_export::MAX_FRAMES`]): the cost is the frames, times the channels, times the
/// size of the graph's float, and a span too long for that is refused before anything is
/// rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToMemory {
    channels: Option<ChannelSet>,
}

impl ToMemory {
    /// Planes as wide as the output they hold.
    pub fn new() -> Self {
        Self::default()
    }

    /// The planes' channels: their width and the speakers they are, folded onto from the
    /// output's. A narrowing the fold cannot place is refused when the render starts, never
    /// truncated.
    pub fn channels(mut self, channels: ChannelSet) -> Self {
        self.channels = Some(channels);
        self
    }

    /// The planes of an output `width` channels wide unless channels were stated.
    pub(crate) fn planes(self, width: ChannelLayout) -> BufferTarget {
        match self.channels {
            None => BufferTarget::new(width),
            Some(set) => {
                let planes = BufferTarget::new(set.width());
                match set.topology() {
                    Some(topology) => planes.topology(*topology),
                    None => planes,
                }
            }
        }
    }
}
