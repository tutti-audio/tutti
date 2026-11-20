//! A blob: bytes that cross the control connection as one memory object.
//!
//! The sender makes the object ([`Blob::create`]), announces it in a frame
//! that carries its [`BlobWire`], and sends it right after that frame
//! ([`Blob::send`]); the receiver takes it from the stream
//! ([`Blob::receive`]) and copies the bytes out once ([`Received::read`]),
//! after checking the object against what the frame declared. On unix the
//! object is the slab's own kind (a sealed `memfd`, an unlinked shared memory
//! object) and travels as a descriptor; on Windows it is the named temp file the
//! slab uses, named in the frame.
//!
//! The bytes are read with `read`, not mapped: a read of an object a peer holds
//! cannot fault if the peer truncates it, which a mapping of an unsealed one
//! (macOS) could.

use super::platform;
use crate::SlabError;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

/// What a frame carries of a blob: the same form as a slab's.
pub type BlobWire = super::SlabWire;

/// A memory object holding bytes, made by [`create`](Self::create) to be sent.
#[derive(Debug)]
pub struct Blob {
    slab: platform::Slab,
    backing: platform::Backing,
}

/// What keeps a sent blob readable where it is read by name (Windows), and
/// nothing elsewhere. Held until the peer has taken the blob.
#[derive(Debug)]
#[must_use = "dropping the lease may remove the blob before the peer has read it"]
pub struct Lease {
    _keep: platform::Backing,
}

/// A blob taken from the connection, not yet read: its object is checked
/// against the frame's declaration by [`read`](Self::read).
#[derive(Debug)]
pub struct Received {
    slab: platform::Slab,
}

impl Blob {
    /// Makes an object holding `bytes`, sealed where the platform has seals.
    ///
    /// # Errors
    ///
    /// [`SlabError::Name`] if the random source fails, and [`SlabError::File`]
    /// if the object cannot be created, sized, sealed or written.
    pub fn create(bytes: &[u8]) -> Result<Self, SlabError> {
        let (mut file, backing, slab) = platform::create(bytes.len())?;
        file.write_all(bytes).map_err(|source| SlabError::File {
            op: "write",
            source,
        })?;
        Ok(Self { slab, backing })
    }

    /// What goes in the frame that announces this blob.
    #[must_use]
    pub fn wire(&self) -> BlobWire {
        platform::wire(&self.slab)
    }

    /// Sends the blob over `conn`, immediately after the frame that carries its
    /// [`wire`](Self::wire) has been written. The sender's own descriptor closes
    /// with this call; the returned [`Lease`] is what remains.
    ///
    /// # Errors
    ///
    /// The OS error of the send, a send timeout included: the connection is then
    /// mid-message and cannot go on.
    pub fn send(self, conn: platform::Connection<'_>) -> std::io::Result<Lease> {
        platform::send(conn, &self.slab)?;
        Ok(Lease {
            _keep: self.backing,
        })
    }

    /// Takes the blob the frame carrying `wire` announced, over `conn`, right
    /// after that frame was read.
    ///
    /// # Errors
    ///
    /// [`SlabError::NoDescriptor`], [`SlabError::ExtraDescriptors`],
    /// [`SlabError::BadMark`] or [`SlabError::Transfer`] if what arrived is not
    /// exactly one descriptor.
    pub fn receive(conn: platform::Connection<'_>, wire: BlobWire) -> Result<Received, SlabError> {
        platform::receive(conn, wire).map(|slab| Received { slab })
    }
}

impl Received {
    /// Copies the object's bytes out and closes it, once the object is the
    /// `declared` length (exactly, where the platform keeps lengths exact, and
    /// at least where it rounds objects up) and no more than `max`, and where
    /// the platform seals, sealed as [`Blob::create`] seals.
    ///
    /// # Errors
    ///
    /// [`SlabError::BlobTooLarge`] for a declared length over `max`,
    /// [`SlabError::BlobSize`] for an object of another length,
    /// [`SlabError::NotSealed`] for an object that is not sealed,
    /// [`SlabError::NoRoom`] if the copy cannot be allocated and
    /// [`SlabError::File`] if the object cannot be opened or read.
    pub fn read(self, declared: u64, max: usize) -> Result<Vec<u8>, SlabError> {
        let too_large = || SlabError::BlobTooLarge {
            declared,
            limit: max,
        };
        let len = usize::try_from(declared).map_err(|_| too_large())?;
        if len > max {
            return Err(too_large());
        }
        let (mut file, _backing) = platform::open(&self.slab)?;
        let file_error = |op| move |source| SlabError::File { op, source };
        let actual = file
            .metadata()
            .map_err(file_error("read the size of"))?
            .len();
        let fits = if platform::ROUNDS_UP {
            actual >= declared
        } else {
            actual == declared
        };
        if !fits {
            return Err(SlabError::BlobSize { declared, actual });
        }
        platform::verify(&file, len)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| SlabError::NoRoom { bytes: len })?;
        read_front(&mut file, len, &mut bytes).map_err(file_error("read"))?;
        if bytes.len() != len {
            return Err(SlabError::BlobSize {
                declared,
                actual: bytes.len() as u64,
            });
        }
        Ok(bytes)
    }
}

