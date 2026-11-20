//! The slab on macOS: a POSIX shared memory object, unlinked as soon as it is
//! sized.
//!
//! The object has a name only for the instant between `shm_open` and
//! `shm_unlink`, so nothing opens it by name and nothing outlives its holders.
//! It is memory, not a file the kernel writes back. There are no seals on this
//! platform; the object is sized by one `ftruncate`.

use crate::SlabError;
use crate::descriptor::{self, duplicate, file_error};
use rustix::fs::{Mode, ftruncate};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::RecvFlags;
use rustix::shm::{OFlags, open as shm_open, unlink as shm_unlink};
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// `PSHMNAMLEN`: the longest name `shm_open` takes, the slash included.
const NAME_MAX: usize = 31;

/// How long [`receive`] waits for the descriptor's byte once the frame that
/// announces it has been read (a guard against a hang, not a measure).
const DESCRIPTOR_PATIENCE: Duration = Duration::from_secs(2);

/// What the slab's owner holds, and what a peer receives: the shared memory
/// object.
pub(super) type Slab = OwnedFd;

pub use crate::descriptor::{Connection, SlabWire};

/// What the platform keeps beside the mapping: nothing.
pub(super) type Backing = ();

/// Creates a shared memory object `size` bytes long, already unlinked.
pub(super) fn create(size: usize) -> Result<(File, Backing, Slab), SlabError> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).map_err(SlabError::Name)?;
    // The process id, a count and thirty-two random bits, in hex: at most
    // 3 + 8 + 1 + 4 + 1 + 8 characters.
    let name = format!(
        "/tt{:x}-{:x}-{:08x}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed) & 0xffff,
        u32::from_ne_bytes(bytes),
    );
    debug_assert!(name.len() <= NAME_MAX, "{name} is over PSHMNAMLEN");
    let fd = shm_open(
        &name,
        OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(file_error("create"))?;
    // The object is sized before it is unlinked, and the name is dropped
    // whether or not the size took, so a failure leaves nothing named.
    let sized = ftruncate(&fd, size as u64);
    let unlinked = shm_unlink(&name);
    sized.map_err(file_error("set the size of"))?;
    unlinked.map_err(file_error("unlink"))?;
    fcntl_setfd(&fd, FdFlags::CLOEXEC).map_err(file_error("close on exec"))?;
    Ok((duplicate(&fd)?, (), fd))
}

/// The peer's slab as a file to map.
pub(super) fn open(slab: &Slab) -> Result<(File, Backing), SlabError> {
    Ok((duplicate(slab)?, ()))
}

/// Nothing to check beyond the length the caller checks: this platform has no
/// seals to look for.
pub(super) fn verify(_: &File, _: usize) -> Result<(), SlabError> {
    Ok(())
}

pub(super) fn wire(_: &Slab) -> SlabWire {
    SlabWire
}

/// Sends `slab` over `conn`, after the frame that announces it.
pub(super) fn send(conn: Connection<'_>, slab: &Slab) -> std::io::Result<()> {
    descriptor::send(conn, slab.as_fd())
}

/// Receives the slab a frame announced, over `conn`.
pub(super) fn receive(conn: Connection<'_>, _: SlabWire) -> Result<Slab, SlabError> {
    let fd = descriptor::recv(conn, RecvFlags::empty(), DESCRIPTOR_PATIENCE)?;
    fcntl_setfd(&fd, FdFlags::CLOEXEC).map_err(|e| SlabError::Transfer { source: e.into() })?;
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::super::SlabHandle;
    use super::*;
    use crate::MappingWriter;
    use crate::header::SLAB_HEADER_BYTES;
    use rustix::fs::fstat;

    const SIZE: usize = SLAB_HEADER_BYTES + 64;

    /// **The slab has no name**: `shm_unlink` has run by the time `create`
    /// returns, so no name opens it and nothing outlives its holders.
    #[test]
    fn the_slab_has_no_name() {
        let (_w, handle) = MappingWriter::create(SIZE).unwrap();
        assert_eq!(fstat(&handle.0).unwrap().st_nlink, 0);
    }

    /// The length a layout asks for is the length the object has.
    #[test]
    fn the_object_has_the_size_it_was_made_with() {
        let (_w, handle) = MappingWriter::create(SIZE).unwrap();
        assert_eq!(fstat(&handle.0).unwrap().st_size, SIZE as i64);
    }
}
