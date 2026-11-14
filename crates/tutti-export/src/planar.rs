//! The planar copies of a render: whole blocks between the interleaved shape
//! an encoder takes and the planes a converter or a buffer holds.

use crate::Result;
use tutti_buffers::{Interleaved, InterleavedMut, Planes, PlanesMut, deinterleave, interleave};
use tutti_types::{ChannelLayout, Samples};

/// Deinterleaves `block` into `planes`, each sized to the block's frames (a
/// plane grows only past its capacity).
pub(crate) fn split(block: Interleaved<'_>, planes: &mut [Vec<f32>]) -> Result<()> {
    for plane in planes.iter_mut() {
        plane.clear();
        plane.resize(block.frames().get(), 0.0);
    }
    deinterleave(block, PlanesMut::from_vecs(planes)?)?;
    Ok(())
}

/// Interleaves `planes` into `staging`, which is resized to hold exactly
/// them. No planes interleave to no frames.
pub(crate) fn join(
    planes: &[Vec<f32>],
    staging: &mut Vec<f32>,
    layout: ChannelLayout,
) -> Result<()> {
    let frames = Samples(planes.first().map_or(0, Vec::len));
    staging.clear();
    staging.resize(frames.interleaved_len(layout), 0.0);
    if planes.is_empty() {
        return Ok(());
    }
    interleave(
        Planes::from_vecs(planes)?,
        InterleavedMut::new(staging, layout)?,
    )?;
    Ok(())
}

/// Appends channel `c` of `block` to the end of `planes[c]`. Planes past the
/// block's width are left as they are, and channels past `planes.len()` are
/// skipped.
pub(crate) fn append(block: Interleaved<'_>, planes: &mut [Vec<f32>]) {
    let frames = block.frames().get();
    for (c, plane) in planes.iter_mut().enumerate().take(block.channels()) {
        plane.reserve(frames);
        plane.extend(block.iter().map(|frame| frame[c]));
    }
}
