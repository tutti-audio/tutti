//! What a read refused: [`Truncated`], the input that ends too soon, and
//! [`SmfParseError`], the Standard MIDI File bytes that are not well formed.

/// What was being read when the input ended.
///
/// A file that is cut off is an error and never a shorter file: what was read
/// before the end is not returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum Truncated {
    /// An SMF chunk: its type and length short of eight bytes, or a body that
    /// runs past the end of the file.
    #[error("a chunk runs past the end of the file")]
    Chunk,
    /// The SMF header declares more track chunks than the file holds.
    #[error("the file holds fewer tracks than the header declares")]
    Tracks,
    /// A Clip File's UMP claims more words than remain in the stream.
    #[error("a message runs past the end of the clip file")]
    Message,
    /// A Clip File ends inside a structural rule: a required message has no
    /// packets left to be read from.
    #[error("the clip file ends inside its structure")]
    Clip,
}

/// Why Standard MIDI File bytes are not a well-formed file.
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
    /// Bytes that no rule names.
    #[error("unexpected bytes")]
    Unexpected,
}
