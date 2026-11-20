//! What a read or a write of a file refused: [`ReadError`] and [`WriteError`].

use std::path::PathBuf;

use thiserror::Error;
use tutti_platform::StagedCommitError;

use crate::{ClipFileError, SmfParseError, TracksError};

/// Why a MIDI file could not be read ([`MidiFile::read`](crate::MidiFile::read)).
///
/// A file that is cut off is an error and never a shorter file: what was read
/// before the end is not returned.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ReadError {
    /// The source could not be opened or read.
    #[error("could not read the file")]
    Io(#[source] std::io::Error),

    /// The leading bytes match neither container's magic, so no decoder ran.
    /// Distinct from a parse failure: "this is not a MIDI file" is a different
    /// thing to fix than "this MIDI file is malformed".
    #[error("not a Standard MIDI File or a MIDI 2.0 Clip File")]
    UnknownFormat,

    /// The bytes are not a well-formed SMF, or are cut off.
    #[error("SMF{} at byte {offset}: {cause}", track.map_or_else(String::new, |t| format!(" track {t}")))]
    Smf {
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

    /// The bytes are not a well-formed Clip File, or are cut off.
    #[error("clip file at word {word}: {cause}")]
    Clip {
        /// The index of the 32-bit word (after the magic) where the rule that
        /// refused began to match: the Delta Clockstamps before a message
        /// belong to it. A rule that ran out of packets without a start is
        /// at the word the input had reached.
        word: usize,
        /// The rule broken.
        cause: ClipFileError,
    },

    /// The decoded tracks are refused as the constructors refuse them: a Set
    /// Profile On of a Clip File's header that is not one, or tempo events
    /// that do not make a tempo map.
    #[error(transparent)]
    Tracks(#[from] TracksError),
}

/// Why a file could not be written ([`MidiFile::write`](crate::MidiFile::write)).
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// Something (a file, a directory or a symlink) is at the path and the
    /// destination was told to
    /// [`create_new`](tutti_platform::Destination::create_new); it is
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
