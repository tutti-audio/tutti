//! A file being written beside its destination.

use std::path::{Path, PathBuf};
use tutti_platform::SystemTime;

/// A temporary beside `target`: removed when dropped unless
/// [`commit`](Self::commit)ted. Its name carries the target's name, a
/// process-wide counter (two stagings in one process never share a name) and
/// the wall clock's nanoseconds (two processes staging one target do not
/// share one, unless they stage in the same nanosecond with the same count). A leftover that cannot be removed is logged,
/// not an error: the take's outcome is already decided.
pub(crate) struct Staged {
    temp: PathBuf,
    target: PathBuf,
    kept: bool,
}

impl Staged {
    pub(crate) fn beside(target: &Path) -> Self {
        static NEXT: tutti_rt::Mint = tutti_rt::Mint::new();
        let name = target
            .file_name()
            .map_or_else(|| "export".to_owned(), |n| n.to_string_lossy().into_owned());
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let temp = target.with_file_name(format!(".{name}.{nanos}.{}.tmp", NEXT.mint()));
        Self {
            temp,
            target: target.to_path_buf(),
            kept: false,
        }
    }

    pub(crate) fn temp(&self) -> &Path {
        &self.temp
    }

    /// Renames the temporary over the target.
    pub(crate) fn commit(mut self) -> std::io::Result<()> {
        std::fs::rename(&self.temp, &self.target)?;
        self.kept = true;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        match std::fs::remove_file(&self.temp) {
            Ok(()) => {}
            // Never created: the encoder failed before it opened the file.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                "codec: could not remove the unfinished file {}: {e}",
                self.temp.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests;
