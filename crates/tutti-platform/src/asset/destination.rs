//! Where one write goes.

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Staged;

/// Where a one-shot write goes: a file at a path, or a writer the caller lends.
///
/// A destination is written once, so it needs no reopening and no `Seek`: a
/// writer that must seek is wrapped by the writing code in a buffer of its own,
/// which it hands over whole with `write_all`.
///
/// - A path ([`&Path`](Path) or [`PathBuf`]) is written staged ([`Staged`]): to a
///   temporary beside it, renamed into place when whole, so a failure leaves
///   what was there. It replaces an existing file, as [`std::fs::write`] does;
///   [`create_new`](Self::create_new) refuses one with
///   [`AlreadyExists`](std::io::ErrorKind::AlreadyExists) instead.
/// - A mutable borrow of any [`Write`] is written directly. A `Vec<u8>` is such a
///   writer and is appended to: `&mut Vec<u8>` has no impl of its own, because
///   one would overlap this one, so it takes the generic path.
///
/// ```
/// use std::path::Path;
/// use tutti_platform::Destination;
///
/// let mut bytes: Vec<u8> = Vec::new();
/// let to_vec = Destination::from(&mut bytes);
/// let to_file = Destination::from(Path::new("out.wav"));
/// let to_new_file = Destination::from(Path::new("take.wav")).create_new();
/// # let _ = (to_vec, to_file, to_new_file);
/// ```
pub enum Destination<'a> {
    /// A file at a path, written through its staging: the path is the
    /// staging's [`target`](Staged::target), and [`create_new`](Self::create_new)
    /// is the staging's.
    Path(Staged),
    /// A writer the caller lends, given the whole output.
    Writer(&'a mut dyn Write),
}

impl Destination<'_> {
    /// Refuses a file already at a path instead of replacing it, as
    /// [`OpenOptions::create_new`](std::fs::OpenOptions::create_new) does: the
    /// commit fails with [`AlreadyExists`](std::io::ErrorKind::AlreadyExists)
    /// and the target is untouched. A writer has no existing file, so this
    /// changes nothing for one.
    #[must_use]
    pub fn create_new(self) -> Self {
        match self {
            Self::Path(staged) => Self::Path(staged.create_new()),
            writer @ Self::Writer(_) => writer,
        }
    }
}

impl fmt::Debug for Destination<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(staged) => f.debug_tuple("Path").field(staged).finish(),
            Self::Writer(_) => f.debug_tuple("Writer").finish(),
        }
    }
}

impl From<&Path> for Destination<'_> {
    fn from(path: &Path) -> Self {
        Self::Path(Staged::new(path))
    }
}

impl From<&PathBuf> for Destination<'_> {
    fn from(path: &PathBuf) -> Self {
        Self::Path(Staged::new(path))
    }
}

impl From<PathBuf> for Destination<'_> {
    fn from(path: PathBuf) -> Self {
        Self::Path(Staged::new(&path))
    }
}

impl From<&str> for Destination<'_> {
    fn from(path: &str) -> Self {
        Self::Path(Staged::new(Path::new(path)))
    }
}

impl<'a> From<&'a mut dyn Write> for Destination<'a> {
    fn from(writer: &'a mut dyn Write) -> Self {
        Self::Writer(writer)
    }
}

impl<'a, W: Write> From<&'a mut W> for Destination<'a> {
    fn from(writer: &'a mut W) -> Self {
        Self::Writer(writer)
    }
}

#[cfg(test)]
mod tests;
