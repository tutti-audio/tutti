//! The slab's shared memory: the file, the mapping, and the only copies in and
//! out of it.
//!
//! The mapping is a [`memmap2::MmapRaw`] and is reached only through
//! [`SharedMapping::header`] (a shared reference to the atomics at offset 0),
//! [`SharedMapping::read`] and [`MappingWriter::write`]. No reference to the
//! data region is ever formed: another process writes it while we hold the
//! mapping, so a `&[u8]` or `&mut [u8]` over it would assert an exclusivity or
//! immutability that no Rust type can see, let alone keep.
//!
//! A mapping exists only together with its one [`MappingWriter`] (not `Clone`,
//! `write` takes `&mut self`), so within this process every store into the data
//! region is ordered by the borrow checker. That, and the file being made here
//! (exclusively, at 0600 on unix and in the temp directory's ACL on Windows) and
//! never resized, is what lets every call in this module be safe.
//!
//! What makes `&SharedMapping` shareable between threads is the data region's
//! discipline, since `header` hands out `&SlabHeader` (all atomics) and `read`
//! copies through raw pointers and never forms a reference to the region, so no
//! Rust aliasing rule is in play for those bytes. What remains is the data-race
//! question:
//!
//! - *Within this process*, the only stores into the data region are
//!   `MappingWriter::write`, and a mapping has exactly one writer (made with it
//!   in `create` or `open`, not `Clone`, `write` takes `&mut self`).
//! - *Across processes*, the other side writes the other direction's region
//!   (disjoint offsets, decided by `tutti-plugin-ipc`'s layout), and a reader of
//!   a region the peer writes copies only between two `Acquire` loads of the
//!   slot's sequence that both name the block it wants. A copy that overlapped
//!   the peer's rewrite of the slot is discarded, never used.
//!
//! What is NOT claimed: that a misbehaving peer cannot scribble on the region,
//! or that the copy of a slot the peer is rewriting is well-defined in the
//! abstract machine (it is the seqlock pattern, which Rust's model does not yet
//! express). The bytes copied are plain bytes into our own buffer, so a corrupt
//! or racing peer yields wrong audio, which the recheck then discards, never an
//! invalid value on this side.
//!
//! A slab is made with [`MappingWriter::create`], which returns the
//! [`SlabHandle`] the peer opens it with; no caller names, spells or builds
//! one.
//!
//! The backing file's platform is one module, chosen once above: where it lives,
//! how it is opened, and what keeps it from outliving its last mapping.

#[cfg(unix)]
#[path = "mapping/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "mapping/windows.rs"]
mod platform;

use crate::SlabError;
use crate::header::{SLAB_HEADER_BYTES, SlabHeader};
use memmap2::MmapRaw;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A mapped slab: a [`SlabHeader`] at offset 0, then the data region.
///
/// Shared (in an `Arc`) by everything that reads the slab; the one writer is
/// the [`MappingWriter`] it was made with.
#[derive(Debug)]
pub struct SharedMapping {
    /// The mapping, page-aligned and at least [`SLAB_HEADER_BYTES`] long;
    /// unmapped on drop.
    map: MmapRaw,
    /// What the platform keeps open beside the mapping.
    _backing: platform::Backing,
}

/// The one writer of a [`SharedMapping`]'s data region in this process.
///
/// Made only with its mapping ([`create`](Self::create), [`open`](Self::open)),
/// and not `Clone`:
///
/// ```compile_fail
/// fn second(w: &tutti_plugin_ipc_ffi::MappingWriter) -> tutti_plugin_ipc_ffi::MappingWriter {
///     w.clone()
/// }
/// ```
#[derive(Debug)]
pub struct MappingWriter {
    mapping: Arc<SharedMapping>,
}

/// The start of every slab name this module makes.
const NAME_PREFIX: &str = "tutti_plugin-";

