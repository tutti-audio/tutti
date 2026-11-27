//! [`ToFile`]: a path, what is written there, and what happens to what is at it.

use crate::{EncoderOptions, Encoding, FileEncoder, Result};
use std::path::{Path, PathBuf};

/// A file: the [`Encoding`] it is written as, the path it lands at, and the options of a file
/// ([`FileOptions`](crate::FileOptions)'s). An existing file is refused by default
/// ([`EncodeError::Exists`](crate::EncodeError::Exists)) and replaced with
/// [`replace`](Self::replace); the file is written beside its path and put in place when whole by
/// default, and written at its path from the start with [`in_place`](Self::in_place).
#[derive(Debug, Clone, PartialEq)]
#[must_use = "a destination does nothing until `write` or a `Chain` takes it"]
pub struct ToFile {
    path: PathBuf,
    encoding: Encoding,
    replace: bool,
    in_place: bool,
}

impl ToFile {
    /// A file at `path` written as `encoding`, staged and refusing an existing path.
    pub fn new(path: impl Into<PathBuf>, encoding: Encoding) -> Self {
        Self {
            path: path.into(),
            encoding,
            replace: false,
            in_place: false,
        }
    }

    /// Replaces a path that already exists instead of refusing it.
    pub fn replace(mut self) -> Self {
        self.replace = true;
        self
    }

    /// Writes the file at its path from the start, not beside it.
    pub fn in_place(mut self) -> Self {
        self.in_place = true;
        self
    }

    /// Where the file lands.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How the file is written.
    #[must_use]
    pub fn encoding(&self) -> &Encoding {
        &self.encoding
    }

    /// The same file, written as `f` makes of its encoding.
    pub fn map_encoding(mut self, f: impl FnOnce(Encoding) -> Encoding) -> Self {
        self.encoding = f(self.encoding);
        self
    }

    /// A file at `path` written in this one's format, with the channels and the rate of
    /// whatever it is given, and the options of a new file.
    pub fn alongside(&self, path: impl Into<PathBuf>) -> Self {
        Self::new(path, Encoding::new(self.encoding.format()))
    }

    /// The file, begun for `spec`, with `options`' length and seed.
    pub(crate) fn create(&self, options: EncoderOptions<'_>) -> Result<FileEncoder> {
        let file = options.file(&self.path);
        let file = if self.replace { file.replace() } else { file };
        let file = if self.in_place { file.in_place() } else { file };
        file.create()
    }
}
