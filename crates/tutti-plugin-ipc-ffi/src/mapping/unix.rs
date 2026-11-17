//! The slab's backing file on unix: a file under shared memory, removed by its
//! creator when its mapping ends.

use super::Open;
use crate::SlabError;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// What ends the name: the creator's carries the path and unlinks it on drop,
/// which leaves a peer's mapping intact; an opener's carries nothing. It exists
/// from the moment the creator's file does, so a failure after that unlinks it.
#[derive(Debug)]
pub(super) struct Backing(Option<PathBuf>);

impl Drop for Backing {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Creates or opens the slab file, with the [`Backing`] that ends it.
pub(super) fn open_file(name: &str, size: usize, mode: Open) -> Result<(File, Backing), SlabError> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    match mode {
        Open::Create => {
            // Exclusive (`O_CREAT | O_EXCL`): an existing name, a symlink
            // included, is refused. Truncating one would shrink a file that
            // another process may have mapped, which faults it (SIGBUS), and a
            // planted file or link would put our audio where its owner chose.
            opts.create_new(true).mode(0o600);
        }
        Open::Existing => {
            opts.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        }
    }
    let file = opts
        .open(path(name))
        .map_err(|e| SlabError::File(format!("failed to open shared memory file: {e}")))?;
    let backing = Backing(matches!(mode, Open::Create).then(|| path(name)));
    if matches!(mode, Open::Create) {
        file.set_len(size as u64)
            .map_err(|e| SlabError::File(format!("failed to set file size: {e}")))?;
    }
    Ok((file, backing))
}

pub(super) fn path(name: &str) -> PathBuf {
    #[cfg(target_os = "linux")]
    let base = PathBuf::from("/dev/shm");
    #[cfg(target_os = "macos")]
    let base = std::env::temp_dir();
    base.join(format!("tutti_{name}"))
}
