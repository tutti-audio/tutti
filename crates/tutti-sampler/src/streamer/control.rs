//! What a stream plays ([`StreamFile`], what an export fork reads).
//!
//! The control side of a stream is [`Stream`](crate::Stream); the reader it
//! hands its voice is the reply of its open ([`PendingOpen`](crate::PendingOpen)).

use std::sync::Arc;

use tutti_buffers::{Float, Wave};
use tutti_platform::Asset;
use tutti_units::SampleRate;

/// The file a stream plays: the [`Asset`] it is read from, its own rate and, when the streamer
/// holds it decoded whole, the wave.
///
/// Immutable, built once when the stream opens and shared by the
/// [`Stream`](crate::Stream) ([`Stream::file`](crate::Stream::file)) and the
/// stream's [`StreamReader`](crate::StreamReader), so a fork of the voice (made
/// from the node, which holds the reader and not the handle) plays the same
/// material with no streamer at all. What changes while a stream runs is not
/// here: the loop as applied and the direction are the ring's published
/// control, and a stream that has ended is its status.
///
/// A live voice may be the last holder, and its drop can land where the graph
/// frees units, never the audio thread; what it frees is an asset and a wave the
/// streamer's writer shares.
#[derive(Clone)]
pub struct StreamFile<F: Float> {
    asset: Asset,
    file_rate: SampleRate,
    resident: Option<Arc<Wave<F>>>,
}

impl<F: Float> StreamFile<F> {
    pub(crate) fn new(
        asset: impl Into<Asset>,
        file_rate: SampleRate,
        resident: Option<Arc<Wave<F>>>,
    ) -> Self {
        Self {
            asset: asset.into(),
            file_rate,
            resident,
        }
    }

    /// What the stream was opened on. A fork opens it again through
    /// [`Asset::open`].
    #[must_use]
    pub fn asset(&self) -> &Asset {
        &self.asset
    }

    /// The file's own sample rate, from its header.
    #[must_use]
    pub fn file_rate(&self) -> SampleRate {
        self.file_rate
    }

    /// The whole file, when the streamer holds it decoded: a file that cannot
    /// seek (mp3, ogg) is decoded whole to be streamed, and a fork reads that
    /// wave in place rather than decode it a second time. `None` for a file the
    /// streamer reads a piece at a time.
    #[must_use]
    pub fn resident(&self) -> Option<&Arc<Wave<F>>> {
        self.resident.as_ref()
    }
}

impl<F: Float> std::fmt::Debug for StreamFile<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamFile")
            .field("asset", &self.asset)
            .field("file_rate", &self.file_rate)
            .field("resident", &self.resident.is_some())
            .finish()
    }
}
