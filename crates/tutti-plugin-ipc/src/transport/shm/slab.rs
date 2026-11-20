//! Cross-process audio storage: a header plus two independent rings of
//! `channels × samples_per_channel` samples, one per direction.
//!
//! Layout is **mono-planar** — one contiguous region per channel, addressed by
//! index — not interleaved, because the plugin ABIs it feeds are deinterleaved
//! and the memcpy must stay a straight per-channel copy with no transpose. It is
//! a transport, not an edge (`tutti_buffers`' `Source`/`Sink`).
//!
//! # Why shared memory
//!
//! Host and plugin are separate processes, so they share no memory by default,
//! and audio crosses the boundary every block on the audio thread — which must
//! never block, allocate, or enter the kernel. Over the control socket that
//! would be serialize + two syscalls + kernel copies per block.
//!
//! Instead both processes `mmap` the *same* RAM-backed file (`/dev/shm`
//! on Linux, a temp file elsewhere), so the kernel maps the same physical pages
//! into both and transfer is a plain `memcpy`.
//!
//! # Synchronization lives here, not above
//!
//! Delegating the single-writer invariant to a handshake "one layer up" fails
//! invisibly, because this layer cannot report the gap: a read of an untouched
//! region returns full length and plausible bytes. With both directions aliased
//! onto one offset, an unmatched read hands the host its own input back — a
//! silent bypass that measures as working audio.
//!
//! Validity is therefore answered *in the slab*, by `SlabHeader`'s per-slot
//! sequence numbers, and the two directions occupy disjoint regions:
//!
//! - A writer fills every channel of a slot, then calls `publish` **once**. Its
//!   first write of a block marks the slot busy.
//! - A reader calls `*_sequence` first and only copies if it matches the block
//!   it wants, then rechecks; otherwise it substitutes silence.
//!
//! `read_*_into` still returns a copy count, but that count means only "how many
//! samples I copied" — **it is not evidence that anyone wrote them**. The
//! sequence number is. See `header.rs` for the Release/Acquire argument.
//!
//! Samples stay raw `f32`/`f64`, not unit newtypes: this is a C-ABI / IPC
//! boundary where the layout must be exactly the primitive's.
//!
//! # Lifecycle
//!
//! The host [`create`s](AudioSlab::create) the slab (the backing file ends with
//! the creator's mapping) and stamps its header; the server
//! [`open`s](AudioSlab::open) it (a `View`) using a matching [`SlabLayout`]
//! and validates that header before trusting a byte.
//!
//! # One writer per direction
//!
//! Each call hands back a [`SlabWriter`], the only way to write: `create`'s
//! writes the inputs (the host's direction), `open`'s the outputs (the
//! server's). It is not `Clone` and writes through `&mut self`, so within a
//! process a direction has one writer, and the [`AudioSlab`] everyone else
//! shares (`writer.slab()`, an `Arc`) can only read. Across processes the two
//! directions are disjoint regions, so the two writers never meet.
//!
//! # A read is evidence
//!
//! A `read_*_into` copies a slot only between two loads of its sequence that
//! both name the block asked for, with an acquire fence before the second. If
//! either does not, the peer had not published that block or was rewriting the
//! slot (a busy slot names no block) during the copy, and the
//! read fills the destination with silence and returns 0. So a non-zero count
//! means "these are block `seq`'s samples", and torn audio cannot leak out.
//!

use crate::SlabLayout;
use std::sync::Arc;
use tutti_buffers::Float;
use tutti_buffers::FloatWidth;
use tutti_plugin_types::MAX_BLOCK_CHANNELS;
use tutti_plugin_types::{BusChannels, ChannelLayout};
use tutti_plugin_types::{IpcError, IpcResult, SlabAccessError, SlabLayoutError};

use tutti_plugin_ipc_ffi::{
    Direction, MappingWriter, RING_SLOTS, SLAB_HEADER_BYTES, SharedMapping, SlabHandle, slot_for,
};

/// Shared mmap region: a `SlabHeader`, then a ring per direction.
///
/// Create on one side, open on the other with a matching [`SlabLayout`].
/// The backing file ends with the creator's mapping; the opener is a view.
#[derive(Debug)]
pub struct AudioSlab {
    mapping: Arc<SharedMapping>,
    layout: SlabLayout,
}

impl AudioSlab {
    // ---- Construction: one side creates, the other opens ----

    /// Creates the slab, the host's end: allocates the backing file, sizes it to hold the header and both rings, maps it, and stamps the
    /// header. Returns the writer of the inputs and the [`SlabHandle`] the other
    /// side opens it with.
    ///
    /// The file ends with this side's mapping. Exactly one side
    /// calls this; the other calls [`open`](Self::open) with a matching
    /// `layout`. The file is created exclusively, under a name no caller
    /// chooses: an existing file (a symlink included) is an error, never a
    /// file to truncate, since another process could hold it mapped.
    ///
    /// # Errors
    ///
    /// Returns an error if `layout` is refused ([`SlabLayoutError`]), or if the
    /// backing file exists or cannot be created, sized or mapped.
    pub fn create(layout: SlabLayout) -> IpcResult<(SlabWriter, SlabHandle)> {
        let bytes = check_layout(&layout)?;
        // Sequenced before it returns: the peer gets the handle only after
        // this, and the header's magic is released last.
        let (writer, handle) = MappingWriter::create(bytes).map_err(IpcError::from)?;
        let slab = Self {
            mapping: Arc::clone(writer.mapping()),
            layout,
        };
        Ok((SlabWriter::new(slab, writer, Direction::Input), handle))
    }

