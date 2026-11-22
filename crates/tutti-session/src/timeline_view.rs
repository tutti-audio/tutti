//! [`TimelineView`]: what the session lets a node read of its timeline.

use tutti_timeline::{LoopReader, TempoReader, Transport};

/// The session's timeline as a node may read it: the tempo map and the loop region, each a
/// read-only view the transport publishes ([`TempoReader`], [`LoopReader`]). A node bound
/// to it ([`SessionNode::bind_timeline`](crate::SessionNode::bind_timeline)) keeps the
/// readers it follows and can neither edit the map or the loop nor command the transport.
#[derive(Clone, Debug)]
pub struct TimelineView {
    tempo: TempoReader,
    loops: LoopReader,
}

impl TimelineView {
    /// A view over `tempo` and `loops`.
    #[must_use]
    pub fn new(tempo: TempoReader, loops: LoopReader) -> Self {
        Self { tempo, loops }
    }

    /// The view of `transport`: its tempo map and its loop region.
    #[must_use]
    pub fn of(transport: &Transport) -> Self {
        Self::new(
            transport.settings().tempo_reader(),
            transport.motion().loop_reader(),
        )
    }

    /// The tempo map's reader.
    #[must_use]
    pub fn tempo(&self) -> &TempoReader {
        &self.tempo
    }

    /// The loop region's reader.
    #[must_use]
    pub fn loops(&self) -> &LoopReader {
        &self.loops
    }
}
