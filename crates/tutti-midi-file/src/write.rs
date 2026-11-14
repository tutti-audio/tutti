//! Writing a [`MidiFile`] to a path: staged beside the target, renamed into
//! place.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::{Error, MidiFile, Result};

/// What a write does when `path` already holds a file.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Existing {
    /// The write fails with [`Error::Exists`].
    Refuse,
    /// The staged file atomically replaces it.
    Replace,
}

/// A write of a [`MidiFile`] to a path, begun by [`MidiFile::write`].
///
/// The bytes go to a temporary file in the target's directory, created with
/// the process umask's mode, and are renamed to the target by
/// [`commit`](Self::commit). A failed or crashed write leaves nothing under
/// the target's name, and a failed commit removes its temporary file.
#[derive(Debug)]
#[must_use = "a write does nothing until it is committed"]
pub struct FileWrite<'a> {
    pub(crate) file: &'a MidiFile,
    pub(crate) path: PathBuf,
    pub(crate) existing: Existing,
}

impl FileWrite<'_> {
    /// Replaces a file already at the path instead of refusing it.
    pub fn replace(mut self) -> Self {
        self.existing = Existing::Replace;
        self
    }

    /// Serialises the file, stages it and renames it to the path.
    ///
    /// An existing file is checked for first (a fast refusal); the rename is
    /// the authority, so a file that appears meanwhile is refused too.
    ///
    /// # Errors
    ///
    /// [`Error::Exists`] if the path holds a file and [`replace`](Self::replace)
    /// was not called; [`Error::Io`] if the staging or the rename fails;
    /// otherwise as [`MidiFile::to_bytes`].
    pub fn commit(self) -> Result<()> {
        let Self {
            file,
            path,
            existing,
        } = self;
        let exists = || Error::Exists { path: path.clone() };
        if matches!(existing, Existing::Refuse) && path.symlink_metadata().is_ok() {
            return Err(exists());
        }
        let bytes = file.to_bytes()?;
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        // `open` with no explicit mode creates `0o666` under the umask.
        let mut staged = tempfile::Builder::new().make_in(dir, |temp| {
            OpenOptions::new().write(true).create_new(true).open(temp)
        })?;
        staged.write_all(&bytes)?;
        staged.as_file().sync_all()?;
        match existing {
            Existing::Replace => {
                staged.persist(&path).map_err(|e| e.error)?;
            }
            Existing::Refuse => {
                staged.persist_noclobber(&path).map_err(|e| {
                    if e.error.kind() == ErrorKind::AlreadyExists {
                        exists()
                    } else {
                        Error::Io(e.error)
                    }
                })?;
            }
        }
        Ok(())
    }
}
