//! [`ToMemory`] and [`ToSink`]: the destinations that are not a file.

use tutti_buffers::{ChannelLayout, ChannelSet, Sink};
use tutti_export::BufferTarget;

/// Planes in memory: [`Bounced::main_output`](crate::Bounced::main_output) and a take's
/// [`Recorded::output`](crate::Recorded::output) are their
/// [`Rendered`](tutti_export::Rendered), one plane per channel at the render's rate. Nothing
/// is encoded, resampled, dithered or normalized, and a destination in memory has no way to
/// ask for any of them.
///
/// A bounce knows its span, so what it holds is bounded by its frame limit
/// ([`tutti_export::MAX_FRAMES`]): the cost is the frames, times the channels, times the
/// size of the graph's float, and a span too long for that is refused before anything is
/// rendered. A take of no stated length holds every frame it records: the cost grows with
/// the take, and a take to be kept for long goes to a file.
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
    /// output's. A narrowing the fold cannot place is refused when the render or the take
    /// starts, never truncated.
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

#[cfg(feature = "capture")]
impl ToMemory {
    /// The fold from `source`'s channels onto the planes', when they are not the same
    /// channels, and the width of the planes. Built on the control side, so the pump only
    /// applies it.
    ///
    /// # Errors
    ///
    /// [`FoldError`](tutti_buffers::FoldError) for channels the fold cannot place.
    pub(crate) fn fold<F: tutti_buffers::Float>(
        &self,
        source: &ChannelSet,
    ) -> Result<(ChannelLayout, Option<tutti_buffers::Fold<F>>), tutti_buffers::FoldError> {
        let Some(planes) = &self.channels else {
            return Ok((source.width(), None));
        };
        let fold = if planes == source {
            None
        } else {
            Some(tutti_buffers::Fold::to_channels(source, planes).build()?)
        };
        Ok((planes.width(), fold))
    }
}

/// A caller's [`Sink`]: a bounce streams an output into it as it renders, and a take streams
/// its frames into it as it records.
///
/// The sink takes the frames as the source plays them, at the source's width, the graph's
/// rate and the graph's float: no fold, no resample and no dither. A sink that quantizes
/// dithers itself (put a [`tutti_dsp::Quantizer`] in front of an integer sink). Its
/// [`layout`](Sink::layout) must be the source's width, which carries no rate to check: a
/// width that differs is refused before anything is written
/// ([`TakeError::Width`](crate::TakeError::Width) for a take,
/// [`tutti_export::TargetError::SinkWidth`] for a bounce). The sink is finalized once, after
/// the last frame; one that fails ends the bounce or the take with its error. A sink has no
/// path, so no [`Recorded::clip`](crate::Recorded::clip), and cannot be normalized.
#[must_use = "a destination does nothing until a bounce or a take takes it"]
pub struct ToSink<S>(pub(crate) S);

impl<S: Sink> ToSink<S> {
    /// Streams into `sink`.
    pub fn new(sink: S) -> Self {
        Self(sink)
    }
}

impl<S> std::fmt::Debug for ToSink<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToSink").finish_non_exhaustive()
    }
}
