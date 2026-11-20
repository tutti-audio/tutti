//! Where one write goes.

use std::borrow::Cow;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where a one-shot write goes: a file at a path, or a writer the caller lends.
///
/// A destination is written once, so it needs no reopening and no `Seek`: a
/// writer that must seek is wrapped by the writing code in a buffer of its own,
/// which it hands over whole with `write_all`.
///
/// - A path ([`&Path`](Path) or [`PathBuf`]) is written staged ([`Staged`](crate::Staged)): to a
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
    /// A file at this path, written staged.
    Path {
        /// The file's path.
        path: Cow<'a, Path>,
        /// Refuse an existing file instead of replacing it
        /// ([`create_new`](Destination::create_new)).
        create_new: bool,
    },
    /// A writer the caller lends for the one write.
    Writer(&'a mut dyn Write),
}

impl Destination<'_> {
    /// Refuses a file that exists instead of replacing it, like
    /// [`OpenOptions::create_new`](std::fs::OpenOptions::create_new): the write
    /// fails with [`AlreadyExists`](std::io::ErrorKind::AlreadyExists) and the
    /// file is untouched. See [`Staged::create_new`](crate::Staged::create_new) for the atomicity.
    ///
    /// A writer has no existing file to refuse, so this changes nothing for one.
    #[must_use]
    pub fn create_new(mut self) -> Self {
        if let Self::Path { create_new, .. } = &mut self {
            *create_new = true;
        }
        self
    }
}

impl fmt::Debug for Destination<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path { path, create_new } => f
                .debug_struct("Path")
                .field("path", path)
                .field("create_new", create_new)
                .finish(),
            Self::Writer(_) => f.debug_tuple("Writer").finish(),
        }
    }
}

impl<'a> From<&'a Path> for Destination<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path {
            path: Cow::Borrowed(path),
            create_new: false,
        }
    }
}

impl<'a> From<&'a PathBuf> for Destination<'a> {
    fn from(path: &'a PathBuf) -> Self {
        Self::Path {
            path: Cow::Borrowed(path),
            create_new: false,
        }
    }
}

impl From<PathBuf> for Destination<'_> {
    fn from(path: PathBuf) -> Self {
        Self::Path {
            path: Cow::Owned(path),
            create_new: false,
        }
    }
}

impl<'a> From<&'a str> for Destination<'a> {
    fn from(path: &'a str) -> Self {
        Self::Path {
            path: Cow::Borrowed(Path::new(path)),
            create_new: false,
        }
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