    /// Opens an existing slab as a view, the server's end, validating its
    /// header. Returns the writer of the outputs.
    ///
    /// The `layout` must match the one the owner created it with; both sides
    /// agree on the shape over the control channel before mapping. Detaches on
    /// drop without deleting the backing file.
    ///
    /// # Errors
    ///
    /// Returns an error if `layout` is invalid (as for
    /// [`create`](Self::create)), the file cannot be opened or mapped, or it is
    /// too short, is not a tutti slab, or was written with a different header
    /// version.
    pub fn open(handle: &SlabHandle, layout: SlabLayout) -> IpcResult<SlabWriter> {
        let bytes = check_layout(&layout)?;
        // Checks the length before the header, and the header before this
        // side trusts a byte.
        let writer = MappingWriter::open(handle, bytes).map_err(IpcError::from)?;
        let slab = Self {
            mapping: Arc::clone(writer.mapping()),
            layout,
        };
        Ok(SlabWriter::new(slab, writer, Direction::Output))
    }

    // ---- Accessors ----

    /// Returns a clone of the layout. For the allocation-free audio path, prefer
    /// [`layout_ref`](Self::layout_ref).
    pub fn layout(&self) -> SlabLayout {
        self.layout.clone()
    }

    /// Borrows the layout without cloning its (heap-backed) bus list — for the
    /// allocation-free audio path. [`layout`](Self::layout) (which clones) is
    /// for callers that need to own a copy.
    pub fn layout_ref(&self) -> &SlabLayout {
        &self.layout
    }

    // ---- Per-block reads: the hot path ----
    //
    // Each is a bounds check plus a `memcpy` out of the mapped region, between
    // two sequence loads: no syscall, no allocation, safe on the audio thread.
    // Writing is `SlabWriter`'s.

    /// Copies one channel of the input ring's slot for block `seq` into
    /// `output`.
    ///
    /// Server side. Returns how many samples were copied, which are block
    /// `seq`'s; 0, with `output` silenced, when the slot does not hold that
    /// block for the whole copy (module docs, "A read is evidence").
    ///
    /// # Errors
    ///
    /// Returns an error if `channel` is out of range.
    pub fn read_input_into<T: Float>(
        &self,
        seq: u64,
        channel: usize,
        output: &mut [T],
    ) -> Result<usize, SlabAccessError> {
        self.read_region(Direction::Input, seq, channel, output)
    }

    /// Copies one channel of the output ring's slot for block `seq` into
    /// `output`.
    ///
    /// Host side. Returns how many samples were copied, which are block
    /// `seq`'s; 0, with `output` silenced, when the slot does not hold that
    /// block for the whole copy (module docs, "A read is evidence").
    ///
    /// # Errors
    ///
    /// Returns an error if `channel` is out of range.
    pub fn read_output_into<T: Float>(
        &self,
        seq: u64,
        channel: usize,
        output: &mut [T],
    ) -> Result<usize, SlabAccessError> {
        self.read_region(Direction::Output, seq, channel, output)
    }

    /// Which block currently occupies the input slot that `seq` maps to.
    ///
    /// Equal to `seq` means "block `seq`'s inputs are there and complete".
    /// Anything else means the slot holds a different block — read nothing.
    #[inline]
    pub(crate) fn input_sequence(&self, seq: u64) -> u64 {
        self.mapping
            .header()
            .sequence(Direction::Input, slot_for(seq))
    }

    /// Which block currently occupies the output slot that `seq` maps to.
    #[inline]
    pub(crate) fn output_sequence(&self, seq: u64) -> u64 {
        self.mapping
            .header()
            .sequence(Direction::Output, slot_for(seq))
    }

    /// True when the output ring holds block `seq` — the single check the host
    /// makes before reading a block back.
    #[inline]
    pub fn has_output(&self, seq: u64) -> bool {
        seq != 0 && self.output_sequence(seq) == seq
    }

    /// True when the input ring holds block `seq` — the server's check before
    /// feeding the plugin.
    #[inline]
    pub fn has_input(&self, seq: u64) -> bool {
        seq != 0 && self.input_sequence(seq) == seq
    }

    // ---- Internal helpers ----

    /// Only [`SlabWriter`] calls this, with the mapping's one writer.
    fn write_region<T: Float>(
        &self,
        writer: &mut MappingWriter,
        open: &mut Option<u64>,
        direction: Direction,
        seq: u64,
        channel: usize,
        data: &[T],
    ) -> Result<(), SlabAccessError> {
        self.check_width::<T>()?;
        let offset = self.offset_of(direction, seq, channel)?;
        if data.len() > self.layout.samples_per_channel {
            return Err(SlabAccessError::Length);
        }
        if *open != Some(seq) {
            self.mapping.header().begin(direction, slot_for(seq));
            *open = Some(seq);
        }
        writer.write(offset, bytemuck::cast_slice(data));
        Ok(())
    }

