//! What a read or a write of a file refused: [`ReadError`] and its kinds' errors,
//! the errors of building a Clip File, and [`WriteError`].

use std::path::PathBuf;

use thiserror::Error;
use tutti_platform::StagedCommitError;

use crate::{ClipFileError, SmfParseError, Tick, TracksError};

/// Why a MIDI file of a kind the caller did not know could not be read
/// ([`MidiFile::read`](crate::MidiFile::read)).
///
/// A file that is cut off is an error and never a shorter file: what was read
/// before the end is not returned.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ReadError {
    /// The source could not be opened or read.
    #[error("could not read the file")]
    Io(#[source] std::io::Error),

    /// The leading bytes match neither kind's magic, so no decoder ran.
    /// Distinct from a parse failure: "this is not a MIDI file" is a different
    /// thing to fix than "this MIDI file is malformed".
    #[error("not a Standard MIDI File or a MIDI 2.0 Clip File")]
    UnknownFormat,

    /// The bytes are a Standard MIDI File that does not read.
    #[error(transparent)]
    Smf(#[from] SmfReadError),

    /// The bytes are a Clip File that does not read.
    #[error(transparent)]
    Clip(#[from] ClipReadError),
}

/// Why a Standard MIDI File could not be read
/// ([`StandardMidiFile::from_bytes`](crate::StandardMidiFile::from_bytes)).
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SmfReadError {
    /// The bytes are not a well-formed SMF, or are cut off.
    #[error("SMF{} at byte {offset}: {cause}", track.map_or_else(String::new, |t| format!(" track {t}")))]
    Parse {
        /// The track chunk (counted from 0, alien chunks not counted) the
        /// event is in; `None` for the header and the chunk framing.
        track: Option<usize>,
        /// The byte offset in the file where the header, chunk or event that
        /// was refused starts; for a file with fewer tracks than its header
        /// declares, the length of the file.
        offset: usize,
        /// What was wrong.
        cause: SmfParseError,
    },

    /// The decoded tracks are refused as the constructor refuses them: tempo
    /// events that do not make a tempo map, or a file past the format's
    /// length fields.
    #[error(transparent)]
    Tracks(#[from] TracksError),
}

/// Why a Clip File could not be read
/// ([`ClipFile::from_bytes`](crate::ClipFile::from_bytes)).
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClipReadError {
    /// The bytes are not a well-formed Clip File, or are cut off.
    #[error("clip file at word {word}: {cause}")]
    Parse {
        /// The index of the 32-bit word (after the magic) where the rule that
        /// refused began to match: the Delta Clockstamps before a message
        /// belong to it. A rule that ran out of packets without a start is
        /// at the word the input had reached.
        word: usize,
        /// The rule broken.
        cause: ClipFileError,
    },

    /// The decoded messages are refused as the constructor refuses them: tempo
    /// messages that do not make a tempo map, or a clip past the writer's
    /// bound.
    #[error(transparent)]
    Build(#[from] ClipBuildError),
}

/// Which part of a Clip File an error is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClipPart {
    /// The configuration header (M2-116 section 6), on a clock of its own.
    Header,
    /// The Sequence Data (section 7).
    Sequence,
}

impl std::fmt::Display for ClipPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Header => "header",
            Self::Sequence => "sequence",
        })
    }
}

/// Why the header and sequence cannot make a Clip File
/// ([`ClipFile::new`](crate::ClipFile::new), and a read): each variant is one
/// rule of M2-116, with where it was broken.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClipBuildError {
    /// The messages break a rule of the format that a read enforces as well. The
    /// [`ClipFileError`] is one of the header's rules (M2-116 sections 6 to
    /// 6.2): [`DuplicateHeaderTempo`](ClipFileError::DuplicateHeaderTempo),
    /// [`MisplacedHeaderTempo`](ClipFileError::MisplacedHeaderTempo),
    /// [`DuplicateHeaderTimeSignature`](ClipFileError::DuplicateHeaderTimeSignature),
    /// [`MisplacedHeaderTimeSignature`](ClipFileError::MisplacedHeaderTimeSignature)
    /// or [`ProfileOn`](ClipFileError::ProfileOn); or
    /// [`PropertyExchange`](ClipFileError::PropertyExchange), a MIDI-CI Property
    /// Exchange message in the header or the sequence (sections 6.4 and 7.2). A
    /// build raises no other variant: the rest are rules of the byte stream.
    #[error("the clip is not well formed")]
    Format(#[from] ClipFileError),
    /// A Flex Data message (a text of several packets) is interrupted by
    /// another message, or has no End: a message between its Start and End is
    /// only a Continue of it or a System Real Time message (M2-104 section
    /// 7.5.2).
    #[error("{part}: a Flex Data message is interrupted or left open at tick {}", at.0)]
    TextInterrupted {
        /// The part it is in.
        part: ClipPart,
        /// The tick of the message that interrupts it, or of the last message
        /// of the part if it is never ended.
        at: Tick,
    },
    /// The Clip File cannot hold a message.
    #[error("{part}: {refusal}")]
    Refused {
        /// The part it is in.
        part: ClipPart,
        /// Which rule it broke.
        refusal: ClipRefusal,
    },
    /// A gap needs more Null restarts than the writer holds. The writer's
    /// bound on what it allocates is not M2-116's: the format holds any gap
    /// with Null restarts (section 3.2.2), and the writer refuses a gap whose
    /// restarts alone would be over 4 GB.
    #[error("the clip is larger than the writer holds")]
    TooLarge,
    /// The Set Tempo messages do not make a tempo map.
    #[error("the clip's tempo messages do not make a tempo map")]
    Tempo(#[from] tutti_tempo::TempoError),
}

/// Why a Clip File cannot hold a message ([`ClipBuildError::Refused`]).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClipRefusal {
    /// A Utility message is the Clip File's own structure.
    #[error("a Utility message is the Clip File's own structure")]
    Utility,
    /// Start and End of Clip are the Clip File's own structure.
    #[error("Start of Clip and End of Clip are the Clip File's own structure")]
    Structure,
}

/// Why a file could not be written ([`StandardMidiFile::write`](crate::StandardMidiFile::write) and [`ClipFile::write`](crate::ClipFile::write)).
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// Something (a file, a directory or a symlink) is at the path and the
    /// destination was told to
    /// [`create_new`](tutti_platform::WriteTarget::create_new); it is
    /// untouched. The complete file stays at
    /// [`kept`](tutti_platform::StagedCommitError::kept) of the source.
    #[error("{} already exists", path.display())]
    Exists {
        /// The path that is taken.
        path: PathBuf,
        /// The refused commit, which names where the staged file is kept.
        #[source]
        source: StagedCommitError,
    },

    /// The temporary file beside the target could not be created or written.
    /// Nothing is left behind.
    #[error("could not stage the file beside the target")]
    Stage(#[source] std::io::Error),

    /// The sync or the move into place failed. The target is untouched and the
    /// complete file is kept at [`kept`](StagedCommitError::kept).
    #[error(transparent)]
    Commit(#[from] StagedCommitError),

    /// The writer given to the write failed.
    #[error("could not write to the writer")]
    Writer(#[source] std::io::Error),
}
