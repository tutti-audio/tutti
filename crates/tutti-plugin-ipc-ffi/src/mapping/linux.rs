//! The slab on Linux: a sealed anonymous memory file.
//!
//! A `memfd` has no name in any directory, so nothing can open it by name and
//! nothing is left behind when its holders go. It is sized once and then sealed
//! against shrinking, growing and further sealing, so the process that receives
//! it (the one that loads untrusted plugin code) cannot `ftruncate` the memory
//! out from under the host's mapping, which would fault the host's audio thread.

use crate::SlabError;
use crate::descriptor::{self, duplicate, file_error};
use rustix::fs::{
    MemfdFlags, SealFlags, fcntl_add_seals, fcntl_get_seals, ftruncate, memfd_create,
};
use rustix::net::RecvFlags;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::time::Duration;

/// The seals every slab carries from the moment it is shared.
const SEALS: SealFlags = SealFlags::SHRINK
    .union(SealFlags::GROW)
    .union(SealFlags::SEAL);

/// How long [`receive`] waits for the descriptor's byte once the frame that
/// announces it has been read: the sender writes both back to back, so this
/// only ends a peer that announced a descriptor and sent none. It guards a
/// hang and measures nothing.
const DESCRIPTOR_PATIENCE: Duration = Duration::from_secs(2);

/// What the slab's owner holds, and what a peer receives: the memory file.
pub(super) type Slab = OwnedFd;

pub use crate::descriptor::{Connection, SlabWire};

/// Whether the platform rounds an object's length up past what it was sized to
/// (macOS, to a page): a blob's length is then checked as at least its declared one.
pub(super) const ROUNDS_UP: bool = false;

/// What the platform keeps beside the mapping: nothing.
pub(super) type Backing = ();

/// Creates a sealed memory file `size` bytes long.
pub(super) fn create(size: usize) -> Result<(File, Backing, Slab), SlabError> {
    let fd = memfd_create(
        "tutti-slab",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .map_err(file_error("create"))?;
    ftruncate(&fd, size as u64).map_err(file_error("set the size of"))?;
    fcntl_add_seals(&fd, SEALS).map_err(file_error("seal"))?;
    Ok((duplicate(&fd)?, (), fd))
}

/// The peer's slab as a file to map.
pub(super) fn open(slab: &Slab) -> Result<(File, Backing), SlabError> {
    Ok((duplicate(slab)?, ()))
}

/// Checks that `file` is a memory file sealed as [`create`] seals it, and
/// exactly `size` long: a descriptor of any other file could be resized by whoever
/// holds it, and a larger one is a sender that disagrees about the layout.
pub(super) fn verify(file: &File, size: usize) -> Result<(), SlabError> {
    // Only a memory file has seals: any other descriptor fails this call.
    let seals = fcntl_get_seals(file).map_err(|_| SlabError::NotSealed)?;
    if !seals.contains(SEALS) {
        return Err(SlabError::NotSealed);
    }
    let actual = file
        .metadata()
        .map_err(|source| SlabError::File {
            op: "read the size of",
            source,
        })?
        .len();
    if actual > size as u64 {
        return Err(SlabError::Long {
            actual,
            expected: size,
        });
    }
    Ok(())
}

pub(super) fn wire(_: &Slab) -> SlabWire {
    SlabWire
}

/// Sends `slab` over `conn`, after the frame that announces it.
pub(super) fn send(conn: Connection<'_>, slab: &Slab) -> std::io::Result<()> {
    use std::os::fd::AsFd;
    descriptor::send(conn, slab.as_fd())
}

/// Receives the slab a frame announced, over `conn`.
pub(super) fn receive(conn: Connection<'_>, _: SlabWire) -> Result<Slab, SlabError> {
    descriptor::recv(conn, RecvFlags::CMSG_CLOEXEC, DESCRIPTOR_PATIENCE)
}

#[cfg(test)]
mod tests {
    use super::super::SlabHandle;
    use super::*;
    use crate::header::{SLAB_HEADER_BYTES, SLAB_MAGIC};
    use crate::{MappingWriter, SlabError};
    use rustix::fs::fstat;

    const SIZE: usize = SLAB_HEADER_BYTES + 64;

    /// The magic is the first thing in the mapping: the opening side keys on it
    /// before trusting a single byte, so `repr(C)` and its field order are
    /// load-bearing. Read through the memory's own descriptor, not through the
    /// header type.
    #[test]
    fn the_magic_sits_at_offset_zero() {
        let (_w, handle) = MappingWriter::create(SIZE).unwrap();
        let mut bytes = [0u8; 8];
        rustix::io::pread(&handle.0, &mut bytes, 0).unwrap();
        assert_eq!(u64::from_le_bytes(bytes), SLAB_MAGIC);
    }

    /// **The slab has no name**: no directory entry points at it (`st_nlink` is
    /// 0), so nothing can open it but a holder of the descriptor, and nothing is
    /// left behind when the holders go.
    #[test]
    fn the_slab_has_no_filesystem_name() {
        let (_w, handle) = MappingWriter::create(SIZE).unwrap();
        assert_eq!(fstat(&handle.0).unwrap().st_nlink, 0);
    }

    /// **A peer cannot resize the memory it receives**: the sealed memory refuses
    /// `ftruncate` to a smaller or a larger size and refuses another seal, while
    /// the host's mapping goes on reading what it wrote.
    #[test]
    fn the_peer_cannot_shrink_grow_or_reseal_the_memory() {
        let (mut host, handle) = MappingWriter::create(SIZE).unwrap();
        host.write(SLAB_HEADER_BYTES, &[5u8; 8]);
        let peer = handle.0.try_clone().unwrap();
        assert!(ftruncate(&peer, 0).is_err(), "the memory shrank");
        assert!(
            ftruncate(&peer, SIZE as u64 + 1).is_err(),
            "the memory grew"
        );
        assert!(
            fcntl_add_seals(&peer, SealFlags::WRITE).is_err(),
            "a sealed memory took another seal"
        );
        let mut back = [0u8; 8];
        host.mapping().read(SLAB_HEADER_BYTES, &mut back);
        assert_eq!(back, [5u8; 8]);
        assert_eq!(fstat(&peer).unwrap().st_size, SIZE as i64);
    }

    /// **A descriptor of any other file is refused by name**: an ordinary file
    /// could be resized by whoever holds it, so it never maps.
    #[test]
    fn an_ordinary_file_is_refused_as_not_sealed() {
        let file = File::open("/proc/self/exe").unwrap();
        let err = MappingWriter::open(&SlabHandle(file.into()), SIZE).expect_err("opened");
        assert!(matches!(err, SlabError::NotSealed), "{err}");
    }

    /// An unsealed memory file is refused too: a memfd is not enough, it must be
    /// sealed as `create` seals it.
    #[test]
    fn an_unsealed_memfd_is_refused() {
        let fd = memfd_create("t", MemfdFlags::ALLOW_SEALING).unwrap();
        ftruncate(&fd, SIZE as u64).unwrap();
        let err = MappingWriter::open(&SlabHandle(fd), SIZE).expect_err("unsealed");
        assert!(matches!(err, SlabError::NotSealed), "{err}");
    }

    /// A sealed memory longer than the layout is a sender that disagrees about
    /// the layout, not a slab with room to spare.
    #[test]
    fn a_sealed_memory_longer_than_the_layout_is_refused() {
        let (_w, handle) = MappingWriter::create(SIZE).unwrap();
        let err = MappingWriter::open(&handle, SIZE - 1).expect_err("a long slab");
        assert!(matches!(err, SlabError::Long { .. }), "{err}");
    }
}
