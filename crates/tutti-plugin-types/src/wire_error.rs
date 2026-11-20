//! The typed causes inside a [`IpcError`](crate::IpcError): what is wrong
//! with a frame, an audio slab's layout, or a slab access.

use thiserror::Error;

use crate::MAX_BLOCK_CHANNELS;

/// A frame whose length is over the protocol's limit.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("frame of {len} bytes is over the {limit}-byte protocol limit")]
pub struct FrameTooLarge {
    /// The body length, in bytes, that was advertised or about to be sent.
    pub len: usize,
    /// The protocol's limit, in bytes.
    pub limit: usize,
}

/// Why an audio slab's layout is refused: it is a shape the peer chose, and a
/// slab is mapped and addressed from it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SlabLayoutError {
    /// A direction names no bus: an empty list would read as "share one region
    /// in place", which is the aliasing bypass.
    #[error(
        "slab layout must name at least one bus per direction; \
         an empty list would read as 'share one region in place', which is the bypass"
    )]
    NoBus,
    /// The ring depth is not this build's.
    #[error("slab layout asks for {asked} ring slots, this build addresses {served}")]
    Slots {
        /// The depth the layout names.
        asked: u32,
        /// The depth this build addresses.
        served: u32,
    },
    /// A direction names more buses than a block carries channels.
    #[error("slab layout names {buses} {direction} buses, at most {MAX_BLOCK_CHANNELS} are served")]
    TooManyBuses {
        /// `"input"` or `"output"`.
        direction: &'static str,
        /// The bus count the layout names.
        buses: usize,
    },
    /// A direction is wider than a block's channel tables.
    #[error(
        "slab layout names {channels} {direction} channels, at most {MAX_BLOCK_CHANNELS} are served"
    )]
    TooManyChannels {
        /// `"input"` or `"output"`.
        direction: &'static str,
        /// The channel count the layout names.
        channels: u64,
    },
    /// The mapping's size does not fit in `usize`.
    #[error("slab layout's size overflows usize")]
    SizeOverflow,
}

/// Why a read or a write of a slab's channel was refused. `Copy` and free of
/// text, so the audio thread can meet it without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Error)]
pub enum SlabAccessError {
    /// The sample type is not the width the slab was laid out at.
    #[error("sample type does not match the slab's sample format")]
    SampleWidth,
    /// The slab has no such channel in that direction.
    #[error("channel index out of bounds")]
    Channel,
    /// The data is longer than a channel holds.
    #[error("data length exceeds buffer capacity")]
    Length,
}

/// Why a result file could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ResultError {
    /// The file is shorter than its own header or its declared payload.
    #[error("the probe result is truncated")]
    Truncated,
    /// The file does not begin with a result's magic.
    #[error("not a probe result (bad magic)")]
    BadMagic,
    /// The file is a probe result of another format version: a probe and a host
    /// of different builds.
    #[error("probe result of format version {found}, this build reads {expected}")]
    ProtocolMismatch {
        /// The version this build reads.
        expected: u8,
        /// The version the file carries.
        found: u8,
    },
    /// The payload does not decode as a `ProbeOutcome`.
    #[error("malformed probe result: {0}")]
    Malformed(String),
    /// The file exists and could not be read.
    #[error("probe result unreadable: {0}")]
    Io(String),
}
