//! A file's stamp: what a cache compares to tell that a file changed.

use std::io;
use std::path::Path;
use std::time::SystemTime;

/// A file's length and modification time, as its metadata gives them.
///
/// Two stamps of one path are equal when the file did not change between them,
/// as far as its length and modification time can tell. A stamp is taken before
/// the file is read, so a change during the read shows at the next stamp.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    /// The stamp of the file at `path`.
    ///
    /// # Errors
    ///
    /// The metadata call's error: the path does not exist, or is unreadable. A
    /// target with no filesystem (the web) always fails.
    pub fn of(path: &Path) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}
