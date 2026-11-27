//! [`MidiFile`]: a file whose kind the caller does not know, read from its
//! bytes.

use std::io::{ErrorKind, Write as _};

use tutti_platform::{Asset, WriteTarget};

use crate::{ClipFile, MidiFileKind, ReadError, StandardMidiFile, WriteError};

/// A MIDI file of the kind its bytes say: a [`StandardMidiFile`] or a
/// [`ClipFile`].
///
/// `.mid` is worn by both kinds alike, so the kind is decided by the leading
/// bytes ([`MidiFileKind::detect`]), never by the name. This is the one place
/// that does not know the kind; the app matches it and holds the kind's own
/// type from there. A caller who knows the kind reads it as that type
/// ([`StandardMidiFile::from_bytes`], [`ClipFile::from_bytes`]).
///
/// ```
/// use tutti_midi_file::{Layout, LeadTrack, MidiFile, SmfDivision, StandardMidiFile, Track};
///
/// let file = StandardMidiFile::new(
///     SmfDivision::per_quarter(480),
///     Layout::Format0(LeadTrack::new(Track::new())),
/// )?;
/// let mut bytes = Vec::new();
/// file.write(&mut bytes)?;
/// match MidiFile::read(bytes)? {
///     MidiFile::Standard(read) => assert_eq!(read, file),
///     MidiFile::Clip(_) => unreachable!("the bytes begin with MThd"),
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// A `match` on it is exhaustive over the two kinds of MIDI file and takes no
/// wildcard arm.
#[derive(Clone, Debug, PartialEq)]
pub enum MidiFile {
    /// A Standard MIDI File, whose bytes begin with the header chunk id.
    Standard(StandardMidiFile),
    /// A MIDI 2.0 Clip File, whose bytes begin with `SMF2CLIP`.
    Clip(ClipFile),
}

impl MidiFile {
    /// The extensions a MIDI file of either kind is found under.
    ///
    /// `.mid` and `.midi` are the Standard MIDI File's; a Clip File "shall use
    /// the file extension .midi2" (M2-116 section 4.1). A reader handles all
    /// three the same way, detecting rather than trusting any of them.
    pub const EXTENSIONS: &'static [&'static str] = &["mid", "midi", "midi2"];

    /// Reads and decodes a complete MIDI file from `src` (any `impl Into<Asset>`:
    /// a path, bytes or an app's own source), detecting the kind from its
    /// magic: the header chunk id for a Standard MIDI File, `SMF2CLIP` for a Clip File.
    ///
    /// # Errors
    ///
    /// - [`ReadError::Io`] if the source cannot be opened or read.
    /// - [`ReadError::UnknownFormat`] when the magic matches neither kind (no
    ///   decoder ran).
    /// - [`ReadError::Smf`] for a Standard MIDI File that does not read, the
    ///   [`SmfReadError`](crate::SmfReadError) naming why.
    /// - [`ReadError::Clip`] for a Clip File that does not read, the
    ///   [`ClipReadError`](crate::ClipReadError) naming why.
    pub fn read(src: impl Into<Asset>) -> Result<Self, ReadError> {
        use std::io::Read;

        let mut bytes = Vec::new();
        src.into()
            .open()
            .and_then(|mut reader| reader.read_to_end(&mut bytes))
            .map_err(ReadError::Io)?;
        Self::from_bytes(&bytes)
    }

    /// Decodes `bytes`, detecting the kind from its magic.
    ///
    /// # Errors
    ///
    /// As [`read`](Self::read), but for the source.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ReadError> {
        match MidiFileKind::of_header(bytes) {
            Some(MidiFileKind::StandardMidiFile) => StandardMidiFile::from_bytes(bytes)
                .map(Self::Standard)
                .map_err(ReadError::Smf),
            Some(MidiFileKind::ClipFile) => ClipFile::from_bytes(bytes)
                .map(Self::Clip)
                .map_err(ReadError::Clip),
            None => Err(ReadError::UnknownFormat),
        }
    }
}

/// Puts `bytes`, a whole file, in `dst`.
///
/// A path is written through [`Staged`](tutti_platform::Staged): the bytes go
/// to a temporary file beside the path and are put in place when whole.
pub(crate) fn write_bytes(bytes: &[u8], dst: WriteTarget<'_>) -> Result<(), WriteError> {
    match dst {
        WriteTarget::Path(staged) => {
            let path = staged.target().to_path_buf();
            let mut file = staged.open().map_err(WriteError::Stage)?;
            file.write_all(bytes).map_err(WriteError::Stage)?;
            file.commit().map_err(|e| {
                // Only a refused `create_new` commit is `AlreadyExists`.
                if e.kind() == ErrorKind::AlreadyExists {
                    WriteError::Exists { path, source: e }
                } else {
                    WriteError::Commit(e)
                }
            })
        }
        WriteTarget::Writer(writer) => writer.write_all(bytes).map_err(WriteError::Writer),
    }
}
