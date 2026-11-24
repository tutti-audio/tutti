//! What a track list can be refused for: [`TracksError`], and the order a
//! builder holds ([`OutOfOrder`]).

use thiserror::Error;

use crate::Tick;

/// Why the tracks cannot make a Standard MIDI File
/// ([`StandardMidiFile::new`](crate::StandardMidiFile::new), and a read): each
/// variant is one rule of SMF 1.0, with where it was broken. A `track` is an
/// index into the list given.
///
/// [`SysExNotEnded`](Self::SysExNotEnded) and
/// [`SysExInterrupted`](Self::SysExInterrupted) are the rules of SMF 1.0 a
/// writer keeps that no type makes unwritable:
/// [`StandardMidiFile::new`](crate::StandardMidiFile::new) refuses what breaks them. The rest
/// of section 3.3's placement is a [`Layout`](crate::Layout)'s: the Sequence
/// Number, SMPTE Offset and copyright notice are fields of its lead track or
/// patterns, written first at tick 0, and a read makes a file keep it
/// ([`StandardMidiFile::repairs`](crate::StandardMidiFile::repairs)). tutti's writer also
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
    /// Two events are further apart than an SMF's delta (28 bits).
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
    /// A count or length the writer cannot hold: an event past 2^28 bytes or
    /// a chunk past 4 GB is past the format's fields (SMF 1.0 section 1.1).
    #[error("the file is larger than the writer holds")]
    TooLarge,
    /// The tempo events do not make a tempo map.
    #[error("the file's tempo events do not make a tempo map")]
    Tempo(#[from] tutti_tempo::TempoError),
}

/// An event pushed before the last one of a track, a header or a sequence
/// ([`Track::push`](crate::Track::push), [`ClipSequence::push`](crate::ClipSequence::push)):
/// events do not go back in time.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("an event at tick {} follows one at tick {}", at.0, last.0)]
pub struct OutOfOrder {
    /// The tick of the last event of the track.
    pub last: Tick,
    /// The earlier tick pushed.
    pub at: Tick,
}
