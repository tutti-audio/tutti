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
//! region is ordered by the borrow checker. That, and the memory being made here
//! and never resized (sealed against it on Linux, a file in the temp directory's
//! ACL on Windows), is what lets every call in this module be safe.
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
//! one. On unix the handle is a descriptor, sent over the control socket
//! ([`SlabHandle::send`], [`SlabHandle::receive`]) beside the frame that carries
//! its [`SlabWire`]; the memory has no name anywhere. On Windows the wire form is
//! the name of a temp file.
//!
//! The backing's platform is one module, chosen once below: what the memory is,
//! how it travels, and what keeps it from outliving its last mapping.

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("the audio slab is supported on Linux, macOS and Windows only");
#[cfg(target_os = "linux")]
#[path = "mapping/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "mapping/macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "mapping/windows.rs"]
mod platform;

mod blob;

use crate::SlabError;
use crate::header::{SLAB_HEADER_BYTES, SlabHeader};
pub use blob::{Blob, BlobWire, Lease, Received};
use memmap2::{MmapOptions, MmapRaw};
pub use platform::{Connection, SlabWire};
use std::fs::File;
use std::sync::Arc;

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

/// What a peer needs to open a slab made by [`MappingWriter::create`]: handed
/// out by `create`, sent to the peer ([`send`](Self::send), with its
/// [`wire`](Self::wire) in the frame) and taken by [`MappingWriter::open`].
///
/// Opaque: its contents are private to this module, so nothing outside can
/// read or construct one. On unix it is the memory's descriptor, which only a
/// received one or a made one can be; it does not serialize, so it cannot be
/// written into a frame in place of a send.
#[derive(Debug)]
pub struct SlabHandle(platform::Slab);

impl SlabHandle {
    /// What goes in the frame that announces this slab.
    #[must_use]
    pub fn wire(&self) -> SlabWire {
        platform::wire(&self.0)
    }

    /// Sends the slab to the peer over `conn`, immediately after the frame
    /// that carries its [`wire`](Self::wire) has been written.
    ///
    /// # Errors
    ///
    /// The OS error of the send, a send timeout included: the connection is
    /// then mid-message and cannot go on.
    pub fn send(&self, conn: Connection<'_>) -> std::io::Result<()> {
        platform::send(conn, &self.0)
    }

    /// Takes the slab the frame carrying `wire` announced, over `conn`, right
    /// after that frame was read.
    ///
    /// # Errors
    ///
    /// [`SlabError::NoDescriptor`], [`SlabError::ExtraDescriptors`] or
    /// [`SlabError::Transfer`] if what arrived is not exactly one descriptor.
    pub fn receive(conn: Connection<'_>, wire: SlabWire) -> Result<Self, SlabError> {
        platform::receive(conn, wire).map(Self)
    }
}

impl MappingWriter {
    /// Creates a slab `size` bytes long, maps it and stamps its header, before
    /// any peer holds its [`SlabHandle`], which is returned beside the writer.
    ///
    /// On Linux the memory is a sealed `memfd`: it has no name and neither end
    /// can shrink or grow it. On macOS it is a POSIX shared memory object,
    /// unlinked as soon as it is sized, and sealed by nothing. On Windows it is
    /// a file in the temp directory, created exclusively and opened
    /// delete-on-close; the creator keeps that handle, so it is deleted when
    /// the last handle and view of it close.
    ///
    /// # Errors
    ///
    /// [`SlabError::Name`] if the random source fails, and
    /// [`SlabError::File`] if the memory cannot be created, sized, sealed or
    /// mapped.
    ///
    /// # Panics
    ///
    /// If `size` is smaller than [`SLAB_HEADER_BYTES`]: the caller sizes the
    /// slab from a layout that always includes the header.
    pub fn create(size: usize) -> Result<(Self, SlabHandle), SlabError> {
        let (file, backing, slab) = platform::create(size)?;
        let writer = Self::map(file, backing, size)?;
        // Stamp before anyone can open it: the peer gets the handle only
        // after this returns, and `initialize` releases the magic last so an
        // opener that sees the magic sees a fully written header.
        writer.mapping.header().initialize();
        Ok((writer, SlabHandle(slab)))
    }

