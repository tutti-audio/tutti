//! [`RenderMode`]: whether blocks arrive under realtime pressure.
//!
//! The session sets it once, when it prepares the graph, and every node reads
//! it from [`Prepare::render_mode`](crate::Prepare::render_mode) beside the
//! sample rate and the largest block. It is never a per-block field: a node
//! that picks an algorithm, a buffer size or a way of waiting by it does so at
//! [`Node::prepare`](crate::Node::prepare), and a change of mode is a new
//! prepare.
//!
//! # Why two variants
//!
//! A third mode sits between these in the plugin formats (VST3's `kPrefetch`,
//! VST2's `ProcessLevel::Prefetch`): blocks that arrive at a variable rate but
//! must still be rendered at realtime quality. It is a live-playback mode, not
//! a render one, and no host selects it, so it is [`Realtime`](RenderMode::Realtime)
//! here. An offline bounce permits higher quality and unbounded time, which is
//! the opposite instruction.

/// Whether the session renders under realtime pressure.
///
/// Carried by [`Prepare`](crate::Prepare), fixed for the node until the next
/// prepare. A node uses the answer to trade time for quality or exactness: a
/// disk-streaming sampler waits for its reads offline and counts a late read
/// live; a plugin picks a more expensive algorithm (longer FFT windows, more
/// oversampling, look-ahead) offline.
///
/// The session prepares [`Realtime`](Self::Realtime) for a live graph and
/// [`Offline`](Self::Offline) for an offline render or a bounce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RenderMode {
    /// Blocks arrive at a realtime cadence: a node finishes each block in its
    /// slice and never waits. The default, because it is the conservative
    /// answer: a node told nothing behaves as it does live.
    #[default]
    Realtime,
    /// Blocks arrive without time pressure (an offline bounce or a
    /// freewheeling render). A node may wait for its inputs and spend more per
    /// block for higher quality.
    Offline,
}

impl RenderMode {
    /// Returns whether this mode is free of realtime pressure.
    ///
    /// Named rather than a bare `== Offline` comparison because every reader
    /// reduces the mode to this one boolean.
    #[must_use]
    pub const fn is_offline(self) -> bool {
        matches!(self, RenderMode::Offline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node told nothing behaves as it does live: every format defaults this
    /// way (AU's property "defaults to false", CLAP's `CLAP_RENDER_REALTIME` is
    /// 0), and a host that forgets to set it must not silently opt a node into
    /// a slower path on the audio thread.
    #[test]
    fn the_default_is_realtime() {
        assert_eq!(RenderMode::default(), RenderMode::Realtime);
        assert!(!RenderMode::default().is_offline());
        assert!(RenderMode::Offline.is_offline());
    }
}