/// What a peer needs to open a slab made by [`MappingWriter::create`]: handed
/// out by `create`, sent to the peer, and taken by [`MappingWriter::open`].
///
/// Opaque: its contents are private to this module, so nothing outside can
/// read or construct one. A handle decoded from a peer's bytes is checked as it
/// is decoded (a name this module could not have made does not decode), so a
/// peer cannot point `open` at another path.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "WireName")]
pub struct SlabHandle {
    /// The backing file's name in the platform's shared directory.
    name: String,
}

/// A handle as it travels: the name alone, unchecked until it becomes one.
/// The type is private to this module; its field is `pub` because
/// `check-range-types` flags a private field under a plain `Deserialize`.
#[derive(Deserialize)]
struct WireName(pub String);

impl TryFrom<WireName> for SlabHandle {
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
            Ok(Self { name })
        } else {
            Err(format!("{name:?} is not a slab name"))
        }
    }
}

impl Serialize for SlabHandle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.name)
    }
}

impl SlabHandle {
    /// A fresh name: the process id, a per-process count and sixty-four random
    /// bits, so no two slabs this process makes meet and a peer cannot guess it.
    fn fresh() -> Result<Self, SlabError> {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(SlabError::Name)?;
        Ok(Self {
            name: format!(
                "{NAME_PREFIX}{}-{}-{:016x}",
                std::process::id(),
                COUNT.fetch_add(1, Ordering::Relaxed),
                u64::from_ne_bytes(bytes),
            ),
        })
    }
}

/// Whether [`platform::open_file`] creates the backing file or opens the peer's.
enum Open {
    Create,
    Existing,
}

impl MappingWriter {
    /// Creates a slab `size` bytes long, maps it and stamps its header, before
    /// any peer holds its [`SlabHandle`], which is returned beside the writer.
    ///
    /// The file is created exclusively, at 0600 on unix (the temp directory's
    /// ACL on Windows): a name that already exists (a symlink included) is an
    /// error, never a file to truncate, since another process could hold that
    /// file mapped, or have planted it. The file ends
    /// with the creator's mapping: on unix the creator's drop unlinks it, and on
    /// Windows it is opened delete-on-close and the creator keeps that handle,
    /// so it is deleted when the last handle and view of it close.
    ///
    /// # Errors
    ///
    /// [`SlabError::Name`] if the random source fails, and
    /// [`SlabError::File`] if the file exists or cannot be created, sized or
    /// mapped.
    ///
    /// # Panics
    ///
    /// If `size` is smaller than [`SLAB_HEADER_BYTES`]: the caller sizes the
    /// slab from a layout that always includes the header.
    pub fn create(size: usize) -> Result<(Self, SlabHandle), SlabError> {
        let handle = SlabHandle::fresh()?;
        let (file, backing) = platform::open_file(&handle.name, size, Open::Create)?;
        let writer = Self::map(file, backing)?;
        // Stamp before anyone can open it: the peer gets the handle only
        // after this returns, and `initialize` releases the magic last so an
        // opener that sees the magic sees a fully written header.
        writer.mapping.header().initialize();
        Ok((writer, handle))
    }

    /// Opens the peer's slab that `handle` names, checking that it is at least `size`
    /// bytes and a tutti slab of this build's header.
    ///
    /// On unix a symlink at the name is refused (`O_NOFOLLOW`).
    ///
    /// # Errors
    ///
    /// [`SlabError::File`] if it cannot be opened or mapped,
    /// [`SlabError::Short`] if it is shorter than `size`, and
    /// [`SlabError::BadMagic`], [`SlabError::BadVersion`] or
    /// [`SlabError::BadSlots`] if it is not a tutti slab of this header version
    /// and ring depth.
    ///
    /// # Panics
    ///
    /// As for [`create`](Self::create).
    pub fn open(handle: &SlabHandle, size: usize) -> Result<Self, SlabError> {
        let (file, backing) = platform::open_file(&handle.name, size, Open::Existing)?;
        let actual = file
            .metadata()
            .map_err(|source| SlabError::File {
                op: "read the size of",
                source,
            })?
            .len();
        // Length first: `header()` asserts on a short mapping, and an assert is
        // the wrong failure mode for "the peer created a slab of another shape".
        if actual < size as u64 {
            return Err(SlabError::Short {
                actual,
                expected: size,
            });
        }
        let writer = Self::map(file, backing)?;
        writer.mapping.header().validate()?;
        Ok(writer)
    }

