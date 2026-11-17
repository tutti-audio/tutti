//! Writing a [`MidiFile`] to a path: staged beside the target, renamed into
//! place.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::Path;

use crate::{MidiFile, WriteError};

/// What a write does when `path` already holds a file.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Existing {
    /// The write fails with [`WriteError::Exists`].
    Refuse,
    /// The staged file atomically replaces it.
    Replace,
}

/// How a [`MidiFile`] is written to a path, from [`MidiFile::write_options`].
///
/// The bytes go to a temporary file in the target's directory, created with
/// the process umask's mode, and are renamed to the target. A failed or
/// crashed write leaves nothing under the target's name, and a failed write
/// removes its temporary file.
#[derive(Debug)]
#[must_use = "options do nothing until `write` is called"]
pub struct WriteOptions<'a> {
    pub(crate) file: &'a MidiFile,
    pub(crate) existing: Existing,
}

impl WriteOptions<'_> {
    /// Replaces a file already at the path instead of refusing it.
    pub fn replace(mut self) -> Self {
        self.existing = Existing::Replace;
        self
    }

    /// Serialises the file, stages it and renames it to `path`.
    ///
    /// Without [`replace`](Self::replace) the no-clobber rename is the
    /// authority: anything at `path` when it runs, however it got there, is
    /// refused, and the caller still holds the [`MidiFile`] to retry.
    ///
    /// With `replace` the staged file takes the place of whatever is at
    /// `path` (the inode is swapped): it has the default mode and owner,
    /// hard links to the old file are broken, and a symlink at `path` is
    /// replaced, not followed.
    ///
    /// # Errors
    ///
    /// [`WriteError::Exists`] if something (a file, a directory or a symlink)
    /// is at `path` and `replace` was not called; [`WriteError::Stage`] or
    /// [`WriteError::Rename`] if the staging or the rename fails. A filesystem
    /// without an atomic no-replace rename falls back to a hard link; on one
    /// with neither the write fails with [`WriteError::Rename`] and never
    /// overwrites.
    pub fn write(self, path: impl AsRef<Path>) -> Result<(), WriteError> {
        let Self { file, existing } = self;
        let path = path.as_ref().to_path_buf();
        let bytes = file.to_bytes();
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        // `open` with no explicit mode creates `0o666` under the umask.
        let mut staged = tempfile::Builder::new()
            .make_in(dir, |temp| {
                OpenOptions::new().write(true).create_new(true).open(temp)
            })
            .map_err(WriteError::Stage)?;
        staged.write_all(&bytes).map_err(WriteError::Stage)?;
        staged.as_file().sync_all().map_err(WriteError::Stage)?;
        match existing {
            Existing::Replace => {
                staged
                    .persist(&path)
                    .map_err(|e| WriteError::Rename(e.error))?;
            }
            Existing::Refuse => {
                staged.persist_noclobber(&path).map_err(|e| {
                    if e.error.kind() == ErrorKind::AlreadyExists {
                        WriteError::Exists { path: path.clone() }
                    } else {
                        WriteError::Rename(e.error)
                    }
                })?;
            }
        }
        Ok(())
    }
}
