//! What a read of an SMF refused: [`SmfParseError`], the bytes that are not
//! well formed or end too soon.

/// Why Standard MIDI File bytes are not a well-formed file. A file that is cut
/// off is refused with one of the `CutOff` variants and never read as a
/// shorter file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum SmfParseError {
    /// A variable-length quantity of more than four bytes, or one that does not end.
    #[error("malformed variable-length quantity")]
    VariableLengthQuantity,
    /// The division is zero ticks per quarter note.
    #[error("zero ticks-per-beat")]
    ZeroTicksPerBeat,
    /// An SMPTE division whose frame rate is none of the four.
    #[error("invalid SMPTE frame rate")]
    InvalidFrameRate,
    /// The header's format is not 0, 1 or 2.
    #[error("unknown format")]
    UnknownFormat,
    /// The file does not begin with the header chunk.
    #[error("missing header chunk")]
    MissingHeader,
    /// The header chunk is shorter than its six bytes.
    #[error("header chunk shorter than six bytes")]
    ShortHeader,
    /// A second header chunk after the first.
    #[error("a second header chunk")]
    SecondHeader,
    /// A channel message whose data bytes are missing or have their top bit set.
    #[error("channel message data")]
    ChannelData,
    /// A data byte where a running status is not active.
    #[error("data byte with no running status")]
    NoRunningStatus,
    /// A system common or real-time status in a file.
    #[error("system common or real-time status in a file")]
    SystemStatus,
    /// An End of Track that is not the last event of its track.
    #[error("an End of Track that is not the last event in its track")]
    EndOfTrackNotLast,
    /// More track chunks than the header declares.
    #[error("more tracks than the header declares")]
    ExtraTracks,
    /// A format 0 file with other than one track.
    #[error("a format 0 file has one track")]
    FormatZeroTracks,
    /// A chunk's type and length are short of eight bytes, or its body runs
    /// past the end of the file.
    #[error("a chunk runs past the end of the file")]
    ChunkCutOff,
    /// The header declares more track chunks than the file holds.
    #[error("the file holds fewer tracks than the header declares")]
    TracksCutOff,
    /// An event ends past the end of its track chunk.
    #[error("an event is cut off by the end of its chunk")]
    EventCutOff,
}