    fn map(file: File, backing: platform::Backing) -> Result<Self, SlabError> {
        // `open_file` is the only code that makes or opens a slab file: it
        // creates one exclusively and sizes it before this map, and opens the
        // peer's without `truncate` and never sizes it. A process with write
        // access to the file could still shrink it; that is a fault of a
        // trusted peer, as is scribbling on it (the module's argument).
        let map =
            MmapRaw::map_raw(&file).map_err(|source| SlabError::File { op: "map", source })?;
        Ok(Self {
            mapping: Arc::new(SharedMapping::new(map, backing)),
        })
    }

    /// The mapping, for the readers that do not write.
    #[must_use]
    pub fn mapping(&self) -> &Arc<SharedMapping> {
        &self.mapping
    }

    /// Copies `src` into the mapping at byte `offset`.
    ///
    /// # Panics
    ///
    /// If the range reaches into the header or past the mapping. The caller's
    /// offsets come from its layout, so this is the last line of defence, not
    /// the check callers rely on.
    #[inline]
    pub fn write(&mut self, offset: usize, src: &[u8]) {
        let m = &self.mapping;
        let end = m.check_range(offset, src.len());
        debug_assert!(end <= m.map.len());
        // SAFETY: `check_range` proves `offset..end` lies in the mapping and
        // after the header. `src` is our own buffer, never the mapping, so the
        // two ranges cannot overlap. No reference to these bytes exists (the
        // module's rule), and `&mut self` on the mapping's one writer orders
        // every store this process makes (the module's argument).
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), m.map.as_mut_ptr().add(offset), src.len());
        }
    }
}

impl SharedMapping {
    /// Takes ownership of `map` and of what the platform keeps beside it.
    ///
    /// # Panics
    ///
    /// If the mapping is smaller than the header.
    fn new(map: MmapRaw, backing: platform::Backing) -> Self {
        assert!(
            map.len() >= SLAB_HEADER_BYTES,
            "slab mapping is smaller than its own header"
        );
        Self {
            map,
            _backing: backing,
        }
    }

