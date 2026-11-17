//! What a track list can be refused for: [`ConfigError`], and the order a
//! [`Track`](crate::Track) builder holds ([`OutOfOrder`]).

use thiserror::Error;

use crate::Tick;

/// Why the tracks cannot make a file of the container asked for
/// ([`MidiFile::smf`](crate::MidiFile::smf), [`MidiFile::clip`](crate::MidiFile::clip),
/// and a write): each variant is one rule of the container, with where it was
/// broken. A `track` is an index into the list given.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The track list is empty.
    #[error("a file needs at least one track")]
    NoTracks,
    /// A format 0 file has exactly one track.
    #[error("a format 0 file has one track, not {tracks}")]
    SingleTrack {
        /// The tracks given.
        tracks: usize,
    },
    /// More tracks than the header's 16-bit count holds.
    #[error("{tracks} tracks are more than a file's 16-bit track count")]
    TooManyTracks {
        /// The tracks given.
        tracks: usize,
    },
    /// An SMF track with a configuration header, which only a Clip File has.
    #[error("track {track}: an SMF has no configuration header")]
    HeaderInSmf {
        /// The track.
        track: usize,
    },
    /// An event is before the one ahead of it.
    #[error("track {track}: events out of order at tick {}", at.0)]
    OutOfOrder {
        /// The track.
        track: usize,
        /// The tick that went backwards.
        at: Tick,
    },
    /// Two events are further apart than an SMF's delta (28 bits). A Clip File
    /// has no such gap.
    #[error("track {track}: the gap before tick {} is wider than the wire holds", at.0)]
    Gap {
        /// The track.
        track: usize,
        /// The tick the gap ends at.
        at: Tick,
    },
    /// A header message is before the one ahead of it.
    #[error("track {track}: the header's messages out of order at tick {}", at.0)]
    HeaderOutOfOrder {
        /// The track.
        track: usize,
        /// The tick that went backwards.
        at: Tick,
    },
    /// The track ends before its last event.
    #[error("track {track}: ends at tick {} before its last event", end.0)]
    EndBeforeLast {
        /// The track.
        track: usize,
        /// Where it ends.
        end: Tick,
    },
    /// The track ends further after its last event than the wire's delta holds.
    #[error("track {track}: ends at tick {}, too long after its last event", end.0)]
    EndTooFar {
        /// The track.
        track: usize,
        /// Where it ends.
        end: Tick,
    },
    /// The header ends before its last message.
    #[error("track {track}: the header ends at tick {} before its last message", end.0)]
    HeaderEndBeforeLast {
        /// The track.
        track: usize,
        /// Where it ends.
        end: Tick,
    },
    /// A Flex Data message (a text of several packets) is interrupted by
    /// another message, or has no End: a message between its Start and End
    /// is only a Continue of it or a System Real Time message (M2-104 section
    /// 7.5.2).
    #[error("a Flex Data message is interrupted or left open at tick {}", at.0)]
    TextInterrupted {
        /// The tick of the message that interrupts it, or of the last event
        /// of the track if it is never ended.
        at: Tick,
    },
    /// The container cannot hold an event.
    #[error("track {track}{}: {refusal}", if *.in_header { " header" } else { "" })]
    Refused {
        /// The track.
        track: usize,
        /// Whether it is a header message, not an event of the sequence.
        in_header: bool,
        /// Which rule it broke.
        refusal: Refusal,
    },
    /// A count or length the writer cannot hold. An SMF's event past 2^28
    /// bytes, chunk past 4 GB or delta past 28 bits is past the format's
    /// fields (SMF 1.0 section 1.1). A Clip File's limit is the writer's own
    /// bound on what it allocates, not M2-116's: the format holds any gap with
    /// Null restarts (section 3.2.2), and the writer refuses a gap whose
    /// restarts alone would be over 4 GB.
    #[error("the file is larger than the writer holds")]
    TooLarge,
}

/// Why a container cannot hold an event ([`ConfigError::Refused`]).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// An SMF holds the first Group only.
    #[error("an SMF holds the first group only")]
    SmfGroup,
    /// An SMF holds MIDI 1.0 channel messages, not packets.
    #[error("an SMF holds MIDI 1.0 channel messages, not packets")]
    SmfPacket,
    /// A Clip File holds packets, not SMF events (`SysEx`, `Escape`, `Meta`).
    #[error("a Clip File holds packets, not SMF events")]
    ClipSmfEvent,
    /// A Utility message is the Clip File's own structure.
    #[error("a Utility message is the Clip File's own structure")]
    ClipUtility,
    /// Start and End of Clip are the Clip File's own structure.
    #[error("Start of Clip and End of Clip are the Clip File's own structure")]
    ClipStructure,
    /// A MIDI-CI Property Exchange message (M2-116 sections 6.4 and 7.2).
    #[error("a Clip File holds no Property Exchange message (M2-116 sections 6.4 and 7.2)")]
    ClipPropertyExchange,
}

/// An event pushed before the last one of a track
/// ([`Track::push`](crate::Track::push)): events do not go back in time.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("an event at tick {} follows one at tick {}", at.0, last.0)]
pub struct OutOfOrder {
    /// The tick of the last event of the track.
    pub last: Tick,
    /// The earlier tick pushed.
    pub at: Tick,
}
