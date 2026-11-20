//! The slab's backing file on Windows: a file in the temp directory that ends
//! with its last mapping.
//!
//! A file with a mapped view cannot be removed by name, and the peer may still
//! map it when its creator is done. So the creator opens it with
//! `FILE_FLAG_DELETE_ON_CLOSE` and keeps that handle for as long as its mapping
//! lives; the file is deleted when the last handle and the last view of it close,
//! in either process, including after a crash (observed NTFS behaviour, not a
//! documented guarantee). The file sits in the temp directory, so its access is
//! that directory's ACL. The peer opens it by name with the
//! default share mode (read, write and delete), which the creator's open admits,
//! and does not ask for deletion.

use crate::SlabError;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::marker::PhantomData;
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OPEN_REPARSE_POINT,
};

/// The start of every slab name this module makes.
const NAME_PREFIX: &str = "tutti_plugin-";

/// What the slab's owner holds, and what a peer receives: the backing file's
/// name in the temp directory.
#[derive(Debug, Clone)]
pub(super) struct Slab {
    name: String,
}

/// What `SetupSharedMemory` carries of a slab: its name, checked as it is
/// decoded (a name this module could not have made does not decode), so a peer
/// cannot point `open` at another path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "WireName")]
pub struct SlabWire(String);

/// A name as it travels, unchecked until it becomes a [`SlabWire`]. Its field is
/// `pub` because `check-range-types` flags a private field under a plain
/// `Deserialize`.
#[derive(Deserialize)]
struct WireName(pub String);

impl TryFrom<WireName> for SlabWire {
    type Error = String;

    fn try_from(WireName(name): WireName) -> Result<Self, String> {
        // Exactly what `fresh` writes: the prefix, a decimal pid, a decimal
        // count and sixteen lowercase hex digits, joined by `-`.
        let decimal = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let spelled = name.strip_prefix(NAME_PREFIX).is_some_and(|rest| {
            let mut parts = rest.split('-');
            matches!(
                (parts.next(), parts.next(), parts.next(), parts.next()),
                (Some(pid), Some(count), Some(hex), None)
                    if decimal(pid)
                        && decimal(count)
                        && hex.len() == 16
                        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            )
        });
        if spelled {
            Ok(Self(name))
        } else {
            Err(format!("{name:?} is not a slab name"))
        }
    }
}

/// What carries a slab to the peer: nothing, as the name is in the frame.
#[derive(Debug, Clone, Copy)]
pub struct Connection<'a>(PhantomData<&'a ()>);

impl Connection<'_> {
    /// The connection of a platform whose slab travels in the frame.
    #[must_use]
    pub fn detached() -> Self {
        Self(PhantomData)
    }
}

/// Whether the platform rounds an object's length up past what it was sized to:
/// a file is exactly its size.
pub(super) const ROUNDS_UP: bool = false;

/// What ends the name: the creator's handle to the file, kept for as long as
/// its mapping lives. The name stays openable while it is held. It exists from
/// the moment the creator's file does, so a failure after that closes it and
/// deletes the file.
#[derive(Debug)]
pub(super) struct Backing {
    _file: File,
}

/// A fresh name: the process id, a per-process count and sixty-four random
/// bits, so no two slabs this process makes meet and a peer cannot guess it.
fn fresh() -> Result<String, SlabError> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(SlabError::Name)?;
    Ok(format!(
        "{NAME_PREFIX}{}-{}-{:016x}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed),
        u64::from_ne_bytes(bytes),
    ))
}

/// Creates the slab file, `size` bytes long, exclusively: a name that already
/// exists is refused, never truncated under a process that may have it mapped.
pub(super) fn create(size: usize) -> Result<(File, Backing, Slab), SlabError> {
    let name = fresh()?;
    let mut opts = OpenOptions::new();
    opts.read(true)
        .write(true)
        .create_new(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
    let (file, backing) = open_with(&opts, &name)?;
    file.set_len(size as u64)
        .map_err(|source| SlabError::File {
            op: "set the size of",
            source,
        })?;
    Ok((file, backing, Slab { name }))
}

/// Opens the peer's slab file by name.
pub(super) fn open(slab: &Slab) -> Result<(File, Backing), SlabError> {
    let mut opts = OpenOptions::new();
    // The reparse point itself is opened, never its target.
    opts.read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    open_with(&opts, &slab.name)
}

fn open_with(opts: &OpenOptions, name: &str) -> Result<(File, Backing), SlabError> {
    let file = opts
        .open(path(name))
        .map_err(|source| SlabError::File { op: "open", source })?;
    let backing = Backing {
        _file: file.try_clone().map_err(|source| SlabError::File {
            op: "duplicate the handle of",
            source,
        })?,
    };
    Ok((file, backing))
}

/// Writes `bytes` at the start of the freshly made `file`.
pub(super) fn fill(mut file: &File, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    file.write_all(bytes)
}

/// Appends the first `len` bytes of `file` to `out`, from the start whatever
/// offset the descriptor was left at.
pub(super) fn read_front(mut file: &File, len: usize, out: &mut Vec<u8>) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    file.take(len as u64).read_to_end(out).map(drop)
}

