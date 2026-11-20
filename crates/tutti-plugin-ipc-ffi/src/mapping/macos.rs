//! The slab on macOS: a POSIX shared memory object, unlinked as soon as it is
//! sized.
//!
//! The object has a name only for the instant between `shm_open` and
//! `shm_unlink`, so nothing opens it by name and nothing outlives its holders.
//! It is memory, not a file the kernel writes back. It supports neither `read` nor
//! `write` (both are `ENXIO`), so a blob is written and read through a mapping,
//! which is sound because XNU refuses a second `ftruncate` (the tests pin it): no
//! holder can shrink the object under the mapping. A descriptor that is not a
//! shared memory object is refused, a regular file included, because its holder
//! could shrink it under the mapping. There are no seals on this
//! platform; the object is sized by one `ftruncate`, and the tests pin that no
//! later `ftruncate` is accepted.
//!
//! XNU rounds the object up to a page (16 KiB on Apple Silicon), so `fstat` reports
//! the rounded length. The receiver refuses a layout that needs more than that
//! length, which keeps everything it maps backed (no `SIGBUS`), and both ends map
//! exactly the layout's length. It cannot tell a layout that is short but fits in
//! the rounded page from the one the object was made for: the header's magic,
//! version and ring depth are the guard against a mismatched layout.
//!
//! The descriptor is made close-on-exec by a call right after `shm_open` and after
//! `recvmsg` (neither takes the flag atomically here). A fork and exec on another
//! thread of the process between the two calls would inherit it. The host's one
//! spawn site is `launch`'s `Command::spawn` of a plugin server, which can run on
//! another bridge's thread while a slab is being made, so that window is real on the
//! host; it is one syscall wide, and the server's receive window is exposed only to
//! a plugin's own threads.

use crate::SlabError;
use crate::descriptor::{self, duplicate, file_error};
use memmap2::MmapOptions;
use rustix::fs::{Mode, ftruncate};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::RecvFlags;
use rustix::shm::{OFlags, open as shm_open, unlink as shm_unlink};
use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
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

/// Whether the platform rounds an object's length up past what it was sized to
/// (macOS, to a page): a blob's length is then checked as at least its declared one.
pub(super) const ROUNDS_UP: bool = true;

/// What the platform keeps beside the mapping: nothing.
pub(super) type Backing = ();

/// Creates a shared memory object `size` bytes long, already unlinked.
pub(super) fn create(size: usize) -> Result<(File, Backing, Slab), SlabError> {
    create_named(size, &fresh_name()?)
}

/// A name no other slab of this process shares and a peer cannot guess.
fn fresh_name() -> Result<String, SlabError> {
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
    Ok(name)
}

/// [`create`] under `name`, which is unlinked before this returns.
fn create_named(size: usize, name: &str) -> Result<(File, Backing, Slab), SlabError> {
    let fd = shm_open(
        name,
        OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(file_error("create"))?;
    // The object is sized before it is unlinked, and the name is dropped
    // whether or not the size took, so a failure leaves nothing named.
    let sized = ftruncate(&fd, size as u64);
    let unlinked = shm_unlink(name);
    sized.map_err(file_error("set the size of"))?;
    unlinked.map_err(file_error("unlink"))?;
    fcntl_setfd(&fd, FdFlags::CLOEXEC).map_err(file_error("close on exec"))?;
    Ok((duplicate(&fd)?, (), fd))
}

/// The peer's slab as a file to map.
pub(super) fn open(slab: &Slab) -> Result<(File, Backing), SlabError> {
    Ok((duplicate(slab)?, ()))
}

/// Writes `bytes` at the start of the freshly made `file`, through a mapping:
/// a shared memory object refuses `write` here. The mapping is sound because the
/// object is sized once and XNU refuses a second `ftruncate`, so no holder can
/// shrink it under the mapping. Nothing is mapped for no bytes (a zero-length
/// `mmap` is `EINVAL`).
pub(super) fn fill(file: &File, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let map = MmapOptions::new().len(bytes.len()).map_raw(file)?;
    // SAFETY: the mapping is `bytes.len()` long and `map` is the only view this
    // process holds of it; `bytes` is our own buffer, so the ranges do not
    // overlap. No peer holds the object yet, and none can shrink it (above).
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), map.as_mut_ptr(), bytes.len()) };
    Ok(())
}