    /// The slot is a seqlock (`header.rs`): the copy sits between a load of
    /// the sequence that names `seq` and a `recheck`
    /// that still does. The writer marks a slot busy before it rewrites it, so
    /// a copy overtaken by a later block, whether the peer finished it or is
    /// still mid-write, fails the recheck.
    fn read_region<T: Float>(
        &self,
        direction: Direction,
        seq: u64,
        channel: usize,
        output: &mut [T],
    ) -> Result<usize, SlabAccessError> {
        self.check_width::<T>()?;
        let offset = self.offset_of(direction, seq, channel)?;
        let copy_samples = self.layout.samples_per_channel.min(output.len());
        let slot = slot_for(seq);
        let header = self.mapping.header();
        let held = seq != 0 && header.sequence(direction, slot) == seq;
        if held {
            self.mapping.read(
                offset,
                bytemuck::cast_slice_mut(&mut output[..copy_samples]),
            );
        }
        if !held || !header.recheck(direction, slot, seq) {
            output.fill(T::default());
            return Ok(0);
        }
        Ok(copy_samples)
    }

    /// The sample type a caller names must be the width the slab was laid out
    /// at: the layout's stride is in that width's bytes, so a wider type would
    /// write past its channel and a narrower one would read the wrong samples.
    fn check_width<T: Float>(&self) -> Result<(), SlabAccessError> {
        if T::WIDTH != self.layout.format {
            return Err(SlabAccessError::SampleWidth);
        }
        Ok(())
    }

    /// Byte offset of one channel of one slot of one direction's ring.
    ///
    /// Slot-major within each region, so a whole block's write stays
    /// sequential — one slot's channels are contiguous rather than strided
    /// across the ring.
    ///
    /// Cannot overflow: the layout's whole size was computed with checked
    /// arithmetic when the slab was mapped (`byte_size`), and this offset is
    /// below it.
    fn offset_of(
        &self,
        direction: Direction,
        seq: u64,
        channel: usize,
    ) -> Result<usize, SlabAccessError> {
        let channels = match direction {
            Direction::Input => self.layout.input_channels(),
            Direction::Output => self.layout.output_channels(),
        };
        if channel >= channels {
            return Err(SlabAccessError::Channel);
        }
        let base = match direction {
            Direction::Input => SLAB_HEADER_BYTES,
            Direction::Output => SLAB_HEADER_BYTES + self.layout.input_ring_bytes(),
        };
        let stride = self.layout.channel_bytes();
        Ok(base + (slot_for(seq) * channels + channel) * stride)
    }
}

/// The one writer of a slab's direction in this process: the inputs for the
/// host ([`AudioSlab::create`]), the outputs for the server
/// ([`AudioSlab::open`]).
///
/// Not `Clone`, and it writes through `&mut self`, so two threads of this
/// process cannot write one region (module docs, "One writer per direction").
/// It reads through [`Deref`](std::ops::Deref) to the shared [`AudioSlab`],
/// which is also what [`slab`](Self::slab) hands to readers elsewhere.
///
/// A second writer cannot be made by copying this one:
///
/// ```compile_fail
/// fn second(w: &tutti_plugin_ipc::SlabWriter) -> tutti_plugin_ipc::SlabWriter {
///     w.clone()
/// }
/// ```
///
/// and the shared slab cannot write at all:
///
/// ```compile_fail
/// fn write(slab: &tutti_plugin_ipc::AudioSlab) {
///     let _ = slab.write(1, 0, &[0.0f32]);
/// }
/// ```
#[derive(Debug)]
pub struct SlabWriter {
    slab: Arc<AudioSlab>,
    writer: MappingWriter,
    direction: Direction,
    /// The block whose slot this writer has marked busy and not yet
    /// published; `None` between blocks.
    open: Option<u64>,
}

impl SlabWriter {
    fn new(slab: AudioSlab, writer: MappingWriter, direction: Direction) -> Self {
        Self {
            slab: Arc::new(slab),
            writer,
            direction,
            open: None,
        }
    }

    /// The shared, read-only slab, for the readers that do not write.
    #[must_use]
    pub fn slab(&self) -> &Arc<AudioSlab> {
        &self.slab
    }

    /// Copies `data` into one channel of this direction's slot for block `seq`.
    ///
    /// Call once per channel, then [`publish`](Self::publish) once. The first
    /// write of a block marks its slot busy, so the block's previous occupant
    /// is not readable while the new samples land. Allocation-free.
    ///
    /// # Errors
    ///
    /// Returns an error if `channel` is out of range, `data` is longer than the
    /// channel's capacity, or `T` is not the slab's sample width.
    pub fn write<T: Float>(
        &mut self,
        seq: u64,
        channel: usize,
        data: &[T],
    ) -> Result<(), SlabAccessError> {
        self.slab.write_region(
            &mut self.writer,
            &mut self.open,
            self.direction,
            seq,
            channel,
            data,
        )
    }

