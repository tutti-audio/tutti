//! A file written beside its destination and put in place when whole.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::SystemTime;

/// A temporary file beside a target path: [`commit`](Self::commit) puts it in
/// place, and dropping it first removes it.
///
/// This is how a path [`Destination`](crate::Destination) is written: the bytes go
/// to a temporary in the target's directory, so the rename is within one file
/// system and the target holds either what it held before or the whole new
/// file, never a part.
///
/// - The temporary's name carries the target's name, a process-wide counter (two
///   stagings in one process never share a name) and the wall clock's
///   nanoseconds (two processes staging one target do not share one, unless
///   they stage in the same nanosecond with the same count).
/// - [`commit`](Self::commit) replaces an existing target by default, as
///   [`std::fs::write`] does; a symlink at the target is replaced, not followed,
///   and the new file has the default permissions. With
///   [`create_new`](Self::create_new) it refuses an existing target instead.
/// - A leftover that cannot be removed on drop is logged, not an error: the
///   write's outcome is already decided.
///
/// ```
/// use std::io::Write;
/// use tutti_platform::Staged;
///
/// # fn main() -> std::io::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let target = dir.path().join("out.txt");
/// let staged = Staged::new(&target);
/// staged.open()?.write_all(b"whole")?;
/// staged.commit()?;
/// assert_eq!(std::fs::read(&target)?, b"whole");
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Staged {
    temp: PathBuf,
    target: PathBuf,
    create_new: bool,
    kept: bool,
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
            kept: false,
        }
    }

    /// Makes [`commit`](Self::commit) refuse a target that exists, whatever
    /// it is (a file, a directory, a symlink), instead of replacing it.
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
    pub fn open(&self) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.temp)
    }

    /// Puts the temporary in place of the target.
    ///
    /// # Errors
    ///
    /// The file system's error when the rename fails, or
    /// [`io::ErrorKind::AlreadyExists`] under [`create_new`](Self::create_new)
    /// when the target exists. Either way the temporary is removed and the
    /// target is untouched.
    pub fn commit(mut self) -> io::Result<()> {
        if self.create_new {
            std::fs::hard_link(&self.temp, &self.target)?;
            // The target is whole and named; only the extra name is left.
            self.remove_temp();
        } else {
            std::fs::rename(&self.temp, &self.target)?;
        }
        self.kept = true;
        Ok(())
    }

    fn remove_temp(&self) {
        match std::fs::remove_file(&self.temp) {
            Ok(()) => {}
            // Never created: the writer failed before it opened the file.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                "platform: could not remove the staged file {}: {e}",
                self.temp.display()
            ),
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.kept {
            self.remove_temp();
        }
    }
}

#[cfg(test)]
mod tests;
