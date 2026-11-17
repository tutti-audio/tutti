//! What a read or a write of a file refused: [`ReadError`] and [`WriteError`].

use std::path::PathBuf;

use thiserror::Error;

use crate::{ClipFileError, SmfParseError, TracksError};

/// Why a MIDI file could not be read ([`MidiFile::from_bytes`](crate::MidiFile::from_bytes),
/// [`MidiFile::read`](crate::MidiFile::read)).
///
/// A file that is cut off is an error and never a shorter file: what was read
/// before the end is not returned.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ReadError {
    /// The file at the path could not be read.
    #[error("could not read the file")]
    Io(#[source] std::io::Error),

    /// The leading bytes match neither container's magic, so no decoder ran.
    /// Distinct from a parse failure: "this is not a MIDI file" is a different
    /// thing to fix than "this MIDI file is malformed".
    #[error("not a Standard MIDI File or a MIDI 2.0 Clip File")]
    UnknownFormat,

    /// SMPTE/timecode division. The codec reads metrical timing only: beats
    /// come from the file's division, and a timecode file has no beat grid to
    /// read them from or put a tempo map on.
    #[error("an SMPTE division is not supported: the codec reads metrical timing only")]
    SmpteDivision,

    /// The bytes are not a well-formed SMF, or are cut off.
    #[error("SMF{} at byte {offset}: {cause}", track.map_or_else(String::new, |t| format!(" track {t}")))]
    Smf {
        /// The `MTrk` chunk (counted from 0, alien chunks not counted) the
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
        /// refused began to match.
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

/// Why a file could not be written to a path ([`MidiFile::write`](crate::MidiFile::write),
/// [`WriteOptions::write`](crate::WriteOptions::write)).
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// Something (a file, a directory or a symlink) is at the path and the
    /// write was not told to replace it
    /// ([`WriteOptions::replace`](crate::WriteOptions::replace)); it is
    /// untouched.
    #[error("{} already exists", path.display())]
    Exists {
        /// The path that is taken.
        path: PathBuf,
    },

    /// Creating, writing or syncing the temporary file beside the target
    /// failed.
    #[error("could not stage the file beside the target")]
    Stage(#[source] std::io::Error),

    /// Renaming the temporary file onto the target failed.
    #[error("could not rename the staged file onto the target")]
    Rename(#[source] std::io::Error),
}