    /// Announces that block `seq` is complete in this direction.
    ///
    /// Once per block, after the last [`write`](Self::write). Publishing must
    /// happen exactly once, after the last channel, or a reader can observe a
    /// slot marked valid while later channels are still being copied.
    #[inline]
    pub fn publish(&mut self, seq: u64) {
        self.open = None;
        self.slab
            .mapping
            .header()
            .publish(self.direction, slot_for(seq), seq);
    }
}

impl std::ops::Deref for SlabWriter {
    type Target = AudioSlab;

    fn deref(&self) -> &AudioSlab {
        &self.slab
    }
}

impl SlabLayout {
    /// The layout of a plugin with these per-bus widths, at this build's ring
    /// depth: an empty direction becomes one bus of no channels.
    #[must_use]
    pub fn new(
        samples_per_channel: usize,
        format: FloatWidth,
        inputs: BusChannels,
        outputs: BusChannels,
    ) -> Self {
        let bus_or_empty = |buses: BusChannels| {
            if buses.is_empty() {
                vec![ChannelLayout::EMPTY]
            } else {
                buses
            }
        };
        Self {
            samples_per_channel,
            format,
            slots: RING_SLOTS as u32,
            inputs: bus_or_empty(inputs),
            outputs: bus_or_empty(outputs),
        }
    }
}

