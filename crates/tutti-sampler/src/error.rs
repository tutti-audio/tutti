//! The crate-wide error type, for the paths that can fail on file access.
//!
//! Midi1Translator failures live with the operation that raises them — `SampleProbeError`
//! for a header read, `OpenError` for a stream open —
//! because a caller of those branches on the specific cause rather than on "an
//! error happened".

use thiserror::Error;

/// A failure on a sampler path that touches the filesystem.
///
/// `#[non_exhaustive]`: a match on this must carry a `_` arm, so adding a
/// variant for a new failure mode is not a breaking change.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The underlying file could not be read or written.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// The OS refused to start the disk streamer's thread
    /// ([`DiskStreamer::start`](crate::DiskStreamer::start)): there is no
    /// streamer, and a stream command with no consumer would stall silently.
    #[error("the disk streamer's thread could not be started: {0}")]
    Spawn(#[source] std::io::Error),
}

/// `Result` specialized to this crate's [`Error`](enum@Error).
pub type Result<T> = std::result::Result<T, Error>;

/// Why a wave cannot be placed into a voice's channels.
///
/// Raised on the control side when a source is built over a wave
/// ([`MemorySource::with_config`](crate::MemorySource::with_config),
/// [`memory_voice`](crate::memory_voice)), never on the audio thread: a wave
/// that cannot be placed is refused rather than played with a channel in the
/// wrong place or dropped.
#[derive(Error, Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum PlaceError {
    /// A channel of the wave has no place in the voice's frame; the
    /// [`FoldError`](tutti_buffers::FoldError) says which and why. A file
    /// channel no speaker names (a `Speaker::Unknown` in its topology) is
    /// [`FoldError::Unnamed`](tutti_buffers::FoldError::Unnamed).
    #[error("the wave cannot be placed: {0}")]
    Fold(#[from] tutti_buffers::FoldError),
    /// A streamed clip's width is its stream's, chosen when the stream was
    /// opened ([`DiskStreamer::open_stream_into`](crate::DiskStreamer::open_stream_into));
    /// a clip cannot be placed at another.
    #[error("a streamed clip is as wide as its stream, which was opened at that width")]
    Streamed,
    /// The voice's width is zero: a node that reports no outputs is one
    /// nothing can be wired to.
    #[error("a voice needs at least one channel")]
    EmptyWidth,
    /// The voice's width is past the sampler's per-read ceiling
    /// ([`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS)): a voice wider than the
    /// graph's main output can render is one nobody can hear, and a narrower one would
    /// drop channels silently.
    #[error("a voice of {channels} channels is wider than the sampler's {max}")]
    WidthTooWide {
        /// The width asked for.
        channels: u16,
        /// The widest a voice runs at.
        max: u16,
    },
}
