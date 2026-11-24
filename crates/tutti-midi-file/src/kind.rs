//! [`MidiFileKind`]: which of the two containers a byte stream holds, decided
//! by magic rather than by name.
//!
//! Extension is not a reliable discriminator: `.mid` and `.midi` are used for
//! both formats in the wild, and M2-116 section 5 registers `.midi2` without
//! retiring them. Both formats are self-identifying by magic, so
//! [`MidiFileKind::detect`] reads the leading bytes and answers from the
//! content.

use tutti_platform::Asset;

use crate::clip_file::CLIP_FILE_MAGIC;

/// The 4-byte Standard MIDI File header chunk id (SMF spec §2.1). A clip file's
/// magic is `"SMF2CLIP"`, so the two never collide on their first four bytes.
const SMF_MAGIC: [u8; 4] = *b"MThd";

/// Which MIDI file format a byte stream holds, decided by magic rather than by
/// file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MidiFileKind {
    /// A Standard MIDI File — leading `MThd`. Read with [`StandardMidiFile::from_bytes`](crate::StandardMidiFile::from_bytes).
    StandardMidiFile,
    /// A MIDI 2.0 Clip File — leading `SMF2CLIP`. Read with [`ClipFile::from_bytes`](crate::ClipFile::from_bytes).
    ClipFile,
}

impl MidiFileKind {
    /// Identifies the container of `src` (any `impl Into<Asset>`: a path, bytes
    /// or an app's own source) by reading only its header, not the whole
    /// source. `Ok(None)` means readable, but neither format.
    ///
    /// # Errors
    ///
    /// The [`std::io::Error`] if the source cannot be opened or read. A source
    /// shorter than a header is not an error; it returns `Ok(None)`.
    pub fn detect(src: impl Into<Asset>) -> std::io::Result<Option<Self>> {
        use std::io::Read;

        // `read_to_end` over `take(8)` reads until the eight bytes or the end
        // of the source, retrying an interrupted read: a short read is not the
        // end.
        let mut header = Vec::with_capacity(8);
        src.into().open()?.take(8).read_to_end(&mut header)?;
        Ok(Self::of_header(&header))
    }

    /// Identifies a byte stream by its magic: at most the leading 8 bytes,
    /// never the body. `None` if it is neither format (including a stream too
    /// short to tell).
    pub(crate) fn of_header(bytes: &[u8]) -> Option<Self> {
        if bytes.len() >= 8 && bytes[..8] == CLIP_FILE_MAGIC {
            Some(Self::ClipFile)
        } else if bytes.len() >= 4 && bytes[..4] == SMF_MAGIC {
            Some(Self::StandardMidiFile)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;