/// Reject a layout this build cannot address or serve, before it becomes a
/// mapping, and return the mapping's size, header included.
///
/// The layout comes over the wire on the server's side, so a peer can name any
/// shape: the bus and channel counts are capped at [`MAX_BLOCK_CHANNELS`] (the
/// width the render path's tables hold), and the size is computed with checked
/// arithmetic, so every later offset is below a figure that fits in `usize`.
/// An empty bus list and a mismatched slot count would otherwise be silently
/// representable and produce wrong audio rather than an error.
fn check_layout(layout: &SlabLayout) -> Result<usize, SlabLayoutError> {
    if layout.inputs.is_empty() || layout.outputs.is_empty() {
        return Err(SlabLayoutError::NoBus);
    }
    if layout.slots as usize != RING_SLOTS {
        return Err(SlabLayoutError::Slots {
            asked: layout.slots,
            served: RING_SLOTS as u32,
        });
    }
    for (direction, buses) in [("input", &layout.inputs), ("output", &layout.outputs)] {
        if buses.len() > MAX_BLOCK_CHANNELS {
            return Err(SlabLayoutError::TooManyBuses {
                direction,
                buses: buses.len(),
            });
        }
        // At most MAX_BLOCK_CHANNELS buses, so the sum cannot overflow.
        let channels: u64 = buses.iter().map(|b| u64::from(b.count())).sum();
        if channels > MAX_BLOCK_CHANNELS as u64 {
            return Err(SlabLayoutError::TooManyChannels {
                direction,
                channels,
            });
        }
    }
    layout
        .checked_byte_size_with_header(SLAB_HEADER_BYTES)
        .ok_or(SlabLayoutError::SizeOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::FloatWidth;
    use tutti_plugin_types::ChannelLayout;

    /// A real two-bus-per-direction layout. Deliberately *asymmetric* (3 in, 2
    /// out): built at base 0 with equal widths, an offset error that swapped the
    /// directions or dropped the slot term still produces matching bytes. With
    /// different widths and a non-zero output base, those mistakes cannot
    /// round-trip.
    fn layout(samples: usize, format: FloatWidth) -> SlabLayout {
        SlabLayout {
            samples_per_channel: samples,
            format,
            slots: RING_SLOTS as u32,
            inputs: vec![ChannelLayout::STEREO, ChannelLayout::MONO],
            outputs: vec![ChannelLayout::STEREO],
        }
    }

    fn stereo_layout(samples: usize, format: FloatWidth) -> SlabLayout {
        SlabLayout {
            samples_per_channel: samples,
            format,
            slots: RING_SLOTS as u32,
            inputs: vec![ChannelLayout::STEREO],
            outputs: vec![ChannelLayout::STEREO],
        }
    }

    /// Both ends of one slab in this process: the host's input writer
    /// (`create`) and the server's output writer (`open`).
    fn pair(l: SlabLayout) -> (SlabWriter, SlabWriter) {
        let (host, handle) = AudioSlab::create(l.clone()).unwrap();
        let server = AudioSlab::open(&handle, l).unwrap();
        (host, server)
    }

    /// **The size a slab is mapped at and the offsets it is addressed by agree.**
    /// The last channel of the last slot of the output ring ends exactly at
    /// the checked size, at either width: a stride the size and the addressing
    /// computed differently would leave a gap or run past the mapping.
    #[test]
    fn the_last_channel_ends_at_the_checked_size() {
        for width in [FloatWidth::F32, FloatWidth::F64] {
            let l = layout(64, width);
            let (host, _handle) = AudioSlab::create(l.clone()).unwrap();
            let last_slot = (RING_SLOTS - 1) as u64;
            let last_channel = l.output_channels() - 1;
            let end = host
                .slab()
                .offset_of(Direction::Output, last_slot, last_channel)
                .unwrap()
                + l.channel_bytes();
            assert_eq!(
                Some(end),
                l.checked_byte_size_with_header(SLAB_HEADER_BYTES),
                "{width:?}"
            );
        }
    }

    /// **A peer cannot name a layout the render path cannot serve.** A
    /// direction past `MAX_BLOCK_CHANNELS` channels or buses, and a size that
    /// overflows `usize`, are refused by name at `create` and at `open`; the
    /// widest served layout is accepted.
    #[test]
    fn a_layout_past_the_served_width_or_size_is_refused_by_name() {
        let wide = |count: usize| SlabLayout {
            inputs: vec![ChannelLayout::clamped(count)],
            ..layout(64, FloatWidth::F32)
        };
        let many = |count: usize| SlabLayout {
            outputs: vec![ChannelLayout::MONO; count],
            ..layout(64, FloatWidth::F32)
        };
        let huge = SlabLayout {
            samples_per_channel: usize::MAX / 8,
            ..layout(64, FloatWidth::F32)
        };
        let (_owner, handle) = AudioSlab::create(layout(64, FloatWidth::F32)).unwrap();
        let max = MAX_BLOCK_CHANNELS;
        assert!(check_layout(&wide(max)).is_ok(), "the widest layout serves");
        assert!(check_layout(&many(max)).is_ok(), "the most buses serve");
        let refusals = [
            (
                wide(max + 1),
                SlabLayoutError::TooManyChannels {
                    direction: "input",
                    channels: max as u64 + 1,
                },
            ),
            (
                many(max + 1),
                SlabLayoutError::TooManyBuses {
                    direction: "output",
                    buses: max + 1,
                },
            ),
            (huge, SlabLayoutError::SizeOverflow),
        ];
        for (layout, refusal) in refusals {
            assert_eq!(check_layout(&layout), Err(refusal.clone()));
            assert!(
                AudioSlab::create(layout.clone()).is_err(),
                "create: {refusal}"
            );
            assert!(AudioSlab::open(&handle, layout).is_err(), "open: {refusal}");
        }
    }

    /// A caller's sample type must be the slab's width: a mismatch is an error
    /// both ways and touches nothing.
    #[test]
    fn a_sample_type_of_the_wrong_width_is_refused() {
        let (mut host, _server) = pair(layout(32, FloatWidth::F32));
        assert!(host.write(1, 0, &[0.5f64; 32]).is_err());
        assert!(host.read_input_into(1, 0, &mut [0.0f64; 32]).is_err());
        assert!(host.write(1, 0, &[0.5f32; 32]).is_ok());

        let (_host, mut wide) = pair(layout(32, FloatWidth::F64));
        assert!(wide.write(1, 0, &[0.5f32; 32]).is_err());
        assert!(wide.read_output_into(1, 0, &mut [0.0f32; 32]).is_err());
        assert!(wide.write(1, 0, &[0.5f64; 32]).is_ok());
    }

    #[test]
    fn roundtrip_f32() {
        let (mut host, server) = pair(layout(128, FloatWidth::F32));

        let data: Vec<f32> = (0..128).map(|i| i as f32 * 0.1).collect();
        host.write(1, 0, &data).unwrap();
        host.publish(1);

        assert!(server.has_input(1));
        let mut out = vec![0.0f32; 128];
        let got = server.read_input_into(1, 0, &mut out).unwrap();
        assert_eq!(got, 128);
        assert_eq!(data, out);
    }

    #[test]
    fn roundtrip_f64() {
        let (host, mut server) = pair(layout(64, FloatWidth::F64));
        let data: Vec<f64> = (0..64).map(|i| (i as f64).sin()).collect();
        server.write(1, 0, &data).unwrap();
        server.publish(1);

        let mut out = vec![0.0f64; 64];
        assert_eq!(host.read_output_into(1, 0, &mut out).unwrap(), 64);
        assert_eq!(data, out);
    }

    /// **Guards against a silent bypass.** The host writes its input and the
    /// server publishes block 1's output without writing a sample; reading
    /// that output must not hand the host's input back, including for a
    /// plugin with one bus per direction — the common case.
    ///
    /// The server publishes so the read is not refused on the sequence: with
    /// aliased regions, the published "output" would be the input.
    #[test]
    fn the_output_region_never_aliases_the_input_region() {
        let (mut host, mut server) = pair(stereo_layout(64, FloatWidth::F32));

        let input: Vec<f32> = (0..64).map(|i| i as f32 + 1.0).collect();
        for ch in 0..2 {
            host.write(1, ch, &input).unwrap();
        }
        host.publish(1);

        assert!(!host.has_output(1), "no output was published");
        server.publish(1);

        let mut out = vec![0.0f32; 64];
        for ch in 0..2 {
            assert_eq!(host.read_output_into(1, ch, &mut out).unwrap(), 64);
            assert!(
                out.iter().all(|&s| s == 0.0),
                "ch {ch}: reading the output region returned the host's own input — \
                 this is the bypass bug"
            );
        }
    }

    /// Reading a slot that holds a *different* block must be detectable, and
    /// the read must not hand back its bytes: they are real audio, just the
    /// wrong block's.
    #[test]
    fn a_stale_slot_is_detectable_and_never_read() {
        let (host, mut server) = pair(stereo_layout(64, FloatWidth::F32));

        let old: Vec<f32> = (0..64).map(|i| i as f32 + 1.0).collect();
        server.write(1, 0, &old).unwrap();
        server.publish(1);

        // Block 3 maps to the same slot as block 1 (depth 2).
        assert_eq!(slot_for(3), slot_for(1));
        assert!(!host.has_output(3), "slot holds block 1, not block 3");
        assert!(host.has_output(1));

        let mut out = vec![9.0f32; 64];
        assert_eq!(host.read_output_into(3, 0, &mut out).unwrap(), 0);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "a refused read leaves silence, not block 1's audio or the caller's old bytes"
        );
    }

    /// **Sequence 0 is no block.** Real sequences start at 1 and a never-written
    /// slot reads 0, so a peer-supplied 0 must not match one; a write of block 0
    /// still marks its slot busy, so it cannot be read half-written either.
    #[test]
    fn block_zero_is_never_held_and_marks_its_slot_busy() {
        let (host, mut server) = pair(stereo_layout(64, FloatWidth::F32));
        assert!(
            !host.has_output(0) && !host.has_input(0),
            "a fresh slab holds no block 0"
        );
        let mut out = vec![9.0f32; 64];
        assert_eq!(host.read_output_into(0, 0, &mut out).unwrap(), 0);
        assert!(out.iter().all(|&s| s == 0.0));

        server.write(2, 0, &[2.0f32; 64]).unwrap();
        server.publish(2);
        assert!(host.has_output(2));
        assert_eq!(slot_for(0), slot_for(2));
        server.write(0, 0, &[7.0f32; 64]).unwrap();
        assert!(!host.has_output(2), "block 0's write marked the slot busy");
    }

    /// **The busy mark.** The peer's first write of block 3 reuses block 1's
    /// slot, and until it publishes, the slot's sequence must stop naming
    /// block 1: a reader that began on block 1 would otherwise accept half of
    /// each block.
    #[test]
    fn a_slot_being_rewritten_is_not_read() {
        let (host, mut server) = pair(stereo_layout(64, FloatWidth::F32));
        server.write(1, 0, &[1.0f32; 64]).unwrap();
        server.publish(1);
        assert_eq!(slot_for(3), slot_for(1));
        server.write(3, 0, &[3.0f32; 64]).unwrap();

        assert!(!host.has_output(1), "the slot is half block 3");
        assert!(!host.has_output(3), "block 3 is not published");
        let mut out = vec![9.0f32; 64];
        assert_eq!(host.read_output_into(1, 0, &mut out).unwrap(), 0);
        assert!(out.iter().all(|&s| s == 0.0), "and the read leaves silence");

        server.write(3, 1, &[3.0f32; 64]).unwrap();
        server.publish(3);
        assert_eq!(host.read_output_into(3, 0, &mut out).unwrap(), 64);
        assert!(out.iter().all(|&s| s == 3.0));
    }

    /// A reader racing a writer that laps the ring never accepts a torn block:
    /// every block's samples are all its own sequence, so a copy that spans two
    /// blocks shows two values. The busy mark and the reader's fenced recheck
    /// are what keep it out; the reader must accept some blocks, or the test
    /// proves nothing.
    #[test]
    fn a_reader_racing_a_lapping_writer_never_accepts_a_torn_block() {
        use std::sync::atomic::{AtomicBool, Ordering};
        const SAMPLES: usize = 2048;
        const BLOCKS: u64 = 20_000;
        let (host, mut server) = pair(stereo_layout(SAMPLES, FloatWidth::F32));
        let slab = Arc::clone(host.slab());
        let done = Arc::new(AtomicBool::new(false));
        let writer = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let mut block = vec![0.0f32; SAMPLES];
                for seq in 1..=BLOCKS {
                    block.fill(seq as f32);
                    server.write(seq, 0, &block).unwrap();
                    server.write(seq, 1, &block).unwrap();
                    server.publish(seq);
                }
                done.store(true, Ordering::Release);
            })
        };
        let (mut accepted, mut torn) = (0u64, 0u64);
        let mut out = vec![0.0f32; SAMPLES];
        while !done.load(Ordering::Acquire) {
            for slot in 0..RING_SLOTS as u64 {
                let seq = slab.output_sequence(slot);
                if seq == 0 || slab.read_output_into(seq, 0, &mut out).unwrap() == 0 {
                    continue;
                }
                accepted += 1;
                torn += u64::from(out.iter().any(|&s| s != seq as f32));
            }
        }
        writer.join().expect("writer thread");
        assert!(accepted > 0, "the reader never caught a block");
        assert_eq!(torn, 0, "{torn} of {accepted} accepted reads were torn");
    }

    /// Consecutive blocks occupy different slots, which is what lets one be read
    /// while the next is written.
    #[test]
    fn consecutive_blocks_do_not_share_a_slot() {
        let (host, mut server) = pair(stereo_layout(64, FloatWidth::F32));

        let a = vec![1.0f32; 64];
        let b = vec![2.0f32; 64];
        server.write(1, 0, &a).unwrap();
        server.publish(1);
        server.write(2, 0, &b).unwrap();
        server.publish(2);

        assert!(host.has_output(1), "block 1 survived block 2's write");
        assert!(host.has_output(2));

        let mut out = vec![0.0f32; 64];
        host.read_output_into(1, 0, &mut out).unwrap();
        assert_eq!(out, a);
        host.read_output_into(2, 0, &mut out).unwrap();
        assert_eq!(out, b);
    }

    /// Every (direction, slot, channel) triple must be a distinct region.
    /// Written as an exhaustive fill-and-verify rather than an offset
    /// calculation, so it catches a wrong base, a dropped slot term, and a
    /// channel/slot transposition alike.
    #[test]
    fn every_region_is_distinct() {
        let l = layout(16, FloatWidth::F32);
        let (mut host, mut server) = pair(l.clone());

        // Stamp each region with a unique constant, and publish each block in
        // both directions so the reads below are not refused.
        let mut tag = 0.0f32;
        for seq in 1..=RING_SLOTS as u64 {
            for ch in 0..l.input_channels() {
                tag += 1.0;
                host.write(seq, ch, &[tag; 16]).unwrap();
            }
            for ch in 0..l.output_channels() {
                tag += 1.0;
                server.write(seq, ch, &[tag; 16]).unwrap();
            }
            host.publish(seq);
            server.publish(seq);
        }

        // Read every region back; each must still hold its own stamp.
        let mut expect = 0.0f32;
        let mut out = vec![0.0f32; 16];
        for seq in 1..=RING_SLOTS as u64 {
            for ch in 0..l.input_channels() {
                expect += 1.0;
                assert_eq!(server.read_input_into(seq, ch, &mut out).unwrap(), 16);
                assert!(
                    out.iter().all(|&s| s == expect),
                    "input seq={seq} ch={ch} was overwritten by another region"
                );
            }
            for ch in 0..l.output_channels() {
                expect += 1.0;
                assert_eq!(host.read_output_into(seq, ch, &mut out).unwrap(), 16);
                assert!(
                    out.iter().all(|&s| s == expect),
                    "output seq={seq} ch={ch} was overwritten by another region"
                );
            }
        }
    }

    /// The writer's direction is its constructor's: `create` writes inputs and
    /// `open` writes outputs. A writer of the wrong direction would let the
    /// host overwrite the plugin's answer.
    #[test]
    fn create_writes_inputs_and_open_writes_outputs() {
        let (mut host, mut server) = pair(stereo_layout(8, FloatWidth::F32));
        host.write(1, 0, &[1.0f32; 8]).unwrap();
        host.publish(1);
        server.write(1, 0, &[2.0f32; 8]).unwrap();
        server.publish(1);
        let mut out = [0.0f32; 8];
        assert!(server.has_input(1) && host.has_output(1));
        server.read_input_into(1, 0, &mut out).unwrap();
        assert_eq!(out, [1.0; 8]);
        host.read_output_into(1, 0, &mut out).unwrap();
        assert_eq!(out, [2.0; 8]);
    }

    #[test]
    fn channel_out_of_bounds() {
        let (mut host, mut server) = pair(layout(64, FloatWidth::F32));
        let data = vec![0.0f32; 64];
        // 3 input channels, 2 output channels — the asymmetry matters: an index
        // valid for one direction may not be valid for the other.
        assert!(host.write(1, 3, &data).is_err());
        assert!(server.write(1, 2, &data).is_err());
        assert!(host.write(1, 2, &data).is_ok(), "sidechain is in range");
        let mut out = vec![0.0f32; 64];
        assert!(host.read_output_into(1, 5, &mut out).is_err());
    }

    #[test]
    fn data_exceeds_capacity() {
        let (mut host, _server) = pair(stereo_layout(32, FloatWidth::F32));
        let data = vec![0.0f32; 64];
        assert!(host.write(1, 0, &data).is_err());
    }

    /// An empty bus list is the shape that meant "share one region in place".
    /// It must now be rejected outright rather than reinterpreted.
    #[test]
    fn an_empty_bus_list_is_refused() {
        let mut l = stereo_layout(64, FloatWidth::F32);
        l.inputs = Vec::new();
        assert!(AudioSlab::create(l).is_err());
    }

    /// A peer asking for a ring depth this build cannot address is refused,
    /// rather than silently indexing the wrong slot.
    #[test]
    fn a_foreign_ring_depth_is_refused() {
        let mut l = stereo_layout(64, FloatWidth::F32);
        l.slots = RING_SLOTS as u32 + 1;
        assert!(AudioSlab::create(l).is_err());
    }

    /// A layout whose size does not fit in `usize` is an error, from either
    /// end: the server sizes its mapping from a layout the peer sent, and a
    /// wrapped size would map a small file the layout's offsets run past.
    #[test]
    fn a_layout_too_large_to_address_is_refused() {
        // 2 slots x 2 channels x 2^62 frames: each ring's byte count wraps to
        // exactly 0, so a wrapping size would map just the header.
        let l = stereo_layout(1 << (usize::BITS - 2), FloatWidth::F32);
        let create = AudioSlab::create(l.clone()).expect_err("must not map");
        assert!(format!("{create}").contains("overflows"), "got: {create}");
        let (_owner, handle) = AudioSlab::create(stereo_layout(8, FloatWidth::F32)).unwrap();
        let open = AudioSlab::open(&handle, l).expect_err("must not map");
        assert!(format!("{open}").contains("overflows"), "got: {open}");
    }

    /// A slab created for a smaller layout must not be openable with a larger
    /// one — the mapping would end mid-region and every read past it would be
    /// out of bounds.
    #[test]
    fn opening_with_an_oversized_layout_is_refused() {
        let small = stereo_layout(64, FloatWidth::F32);
        let (_owner, handle) = AudioSlab::create(small).unwrap();

        let big = stereo_layout(256, FloatWidth::F32);
        let err = AudioSlab::open(&handle, big).expect_err("an oversized layout must not open");
        assert!(format!("{err}").contains("bytes"), "got: {err}");
    }

    /// Read the mapping's raw bytes and check the layout *without* going through
    /// the offset helper that produced it.
    ///
    /// Every other test here writes and reads through `offset_of`, so a
    /// consistent error in it — a dropped slot term, a swapped channel/slot
    /// factor, the wrong region base — cancels out and round-trips perfectly.
    /// This test recomputes each address from the layout arithmetic
    /// independently, so such an error shows up as a value in the wrong place
    /// rather than not at all.
    ///
    /// This is the by-hand `hexdump` from the pipelining plan, kept as a test
    /// because nothing about it needed a human eye — only bytes at addresses.
    #[test]
    fn raw_bytes_land_where_the_layout_says_they_should() {
        const SAMPLES: usize = 8;
        // 3 in / 2 out, so a swapped direction or channel count cannot alias.
        let l = layout(SAMPLES, FloatWidth::F32);
        let in_channels = l.input_channels();
        let out_channels = l.output_channels();
        assert_eq!(
            (in_channels, out_channels),
            (3, 2),
            "this test's arithmetic assumes the asymmetric helper layout"
        );

        let (mut host, mut server) = pair(l.clone());

        // A distinct constant per (direction, slot, channel). Every sample in a
        // channel carries the same value, so a misaddressed *channel* is
        // visible; the values are spread far enough apart that a misaddressed
        // *slot* or *region* is too.
        let tag = |dir: u8, slot: usize, ch: usize| -> f32 {
            (dir as f32) * 1000.0 + (slot as f32) * 100.0 + (ch as f32) + 1.0
        };

        for slot in 0..RING_SLOTS {
            // `seq` must be a sequence whose ring slot is `slot`; at depth 2 the
            // sequence value and the slot coincide for 0..RING_SLOTS.
            let seq = slot as u64;
            for ch in 0..in_channels {
                host.write(seq, ch, &[tag(0, slot, ch); SAMPLES]).unwrap();
            }
            for ch in 0..out_channels {
                server.write(seq, ch, &[tag(1, slot, ch); SAMPLES]).unwrap();
            }
        }

        // The data region at its absolute offsets; the header's bytes (and the
        // magic at offset 0) are `tutti-plugin-ipc-ffi`'s to check. The read ends
        // at the layout's last byte, so a mapping shorter than the layout panics.
        let mapping = &host.slab().mapping;
        let mut bytes = vec![0u8; check_layout(&l).unwrap()];
        mapping.read(SLAB_HEADER_BYTES, &mut bytes[SLAB_HEADER_BYTES..]);

        // 1. Audio starts after the header, and the two rings are disjoint.
        let stride = SAMPLES * size_of::<f32>();
        let input_base = SLAB_HEADER_BYTES;
        let output_base = SLAB_HEADER_BYTES + l.input_ring_bytes();
        assert_eq!(
            l.input_ring_bytes(),
            RING_SLOTS * in_channels * stride,
            "input ring must cover every slot x channel"
        );
        assert!(
            output_base >= input_base + l.input_ring_bytes(),
            "the output ring starts inside the input ring: bases {input_base} / {output_base}"
        );

        // 2. Every channel of every slot, addressed from the layout rather than
        //    from the code under test.
        let read_at = |offset: usize| -> f32 {
            f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
        };
        for slot in 0..RING_SLOTS {
            for ch in 0..in_channels {
                let at = input_base + (slot * in_channels + ch) * stride;
                assert_eq!(
                    read_at(at),
                    tag(0, slot, ch),
                    "input slot {slot} ch {ch} is not at byte {at}"
                );
            }
            for ch in 0..out_channels {
                let at = output_base + (slot * out_channels + ch) * stride;
                assert_eq!(
                    read_at(at),
                    tag(1, slot, ch),
                    "output slot {slot} ch {ch} is not at byte {at}"
                );
            }
        }

        // 3. The directions really do hold different data. The shipped bypass
        //    was precisely the case where reading "output" returned the input.
        assert_ne!(
            read_at(input_base),
            read_at(output_base),
            "input and output regions hold identical bytes — the aliasing bug"
        );
    }
}
