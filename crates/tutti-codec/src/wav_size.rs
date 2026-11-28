//! The size limits of a WAV file: its sizes are 32-bit.

use crate::pcm::Pcm;
use tutti_buffers::ChannelLayout;

/// The largest header written before the data (the extensible one), less the 8
/// bytes the RIFF size excludes: the RIFF size field is `data + this` and must
/// also fit a `u32`.
pub(crate) const RIFF_HEADER_BYTES: u64 = 60;

/// The most data bytes a (non-RF64) WAV file can hold: both the `data` chunk
/// size and the RIFF size, which adds the header, are `u32`.
pub(crate) const MAX_DATA_BYTES: u64 = u32::MAX as u64 - RIFF_HEADER_BYTES;

/// The most whole frames a WAV file of `layout` at `pcm` can hold under
/// [`MAX_DATA_BYTES`]. A writer that stops here leaves a valid file ending on a
/// frame boundary.
pub(crate) fn max_data_frames(layout: ChannelLayout, pcm: Pcm) -> u64 {
    MAX_DATA_BYTES / frame_bytes(layout, pcm)
}

/// Bytes per frame: every sample in a whole number of bytes.
pub(crate) fn frame_bytes(layout: ChannelLayout, pcm: Pcm) -> u64 {
    u64::from(pcm.bits().div_ceil(8)) * u64::from(layout.count().max(1))
}
