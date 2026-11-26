//! The planar copy of a render: whole blocks onto the planes a buffer holds.

use crate::Result;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{BufferError, Interleaved, Sample};

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
