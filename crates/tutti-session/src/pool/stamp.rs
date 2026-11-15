//! A file's stamp: what the pool compares to tell that a file changed.

use std::io;
use std::path::Path;
use std::time::SystemTime;

/// A file's length and modification time, as its metadata gives them.
///
/// The pool takes one on the worker that loads a file entry, before the load
/// reads the file, so a change during the read shows at the next refresh. It
/// compares stamps only in [`Session::refresh_pool`](crate::Session::refresh_pool);
/// the pool's diff never reads metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    /// The stamp of the file at `path`.
    pub(crate) fn of(path: &Path) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}