    /// Opens the peer's slab that `handle` is, checking that it is exactly
    /// `size` bytes (at least, on macOS and Windows), sealed against resizing
    /// where the platform has seals, and a tutti slab of this build's header.
    ///
    /// # Errors
    ///
    /// [`SlabError::File`] if it cannot be mapped, [`SlabError::NotSealed`] if
    /// the memory is not a memfd sealed as `create` seals it (Linux),
    /// [`SlabError::Short`] or [`SlabError::Long`] if its length is not
    /// `size`, and [`SlabError::BadMagic`], [`SlabError::BadVersion`] or
    /// [`SlabError::BadSlots`] if it is not a tutti slab of this header version
    /// and ring depth.
    ///
    /// # Panics
    ///
    /// As for [`create`](Self::create).
    pub fn open(handle: &SlabHandle, size: usize) -> Result<Self, SlabError> {
        let (file, backing) = platform::open(&handle.0)?;
        platform::verify(&file, size)?;
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
        let writer = Self::map(file, backing, size)?;
        writer.mapping.header().validate()?;
        Ok(writer)
    }

    fn map(file: File, backing: platform::Backing, size: usize) -> Result<Self, SlabError> {
        // The memory is sized once, in `create`, before any peer holds it. On
        // Linux a received descriptor is checked sealed against shrinking
        // before it is mapped (`verify`), so no process can fault this mapping
        // by truncating the memory. Where the platform has no seals, shrinking
        // is a fault of the peer holding the descriptor, as is scribbling on it
        // (the module's argument).
        // Exactly the layout's `size`, not the object's length: where the platform
        // rounds an object up (macOS, to a page) the mapping still ends where the
        // layout does.
        let map = MmapOptions::new()
            .len(size)
            .map_raw(&file)
            .map_err(|source| SlabError::File { op: "map", source })?;
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

    /// The mapping's copies refuse the header's bytes: the header is only ever
    /// stored through its atomics, while `header()` hands out a shared
    /// reference to them.
    #[test]
    #[should_panic(expected = "outside the data region")]
    fn a_copy_into_the_header_panics() {
        let (mut w, _handle) = MappingWriter::create(SLAB_HEADER_BYTES + 64).unwrap();
        w.write(0, &[0u8; 8]);
    }

    /// An opener reads back what the creator wrote, and refuses a slab shorter
    /// than the size it asked for.
    #[test]
    fn open_sees_the_creators_bytes_and_refuses_a_short_slab() {
        let size = SLAB_HEADER_BYTES + 16;
        let (mut w, handle) = MappingWriter::create(size).unwrap();
        w.write(SLAB_HEADER_BYTES, &[7u8; 16]);
        let peer = MappingWriter::open(&handle, size).unwrap();
        let mut back = [0u8; 16];
        peer.mapping().read(SLAB_HEADER_BYTES, &mut back);
        assert_eq!(back, [7u8; 16]);
        // More than the slab holds by far: past a page the platform may round it up to.
        let err = MappingWriter::open(&handle, size + (1 << 20)).expect_err("a short slab");
        assert!(matches!(err, SlabError::Short { .. }), "got {err}");
    }

    /// Two slabs made by one process are two memories: a created slab never
    /// meets another's bytes.
    #[test]
    fn two_creates_make_two_slabs() {
        let size = SLAB_HEADER_BYTES + 16;
        let (mut a, _ha) = MappingWriter::create(size).unwrap();
        let (_b, hb) = MappingWriter::create(size).unwrap();
        a.write(SLAB_HEADER_BYTES, &[1u8; 16]);
        let peer = MappingWriter::open(&hb, size).unwrap();
        let mut back = [0u8; 16];
        peer.mapping().read(SLAB_HEADER_BYTES, &mut back);
        assert_eq!(back, [0u8; 16], "the second slab holds the first's bytes");
    }
}
