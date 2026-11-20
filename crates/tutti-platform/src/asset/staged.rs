//! A file written beside its destination and put in place when whole.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::SystemTime;

/// A file to be written beside a target path, and put in place when whole.
///
/// This is how a path [`Destination`](crate::Destination) is written: the bytes go
/// to a temporary in the target's directory, so the rename is within one file
/// system and the target holds either what it held before or the whole new
/// file, never a part. [`open`](Self::open) creates the temporary and hands out
/// the [`StagedFile`] that owns it.
///
/// - The temporary's name carries the target's name, a process-wide counter (two
///   stagings in one process never share a name) and the wall clock's
///   nanoseconds (two processes staging one target do not share one, unless
///   they stage in the same nanosecond with the same count).
/// - The commit replaces an existing target by default, as [`std::fs::write`]
///   does; a symlink at the target is replaced, not followed, and the new file
///   has the default permissions. With [`create_new`](Self::create_new) it
///   refuses an existing target instead.
///
/// ```
/// use std::io::Write;
/// use tutti_platform::Staged;
///
/// # fn main() -> std::io::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let target = dir.path().join("out.txt");
/// let mut file = Staged::new(&target).open()?;
/// file.write_all(b"whole")?;
/// file.commit().map_err(|e| e.into_source())?;
/// assert_eq!(std::fs::read(&target)?, b"whole");
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Staged {
    temp: PathBuf,
    target: PathBuf,
    create_new: bool,
}

impl Staged {
    /// A staging for `target`: the temporary's name is chosen, the file is not
    /// yet created ([`open`](Self::open) creates it).
    pub fn new(target: &Path) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = target
            .file_name()
            .map_or_else(|| "export".to_owned(), |n| n.to_string_lossy().into_owned());
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let count = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            temp: target.with_file_name(format!(".{name}.{nanos}.{count}.tmp")),
            target: target.to_path_buf(),
            create_new: false,
        }
    }

    /// Makes the commit refuse a target that exists, whatever it is (a file, a
    /// directory, a symlink), instead of replacing it.
    ///
    /// The refusal is atomic: the commit is a hard link of the temporary to
    /// the target, which the file system refuses when the target exists, so a
    /// file that appears between the staging and the commit is refused too. A
    /// file system without hard links fails the commit and never overwrites.
    #[must_use]
    pub fn create_new(mut self) -> Self {
        self.create_new = true;
        self
    }

    /// The temporary's path.
    pub fn temp(&self) -> &Path {
        &self.temp
    }

    /// Creates the temporary and opens it for writing.
    ///
    /// # Errors
    ///
    /// The file system's error when the file cannot be created, or
    /// [`io::ErrorKind::AlreadyExists`] if one is at the temporary's name.
    pub fn open(self) -> io::Result<StagedFile> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.temp)?;
        Ok(StagedFile {
            file: Some(file),
            staged: self,
            keep: false,
        })
    }
}

/// The open temporary of a [`Staged`]: written like a file, put in place by
/// [`commit`](Self::commit), and removed if dropped first.
///
/// It is unbuffered and implements [`Write`] and [`Seek`]; a caller that
/// writes in small pieces wraps it in a [`BufWriter`](std::io::BufWriter) and
/// takes it back with `into_inner` before the commit. The commit owns the file,
/// so it runs only once the file is closed (an open file cannot be renamed on
/// Windows) and only on a file this staging made. A leftover that cannot be
/// removed on drop is logged, not an error: the write's outcome is already
/// decided. A failed commit keeps the file ([`StagedCommitError`]).
#[derive(Debug)]
#[must_use = "a staged file is removed when dropped; commit it"]
pub struct StagedFile {
    /// `None` only inside `commit` and `drop`, once the file is closed.
    file: Option<File>,
    staged: Staged,
    /// A failed commit's file, which the drop leaves alone.
    keep: bool,
}

impl StagedFile {
    /// The temporary's path.
    pub fn temp(&self) -> &Path {
        &self.staged.temp
    }

    fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("the file is open until commit")
    }

    /// Flushes the file to the disk, closes it and puts it in place of the
    /// target, so that after a crash the target holds every byte written.
    ///
    /// # Errors
    ///
    /// A [`StagedCommitError`] when the sync or the move fails (a locked target on
    /// Windows, a permission change, a file system without hard links), or
    /// with [`io::ErrorKind::AlreadyExists`] under
    /// [`create_new`](Staged::create_new) when the target exists. The target is
    /// untouched, and the complete file is **kept** at
    /// [`kept`](StagedCommitError::kept) for the caller to move or delete: the
    /// bytes may be the only copy.
    pub fn commit(mut self) -> Result<(), StagedCommitError> {
        // Kept unless the move succeeds: only an uncommitted drop removes it.
        self.keep = true;
        let kept = |temp: &Path, source| StagedCommitError {
            kept: temp.to_path_buf(),
            source,
        };
        self.file()
            .sync_all()
            .map_err(|e| kept(&self.staged.temp, e))?;
        self.file = None;
        let Staged {
            temp,
            target,
            create_new,
        } = &self.staged;
        if *create_new {
            std::fs::hard_link(temp, target).map_err(|e| kept(temp, e))?;
            // The target is whole and named; only the extra name is left, and
            // the drop removes it (a failure there is logged, not an error).
            self.keep = false;
        } else {
            std::fs::rename(temp, target).map_err(|e| kept(temp, e))?;
            // Renamed away: there is nothing for the drop to remove.
            self.staged.temp = PathBuf::new();
        }
        Ok(())
    }
}

/// A [`StagedFile::commit`] that failed: the complete file is kept at
/// [`kept`](Self::kept) and the target is untouched.
///
/// The file is the caller's to move or delete; nothing removes it.
#[derive(Debug, Error)]
#[error("could not put the staged file in place; it is kept at {}", kept.display())]
pub struct StagedCommitError {
    kept: PathBuf,
    #[source]
    source: io::Error,
}

impl StagedCommitError {
    /// Where the complete file is kept.
    pub fn kept(&self) -> &Path {
        &self.kept
    }

    /// The file system's error.
    pub fn into_source(self) -> io::Error {
        self.source
    }
}

impl Clone for StagedCommitError {
    /// A copy that keeps the I/O error's kind and message, not its source.
    fn clone(&self) -> Self {
        Self {
            kept: self.kept.clone(),
            source: io::Error::new(self.source.kind(), self.source.to_string()),
        }
    }
}

impl Write for StagedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file().flush()
    }
}

impl Seek for StagedFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file().seek(pos)
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        // Closed first: an open file cannot be removed on Windows.
        self.file = None;
        if self.keep || self.staged.temp.as_os_str().is_empty() {
            return;
        }
        match std::fs::remove_file(&self.staged.temp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                "platform: could not remove the staged file {}: {e}",
                self.staged.temp.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests;
