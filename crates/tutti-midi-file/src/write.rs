//! Writing a [`MidiFile`] to a [`Destination`]: a path staged beside the target
//! and renamed into place, or a writer given the whole file.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::Path;

use tutti_platform::Destination;

use crate::{MidiFile, WriteError};

/// How a [`MidiFile`] is written, from [`MidiFile::write_options`].
///
/// A path replaces a file already there, as [`std::fs::write`] does;
/// [`create_new`](Self::create_new) refuses it instead. A writer has no
/// existing file, so `create_new` does not apply to it.
///
/// For a path, the bytes go to a temporary file in the target's directory,
/// created with the process umask's mode, and are renamed to the target. A
/// failed or crashed write leaves nothing under the target's name and what was
/// there untouched, and a failed write removes its temporary file.
#[derive(Debug)]
#[must_use = "options do nothing until `write` is called"]
pub struct WriteOptions<'a> {
    pub(crate) file: &'a MidiFile,
    pub(crate) create_new: bool,
}

impl WriteOptions<'_> {
    /// Refuses a file already at a path with [`WriteError::Exists`], as
    /// [`OpenOptions::create_new`] does, instead of replacing it. A writer is
    /// unaffected.
    pub fn create_new(mut self) -> Self {
        self.create_new = true;
        self
    }

    /// Serialises the file and writes it to `dst`: a path is staged and
    /// renamed to it, a writer is given the whole file in one `write_all`.
    ///
    /// By default the staged file takes the place of whatever is at the path
    /// (the inode is swapped): it has the default mode and owner, hard links
    /// to the old file are broken, and a symlink at the path is replaced, not
    /// followed.
    ///
    /// With [`create_new`](Self::create_new) the no-clobber rename is the
    /// authority: anything at the path when it runs, however it got there, is
    /// refused and left untouched, and the caller still holds the
    /// [`MidiFile`] to retry.
    ///
    /// # Errors
    ///
    /// [`WriteError::Exists`] if something (a file, a directory or a symlink)
    /// is at the path and `create_new` was called; [`WriteError::Stage`] or
    /// [`WriteError::Rename`] if the staging or the rename fails;
    /// [`WriteError::Writer`] if a writer fails. A filesystem without an
    /// atomic no-replace rename falls back to a hard link; on one with neither
    /// a `create_new` write fails with [`WriteError::Rename`] and never
    /// overwrites.
    pub fn write<'a>(self, dst: impl Into<Destination<'a>>) -> Result<(), WriteError> {
        let Self { file, create_new } = self;
        let bytes = file.encode();
        match dst.into() {
            // Either option refuses an existing file: the destination's or this one's.
            Destination::Path {
                path,
                create_new: refuse,
            } => write_staged(&bytes, &path, create_new || refuse),
            Destination::Writer(writer) => writer.write_all(&bytes).map_err(WriteError::Writer),
        }
    }
}

/// Stages `bytes` beside `path` and renames them onto it, refusing an existing
/// file if `create_new`.
fn write_staged(bytes: &[u8], path: &Path, create_new: bool) -> Result<(), WriteError> {
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
    staged.write_all(bytes).map_err(WriteError::Stage)?;
    staged.as_file().sync_all().map_err(WriteError::Stage)?;
    if create_new {
        staged.persist_noclobber(path).map_err(|e| {
            if e.error.kind() == ErrorKind::AlreadyExists {
                WriteError::Exists {
                    path: path.to_path_buf(),
                }
            } else {
                WriteError::Rename(e.error)
            }
        })?;
    } else {
        staged
            .persist(path)
            .map_err(|e| WriteError::Rename(e.error))?;
    }
    Ok(())
}
