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
/// - A path ([`&Path`](Path) or [`PathBuf`]) is written staged: to a temporary
///   beside it, renamed into place when whole, so a failure leaves what was
///   there.
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
/// # let _ = (to_vec, to_file);
/// ```
pub enum Destination<'a> {
    /// A file at this path, written staged.
    Path(Cow<'a, Path>),
    /// A writer the caller lends for the one write.
    Writer(&'a mut dyn Write),
}

impl fmt::Debug for Destination<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => f.debug_tuple("Path").field(path).finish(),
            Self::Writer(_) => f.debug_tuple("Writer").finish(),
        }
    }
}

impl<'a> From<&'a Path> for Destination<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path(Cow::Borrowed(path))
    }
}

impl<'a> From<&'a PathBuf> for Destination<'a> {
    fn from(path: &'a PathBuf) -> Self {
        Self::Path(Cow::Borrowed(path))
    }
}

impl From<PathBuf> for Destination<'_> {
    fn from(path: PathBuf) -> Self {
        Self::Path(Cow::Owned(path))
    }
}

impl<'a> From<&'a str> for Destination<'a> {
    fn from(path: &'a str) -> Self {
        Self::Path(Cow::Borrowed(Path::new(path)))
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
