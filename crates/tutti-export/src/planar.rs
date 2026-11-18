//! The planar copies of a render: whole blocks between the interleaved shape
//! an encoder takes and the planes a converter or a buffer holds.

use crate::Result;
use tutti_buffers::{
    BufferError, Interleaved, InterleavedMut, Planes, PlanesMut, Sample, deinterleave, interleave,
};
use tutti_types::ChannelLayout;
use tutti_units::Samples;

/// Deinterleaves `block` into `planes`, converting to the planes' format, each
/// plane sized to the block's frames (a plane grows only past its capacity).
pub(crate) fn split<A: Sample, B: Sample>(
    block: Interleaved<'_, A>,
    planes: &mut [Vec<B>],
) -> Result<()> {
    for plane in planes.iter_mut() {
        plane.clear();
        plane.resize(block.frames().get(), B::SILENCE);
    }
    deinterleave(block, PlanesMut::from_vecs(planes)?)?;
    Ok(())
}

/// Interleaves `planes` into `staging`, converting to its format; `staging` is
/// resized to hold exactly them. No planes interleave to no frames.
pub(crate) fn join<A: Sample, B: Sample>(
    planes: &[Vec<A>],
    staging: &mut Vec<B>,
    layout: ChannelLayout,
) -> Result<()> {
    let frames = Samples(planes.first().map_or(0, Vec::len));
    staging.clear();
    staging.resize(layout.interleaved_len(frames), B::SILENCE);
    if planes.is_empty() {
        return Ok(());
    }
    interleave(
        Planes::from_vecs(planes)?,
        InterleavedMut::new(staging, layout)?,
    )?;
    Ok(())
}

/// Appends channel `c` of `block` to the end of `planes[c]`: the caller's own
/// extend of its planes, which the owned `Wave` replaces.
///
/// # Errors
///
/// [`BufferError::WidthMismatch`] when `planes` is not as wide as `block`;
/// nothing is appended.
pub(crate) fn append<S: Sample>(block: Interleaved<'_, S>, planes: &mut [Vec<S>]) -> Result<()> {
    if planes.len() != block.channels() {
        return Err(BufferError::WidthMismatch {
            src: block.layout(),
            dst: ChannelLayout::try_from(planes.len()).map_err(BufferError::from)?,
        }
        .into());
    }
    let frames = block.frames().get();
    for (c, plane) in planes.iter_mut().enumerate() {
        plane.reserve(frames);
        plane.extend(block.iter().map(|frame| frame[c]));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