/// Nothing to check beyond the length and the header the caller checks.
pub(super) fn verify(_: &File, _: usize) -> Result<(), SlabError> {
    Ok(())
}

pub(super) fn wire(slab: &Slab) -> SlabWire {
    SlabWire(slab.name.clone())
}

/// The name travels in the frame: there is nothing to send beside it.
pub(super) fn send(_: Connection<'_>, _: &Slab) -> std::io::Result<()> {
    Ok(())
}

/// The slab a frame named.
pub(super) fn receive(_: Connection<'_>, wire: SlabWire) -> Result<Slab, SlabError> {
    Ok(Slab { name: wire.0 })
}

fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tutti_{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name decoded from a peer's bytes is one `create` could have made: one
    /// that climbs out of the temp directory, or is empty, does not decode.
    #[test]
    fn a_decoded_name_must_be_one_create_could_have_made() {
        let decode = |name: &str| SlabWire::try_from(WireName(name.to_owned()));
        assert!(decode(&fresh().unwrap()).is_ok(), "a made name is refused");
        for bad in [
            "",
            "tutti_plugin-",
            "../etc/passwd",
            "tutti_plugin-/../x",
            "tutti_plugin-a/b",
            "other-1-2-3",
            "tutti_plugin-1-2-0123456789abcde",
            "tutti_plugin-1-2-0123456789abcdef0",
            "tutti_plugin-1-2-0123456789ABCDEF",
            "tutti_plugin-1-2-0123456789abcdef-3",
            "tutti_plugin-1-0123456789abcdef",
            "tutti_plugin-x-2-0123456789abcdef",
        ] {
            assert!(decode(bad).is_err(), "{bad:?} decoded");
        }
    }

    /// The same name created twice is refused the second time: an existing
    /// file is never truncated or reused.
    #[test]
    fn creating_an_existing_name_is_refused() {
        let (_file, _backing, slab) = create(4096).unwrap();
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        assert!(open_with(&opts, &slab.name).is_err());
    }

    /// Opening a file that is not a slab fails on the magic, not on a mapping
    /// full of someone else's bytes read as audio.
    #[test]
    fn opening_a_foreign_file_is_refused() {
        use super::super::SlabHandle;
        use crate::MappingWriter;
        let size = crate::SLAB_HEADER_BYTES + 64;
        let name = format!("ffi_foreign_{}", std::process::id());
        std::fs::write(path(&name), vec![0xABu8; size]).unwrap();
        let handle = SlabHandle(Slab { name: name.clone() });
        let err = MappingWriter::open(&handle, size).expect_err("a foreign file must not open");
        std::fs::remove_file(path(&name)).unwrap();
        assert!(matches!(err, SlabError::BadMagic { .. }), "{err}");
    }

    /// **A blob is readable by its name while its lease is held, and gone once the
    /// lease is dropped**: nothing travels on this platform, the name in the
    /// frame is the whole of it, so the lease is what keeps the file for the
    /// reader, and the file does not outlive it.
    #[test]
    fn a_blob_is_readable_by_name_while_its_lease_is_held_and_gone_after() {
        use crate::Blob;
        let blob = Blob::create(&[5u8; 100]).unwrap();
        let wire = blob.wire();
        let lease = blob.send(Connection::detached()).unwrap();
        let got = Blob::receive(Connection::detached(), wire.clone())
            .unwrap()
            .read(100, 1000)
            .unwrap();
        assert_eq!(got, vec![5u8; 100]);
        let wrong = Blob::receive(Connection::detached(), wire.clone())
            .unwrap()
            .read(99, 1000)
            .expect_err("another length than the file's");
        assert!(matches!(wrong, SlabError::BlobSize { .. }), "{wrong}");
        drop(lease);
        let gone = Blob::receive(Connection::detached(), wire)
            .unwrap()
            .read(100, 1000)
            .expect_err("the file outlived its lease");
        assert!(matches!(gone, SlabError::File { .. }), "{gone}");
    }
}
