//! What a track list can be refused for: [`TracksError`], and the order a
//! [`Track`](crate::Track) builder holds ([`OutOfOrder`]).

use thiserror::Error;

use crate::{ClipFileError, Tick};

/// Which part of a track an error is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrackPart {
    /// A Clip File's configuration header, on a clock of its own.
    Header,
    /// The events of the sequence.
    Events,
}

impl std::fmt::Display for TrackPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Header => "header",
            Self::Events => "events",
        })
    }
}

/// Why the tracks cannot make a file of the container asked for
/// ([`MidiFile::smf`](crate::MidiFile::smf), [`MidiFile::clip`](crate::MidiFile::clip),
/// and a read): each variant is one rule of the container, with where it was
/// broken. A `track` is an index into the list given.
///
/// [`SysExNotEnded`](Self::SysExNotEnded) and
/// [`SysExInterrupted`](Self::SysExInterrupted) are the rules of SMF 1.0 a
/// writer keeps that no type makes unwritable:
/// [`MidiFile::smf`](crate::MidiFile::smf) refuses what breaks them. The rest
/// of section 3.3's placement is a [`Layout`](crate::Layout)'s: the Sequence
/// Number, SMPTE Offset and copyright notice are fields of its lead track or
/// patterns, written first at tick 0, and a read makes a file keep it
/// ([`MidiFile::repairs`](crate::MidiFile::repairs)). tutti's writer also
/// refuses what the specification only recommends: the format 1 tempo map
/// outside the first track
/// ([`TempoMapNotInFirstTrack`](Self::TempoMapNotInFirstTrack)).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TracksError {
    /// The track list is empty.
    #[error("a file needs at least one track")]
    NoTracks,
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
    /// Two events are further apart than an SMF's delta (28 bits). A Clip File
    /// has no such gap.
    #[error("track {track}: the gap before tick {} is wider than the wire holds", at.0)]
    Gap {
        /// The track.
        track: usize,
        /// The tick the gap ends at.
        at: Tick,
    },
    /// The track ends further after its last event than the wire's delta holds.
    #[error("track {track}: ends at tick {}, too long after its last event", end.0)]
    EndTooFar {
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
    /// A divided System Exclusive message that is never ended by a packet ending
    /// in `F7` (SMF 1.0 section 3: its last packet ends with it, so the reader
    /// knows it has read all of it).
    #[error("track {track}: the divided SysEx message at tick {} does not end in F7", at.0)]
    SysExNotEnded {
        /// The track.
        track: usize,
        /// The tick of its first packet.
        at: Tick,
    },
    /// A channel message or an `F0` event between the packets of a divided
    /// System Exclusive message (SMF 1.0 section 3: no transmittable MIDI event
    /// goes between them).
    #[error("track {track}: an event at tick {} interrupts a divided SysEx message", at.0)]
    SysExInterrupted {
        /// The track.
        track: usize,
        /// The tick of the event.
        at: Tick,
    },
    /// A Set Tempo or Time Signature in a track of a format 1 file other than
    /// the first (SMF 1.0 section 2.2: the tempo map is stored as the first
    /// track).
    #[error("track {track}: the tempo map at tick {} belongs in the first track", at.0)]
    TempoMapNotInFirstTrack {
        /// The track.
        track: usize,
        /// The tick of the event.
        at: Tick,
    },
    /// The container cannot hold an event.
    #[error("track {track} {part}: {refusal}")]
    Refused {
        /// The track.
        track: usize,
        /// The part of it.
        part: TrackPart,
        /// Which rule it broke.
        refusal: Refusal,
    },
    /// A count or length the writer cannot hold. An SMF's event past 2^28
    /// bytes or chunk past 4 GB is past the format's fields (SMF 1.0 section
    /// 1.1). A Clip File's limit is the writer's own bound on what it
    /// allocates, not M2-116's: the format holds any gap with Null restarts
    /// (section 3.2.2), and the writer refuses a gap whose restarts alone
    /// would be over 4 GB.
    #[error("the file is larger than the writer holds")]
    TooLarge,
    /// A Clip File's header breaks a rule of M2-116 sections 6 to 6.2.
    #[error("the clip header is not well formed")]
    Header(#[from] ClipFileError),
    /// The tempo events do not make a tempo map.
    #[error("the file's tempo events do not make a tempo map")]
    Tempo(#[from] tutti_tempo::TempoError),
}

/// Why a container cannot hold an event ([`TracksError::Refused`]).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Refusal {
    /// An SMF holds the first Group only.
    #[error("an SMF holds the first group only")]
    SmfGroup,
    /// An SMF holds MIDI 1.0 channel messages, not packets.
    #[error("an SMF holds MIDI 1.0 channel messages, not packets")]
    SmfPacket,
    /// A Clip File holds packets, not SMF events (`SysEx`, `Escape`, `Meta`) or
    /// the fields of an SMF track (its Sequence Number, name, SMPTE Offset and
    /// copyright notice).
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