    /// The header at offset 0 of the mapping.
    ///
    /// A shared reference even though callers mutate through it: every field is
    /// an atomic. `write` refuses an offset inside the header, so no copy
    /// ever stores through these bytes while this reference lives.
    #[inline]
    #[must_use]
    pub fn header(&self) -> &SlabHeader {
        #[allow(
            clippy::cast_ptr_alignment,
            reason = "page-aligned mmap base; the alignment argument is below"
        )]
        let header = self.map.as_ptr().cast::<SlabHeader>();
        // SAFETY:
        // - *Alignment*: the header sits at offset 0 of a page-aligned mmap base,
        //   which exceeds `SlabHeader`'s alignment (`CachePadded`, so <= 128).
        // - *Size*: `new` asserts the mapping holds at least the header.
        // - *Validity*: `repr(C)` holding only atomic integers, with no invalid
        //   bit patterns and no niches, so any bytes of the right length are a
        //   valid instance, including a fresh mapping's zeros. `validate`
        //   decides whether the contents are *meaningful*; this cast only needs
        //   them well-formed.
        // - *Aliasing*: the header's bytes are only ever stored through its
        //   atomics (here or in the peer); `write` refuses them.
        // - *Lifetime*: tied to `&self`, and the mapping lives as long as `self`.
        unsafe { &*header }
    }

    /// Copies `dst.len()` bytes out of the mapping from byte `offset`.
    ///
    /// The bytes may be mid-rewrite by the peer: a caller decides whether they
    /// are a published block by the slot's sequence, before and after.
    ///
    /// # Panics
    ///
    /// As for [`MappingWriter::write`].
    #[inline]
    pub fn read(&self, offset: usize, dst: &mut [u8]) {
        let end = self.check_range(offset, dst.len());
        debug_assert!(end <= self.map.len());
        // SAFETY: `check_range` proves `offset..end` lies in the mapping and
        // after the header. `dst` is our own buffer, never the mapping, so the
        // ranges cannot overlap. Any byte value is valid for `u8`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.map.as_ptr().add(offset),
                dst.as_mut_ptr(),
                dst.len(),
            );
        }
    }

    /// `offset + len`, after checking the range is data, not header, and
    /// inside the mapping.
    fn check_range(&self, offset: usize, len: usize) -> usize {
        let end = offset.checked_add(len).expect("slab range overflows usize");
        assert!(
            offset >= SLAB_HEADER_BYTES && end <= self.map.len(),
            "slab range {offset}..{end} is outside the data region {SLAB_HEADER_BYTES}..{}",
            self.map.len()
        );
        end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::SLAB_MAGIC;

    /// A handle naming `name`, for a file the test made itself.
    fn handle_of(name: &str) -> SlabHandle {
        SlabHandle {
            name: name.to_owned(),
        }
    }

    /// The mapping's copies refuse the header's bytes: the header is only ever
    /// stored through its atomics, while `header()` hands out a shared
    /// reference to them.
    #[test]
    #[should_panic(expected = "outside the data region")]
    fn a_copy_into_the_header_panics() {
        let (mut w, _handle) = MappingWriter::create(SLAB_HEADER_BYTES + 64).unwrap();
        w.write(0, &[0u8; 8]);
    }

    /// The magic is the first thing in the mapping: the opening side keys on it
    /// before trusting a single byte, so `repr(C)` and its field order are
    /// load-bearing. Read from the file, not through the header type.
    #[test]
    fn the_magic_sits_at_offset_zero() {
        let (_w, handle) = MappingWriter::create(SLAB_HEADER_BYTES + 64).unwrap();
        let bytes = std::fs::read(platform::path(&handle.name)).unwrap();
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            SLAB_MAGIC
        );
    }

    /// An opener reads back what the creator wrote, and refuses a file shorter
    /// than the size it asked for.
    #[test]
    fn open_sees_the_creators_bytes_and_refuses_a_short_file() {
        let size = SLAB_HEADER_BYTES + 16;
        let (mut w, handle) = MappingWriter::create(size).unwrap();
        w.write(SLAB_HEADER_BYTES, &[7u8; 16]);
        let peer = MappingWriter::open(&handle, size).unwrap();
        let mut back = [0u8; 16];
        peer.mapping().read(SLAB_HEADER_BYTES, &mut back);
        assert_eq!(back, [7u8; 16]);
        let err = MappingWriter::open(&handle, size + 1).expect_err("a short slab");
        assert!(matches!(err, SlabError::Short { .. }), "got {err}");
    }

    /// Two slabs made by one process are two files: a created slab never meets
    /// another's bytes.
    #[test]
    fn two_creates_make_two_slabs() {
        let size = SLAB_HEADER_BYTES + 16;
        let (mut a, ha) = MappingWriter::create(size).unwrap();
        let (_b, hb) = MappingWriter::create(size).unwrap();
        a.write(SLAB_HEADER_BYTES, &[1u8; 16]);
        let peer = MappingWriter::open(&hb, size).unwrap();
        let mut back = [0u8; 16];
        peer.mapping().read(SLAB_HEADER_BYTES, &mut back);
        assert_eq!(back, [0u8; 16], "the second slab holds the first's bytes");
        assert_ne!(ha.name, hb.name);
    }

    /// A handle decoded from a peer's bytes is a name this module could have
    /// made: one that climbs out of the shared directory, or is empty, does not
    /// decode, and one `create` made does.
    #[test]
    fn a_decoded_handle_must_be_a_name_create_could_have_made() {
        let decode = |name: &str| SlabHandle::try_from(WireName(name.to_owned()));
        let (_w, made) = MappingWriter::create(SLAB_HEADER_BYTES + 16).unwrap();
        assert!(decode(&made.name).is_ok(), "a made name is refused");
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

    /// **`create` is exclusive.** The same name made twice through the
    /// platform's create fails the second time: an existing file (a collision,
    /// or one a peer planted) is refused, never truncated or reused.
    #[test]
    fn creating_an_existing_name_is_refused() {
        let size = SLAB_HEADER_BYTES + 16;
        let (_w, existing) = MappingWriter::create(size).unwrap();
        let again = platform::open_file(&existing.name, size, Open::Create);
        assert!(again.is_err(), "a second create reused the name");
    }

    /// A symlink planted at the handle's name is refused by `open`
    /// (`O_NOFOLLOW`).
    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_name_is_refused() {
        let size = SLAB_HEADER_BYTES + 64;
        let (_owner, real) = MappingWriter::create(size).unwrap();
        let link = handle_of(&format!("{}_link", real.name));
        std::os::unix::fs::symlink(platform::path(&real.name), platform::path(&link.name)).unwrap();
        let opened = MappingWriter::open(&link, size);
        let _ = std::fs::remove_file(platform::path(&link.name));
        assert!(opened.is_err(), "open followed a link");
    }

    /// Opening a file that is not a slab must fail on the magic, not produce a
    /// mapping full of someone else's bytes read as audio.
    #[test]
    fn opening_a_foreign_file_is_refused() {
        let size = SLAB_HEADER_BYTES + 64;
        let foreign = handle_of(&format!("ffi_foreign_{}", std::process::id()));
        std::fs::write(platform::path(&foreign.name), vec![0xABu8; size]).unwrap();
        let err = MappingWriter::open(&foreign, size).expect_err("a foreign file must not open");
        std::fs::remove_file(platform::path(&foreign.name)).unwrap();
        assert!(
            matches!(err, SlabError::BadMagic { found, expected: SLAB_MAGIC } if found == 0xABAB_ABAB_ABAB_ABAB),
            "expected a magic failure, got: {err}"
        );
    }

    /// No openable slab outlives the last of its mappings: after the owner and
    /// its peer drop, the handle opens no more (unlinked on unix, deleted or
    /// delete-pending on Windows). CI runs this on unix only.
    #[test]
    fn nothing_outlives_the_last_mapping() {
        let size = SLAB_HEADER_BYTES + 64;
        let (owner, handle) = MappingWriter::create(size).unwrap();
        let peer = MappingWriter::open(&handle, size).unwrap();
        drop(owner);
        drop(peer);
        assert!(
            MappingWriter::open(&handle, size).is_err(),
            "the slab opens after its mappings are gone"
        );
    }

    /// The owner dropping first leaves the peer's mapping working, and the
    /// peer's drop is what ends the slab. CI runs this on unix only.
    #[test]
    fn the_peer_outlives_the_owner_and_ends_the_slab() {
        let size = SLAB_HEADER_BYTES + 16;
        let (owner, handle) = MappingWriter::create(size).unwrap();
        let mut peer = MappingWriter::open(&handle, size).unwrap();
        drop(owner);
        peer.write(SLAB_HEADER_BYTES, &[9u8; 16]);
        let mut back = [0u8; 16];
        peer.mapping().read(SLAB_HEADER_BYTES, &mut back);
        assert_eq!(back, [9u8; 16]);
        drop(peer);
        assert!(
            MappingWriter::open(&handle, size).is_err(),
            "the slab opens after its mappings are gone"
        );
    }

    /// A creation that fails after the file exists leaves no file: a slab
    /// shorter than its header trips the mapping's assertion, and no file is
    /// left once the panic has unwound.
    #[test]
    fn a_failed_create_leaves_no_file() {
        let before = slab_files();
        let made = std::panic::catch_unwind(|| MappingWriter::create(1));
        assert!(made.is_err(), "a slab shorter than its header was made");
        assert_eq!(slab_files(), before, "the failed create left its file");
    }

    /// The backing files of this process's slabs, by name.
    fn slab_files() -> Vec<String> {
        let prefix = format!("tutti_{NAME_PREFIX}{}-", std::process::id());
        let dir = platform::path("x").parent().unwrap().to_owned();
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(&prefix))
            .collect();
        names.sort();
        names
    }
}