/// Appends the first `len` bytes of `file` to `out`, through a read-only mapping
/// (`read` is refused as `write` is). The caller has checked the object is at
/// least `len` long, and a holder cannot shrink it (see [`fill`]). The bytes are
/// copied once with no reference formed over them, as a peer holding the object
/// may write it.
pub(super) fn read_front(file: &File, len: usize, out: &mut Vec<u8>) -> std::io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let map = MmapOptions::new().len(len).map_raw_read_only(file)?;
    let at = out.len();
    out.resize(at + len, 0);
    // SAFETY: the mapping is `len` long and `out` has `len` bytes of its own
    // from `at`; they are distinct allocations, so the ranges do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(map.as_ptr(), out.as_mut_ptr().add(at), len) };
    Ok(())
}

/// Checks that `file` is a shared memory object, not a regular file, pipe,
/// socket, directory or device: this platform has no seals to look for, so the
/// descriptor's type is all it can say. XNU reports a POSIX shared memory object
/// with no file type bits, and only that is accepted: a regular file could be
/// shrunk by its holder under the mapping (XNU refuses a second `ftruncate` of
/// shared memory only); the length is the caller's check.
pub(super) fn verify(file: &File, _: usize) -> Result<(), SlabError> {
    use rustix::fs::{FileType, fstat};
    let stat = fstat(file).map_err(file_error("read the type of"))?;
    match FileType::from_raw_mode(stat.st_mode) {
        FileType::Unknown => Ok(()),
        _ => Err(SlabError::NotSharedMemory),
    }
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
    /// **A regular file is refused**: its holder could `ftruncate` it under the
    /// mapping, which a shared memory object does not allow.
    #[test]
    fn a_regular_file_is_refused() {
        let path = std::env::temp_dir().join(format!("tutti-ffi-regular-{}", std::process::id()));
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(verify(&file, 0), Err(SlabError::NotSharedMemory)));
    }

    /// **A descriptor that is not a memory object is refused by name**: a socket
    /// end sent where a blob was announced does not read.
    #[test]
    fn a_descriptor_that_is_not_shared_memory_is_refused() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let file = File::from(OwnedFd::from(a));
        assert!(matches!(verify(&file, 0), Err(SlabError::NotSharedMemory)));
    }

    use super::super::SlabHandle;
    use super::*;
    use crate::MappingWriter;
    use crate::header::SLAB_HEADER_BYTES;
    use rustix::fs::fstat;
    use rustix::io::Errno;

    /// Deliberately not a page multiple.
    const SIZE: usize = SLAB_HEADER_BYTES + 64;

    /// **The slab has no name**: `shm_unlink` has run by the time `create`
    /// returns, so opening the name again fails with `ENOENT`.
    #[test]
    fn the_name_is_gone_once_the_slab_is_made() {
        let name = fresh_name().unwrap();
        let (_file, _backing, _slab) = create_named(SIZE, &name).unwrap();
        let again = shm_open(&name, OFlags::RDWR, Mode::empty());
        assert_eq!(again.expect_err("the name opened"), Errno::NOENT);
    }

    /// The object is at least the size it was made with (the platform may round
    /// it up to a page).
    #[test]
    fn the_object_is_at_least_the_size_it_was_made_with() {
        let (_w, handle) = MappingWriter::create(SIZE).unwrap();
        assert!(fstat(&handle.0).unwrap().st_size >= SIZE as i64);
    }

    /// **A peer cannot resize the memory it holds**: once the object is sized, a
    /// second `ftruncate` to a smaller, a larger or the same size is refused, on
    /// the creator's descriptor, a duplicate and one received over a socket.
    #[test]
    fn a_second_ftruncate_is_refused_to_every_holder() {
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;
        let (host, handle) = MappingWriter::create(SIZE).unwrap();
        let (a, b) = UnixStream::pair().unwrap();
        handle.send(a.as_fd()).unwrap();
        let received = SlabHandle::receive(b.as_fd(), handle.wire()).unwrap();
        let duplicate = handle.0.try_clone().unwrap();
        for (holder, fd) in [
            ("creator", &handle.0),
            ("duplicate", &duplicate),
            ("receiver", &received.0),
        ] {
            for length in [0, SIZE as u64, SIZE as u64 + 4096] {
                assert!(
                    ftruncate(fd, length).is_err(),
                    "the {holder} resized the object to {length}"
                );
            }
        }
        // The host's mapping still reads what it holds.
        let mut back = [0u8; 8];
        host.mapping().read(SLAB_HEADER_BYTES, &mut back);
    }
}