/// Reads the first `len` bytes of `file` into `out`: from the start, whatever
/// offset the object's descriptor was left at.
fn read_front(file: &mut File, len: usize, out: &mut Vec<u8>) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.take(len as u64).read_to_end(out).map(drop)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::SlabWire;
    use crate::descriptor;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    const CAP: usize = 64 << 20;

    /// More than a frame carries, so a round trip proves the object
    /// carries what a frame could not.
    fn big() -> Vec<u8> {
        (0..9 * 1024 * 1024 + 123)
            .map(|i| (i % 251) as u8)
            .collect()
    }

    /// Sends `bytes` from `from` to `to` as a blob and returns what `to` read.
    fn across(from: &UnixStream, to: &UnixStream, bytes: &[u8]) -> Result<Vec<u8>, SlabError> {
        let blob = Blob::create(bytes).unwrap();
        let wire = blob.wire();
        let _lease = blob.send(from.as_fd()).unwrap();
        Blob::receive(to.as_fd(), wire)?.read(bytes.len() as u64, CAP)
    }

    /// **A blob arrives byte for byte in both directions**, through the real
    /// descriptor, at a size past any chunk a frame carried, and an empty one
    /// arrives empty.
    #[test]
    fn a_blob_round_trips_in_both_directions() {
        let (a, b) = UnixStream::pair().unwrap();
        let bytes = big();
        assert_eq!(across(&a, &b, &bytes).unwrap(), bytes);
        assert_eq!(across(&b, &a, &bytes).unwrap(), bytes);
        assert!(across(&a, &b, &[]).unwrap().is_empty());
    }

    /// **A declared length that is not the object's is refused by name**, whether
    /// it claims more or less than the object holds.
    #[test]
    fn a_declared_length_that_is_not_the_objects_is_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        for declared in [99u64, 101] {
            let blob = Blob::create(&[7u8; 100]).unwrap();
            let wire = blob.wire();
            let _lease = blob.send(a.as_fd()).unwrap();
            let err = Blob::receive(b.as_fd(), wire)
                .unwrap()
                .read(declared, CAP)
                .expect_err("a mismatched length");
            // macOS rounds an object up, so only a claim past it is a mismatch there.
            if platform::ROUNDS_UP && declared < 100 {
                continue;
            }
            assert!(
                matches!(err, SlabError::BlobSize { declared: d, .. } if d == declared),
                "{err}"
            );
        }
    }

    /// **A length past the limit is refused before the object is opened**, even
    /// when the object is that long.
    #[test]
    fn a_length_past_the_limit_is_refused() {
        let (a, b) = UnixStream::pair().unwrap();
        let blob = Blob::create(&[1u8; 4096]).unwrap();
        let wire = blob.wire();
        let _lease = blob.send(a.as_fd()).unwrap();
        let err = Blob::receive(b.as_fd(), wire)
            .unwrap()
            .read(4096, 4095)
            .expect_err("over the limit");
        assert!(
            matches!(
                err,
                SlabError::BlobTooLarge {
                    declared: 4096,
                    limit: 4095
                }
            ),
            "{err}"
        );
    }

    /// **An object that is not sealed as `create` seals it is refused** (Linux):
    /// a peer holding an unsealed memory file could shrink it under the read.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unsealed_memfd_is_refused() {
        use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
        let (a, b) = UnixStream::pair().unwrap();
        let fd = memfd_create("t", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap();
        ftruncate(&fd, 16).unwrap();
        descriptor::send(a.as_fd(), fd.as_fd()).unwrap();
        let err = Blob::receive(b.as_fd(), SlabWire)
            .unwrap()
            .read(16, CAP)
            .expect_err("unsealed");
        assert!(matches!(err, SlabError::NotSealed), "{err}");
    }

    /// **A blob frame whose object never follows is refused by name**: the mark
    /// byte arrives with no descriptor.
    #[test]
    fn a_missing_descriptor_is_refused() {
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[0xFD]).unwrap();
        let err = Blob::receive(b.as_fd(), SlabWire).expect_err("no descriptor");
        assert!(matches!(err, SlabError::NoDescriptor), "{err}");
    }

    /// **Two descriptors where one blob was announced are refused**, and the
    /// extra one closes.
    #[test]
    fn extra_descriptors_are_refused() {
        use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
        use std::io::IoSlice;
        use std::mem::MaybeUninit;
        let (a, b) = UnixStream::pair().unwrap();
        let (x, _x) = UnixStream::pair().unwrap();
        let (y, _y) = UnixStream::pair().unwrap();
        let fds = [x.as_fd(), y.as_fd()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
        sendmsg(
            &a,
            &[IoSlice::new(&[0xFD])],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        let err = Blob::receive(b.as_fd(), SlabWire).expect_err("two descriptors");
        assert!(matches!(err, SlabError::ExtraDescriptors), "{err}");
    }

    /// **No descriptor leaks across blobs**: making, sending, receiving and
    /// reading blobs many times leaves the process with the descriptors it
    /// began with.
    #[test]
    fn many_blobs_leak_no_descriptors() {
        let (a, b) = UnixStream::pair().unwrap();
        let open_fds = || std::fs::read_dir("/dev/fd").unwrap().count();
        let before = open_fds();
        for i in 0..64u8 {
            assert_eq!(across(&a, &b, &[i; 1000]).unwrap(), vec![i; 1000]);
        }
        assert_eq!(open_fds(), before);
    }
}
