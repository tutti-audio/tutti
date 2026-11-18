//! The conversions between formats and between layouts, as functions over two
//! views.
//!
//! Each takes both views whole and refuses a mismatch before writing: the
//! widths must agree, then the frame counts. A source and a destination of
//! different formats convert sample by sample by the rule in [`Sample`].

use crate::{BufferError, Interleaved, InterleavedMut, Mono, MonoMut, Planes, PlanesMut, Sample};
use tutti_types::{ChannelLayout, Samples};

/// Converts every sample of `src` into `dst`'s format by the rule in
/// [`Sample`].
///
/// ```
/// use tutti_buffers::{convert, Interleaved, InterleavedMut};
/// use tutti_types::ChannelLayout;
///
/// let src = [0.5f32, -1.0, 2.0, f32::NAN];
/// let mut dst = [0i16; 4];
/// convert(
///     Interleaved::new(&src, ChannelLayout::STEREO)?,
///     InterleavedMut::new(&mut dst, ChannelLayout::STEREO)?,
/// )?;
/// assert_eq!(dst, [16384, -32768, 32767, 0]);
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
///
/// # Errors
///
/// [`BufferError::WidthMismatch`] if the two widths differ and
/// [`BufferError::LengthMismatch`] if the frame counts differ. Nothing is
/// written then.
pub fn convert<A: Sample, B: Sample>(
    src: Interleaved<'_, A>,
    dst: InterleavedMut<'_, B>,
) -> Result<(), BufferError> {
    convert_counting_clips(src, dst).map(drop)
}

/// Converts as [`convert`] does and returns how many values clipped: a float
/// or a wider integer whose rounded value fell outside the integer target's
/// range and was held at its `MIN` or `MAX`. A NaN reads as `0` and is not a
/// clip; into a float format nothing clips, so the count is `0`.
///
/// ```
/// use tutti_buffers::{convert_counting_clips, Interleaved, InterleavedMut};
/// use tutti_types::ChannelLayout;
///
/// let src = [1.0f32, 32767.0 / 32768.0, -1.0, f32::NAN];
/// let mut dst = [0i16; 4];
/// let clipped = convert_counting_clips(
///     Interleaved::new(&src, ChannelLayout::STEREO)?,
///     InterleavedMut::new(&mut dst, ChannelLayout::STEREO)?,
/// )?;
/// assert_eq!(clipped, 1); // only +1.0: -1.0 is `MIN` exactly
/// assert_eq!(dst, [32767, 32767, -32768, 0]);
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
///
/// # Errors
///
/// The same as [`convert`]: [`BufferError::WidthMismatch`], then
/// [`BufferError::LengthMismatch`], with nothing written.
pub fn convert_counting_clips<A: Sample, B: Sample>(
    src: Interleaved<'_, A>,
    mut dst: InterleavedMut<'_, B>,
) -> Result<usize, BufferError> {
    same_shape(src.layout(), dst.layout(), src.frames(), dst.frames())?;
    let mut clipped = 0;
    for (d, &s) in dst.samples_mut().iter_mut().zip(src.samples()) {
        let (v, clip) = s.convert_to_clipped();
        *d = v;
        clipped += usize::from(clip);
    }
    Ok(clipped)
}

/// Converts one channel of `src` into `dst`'s format by the rule in
/// [`Sample`].
///
/// ```
/// use tutti_buffers::{convert_mono, Mono, MonoMut};
///
/// let src = [0.5f32, -1.0, 2.0, f32::NAN];
/// let mut dst = [0i16; 4];
/// convert_mono(Mono::new(&src), MonoMut::new(&mut dst))?;
/// assert_eq!(dst, [16384, -32768, 32767, 0]);
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
///
/// # Errors
///
/// [`BufferError::LengthMismatch`] if the frame counts differ. Nothing is
/// written then.
pub fn convert_mono<A: Sample, B: Sample>(
    src: Mono<'_, A>,
    mut dst: MonoMut<'_, B>,
) -> Result<(), BufferError> {
    same_frames(src.frames(), dst.frames())?;
    for (d, &s) in dst.samples_mut().iter_mut().zip(src.samples()) {
        *d = s.convert_to();
    }
    Ok(())
}

/// Converts every sample of `src`'s planes into `dst`'s format by the rule in
/// [`Sample`]: plane `c` of `dst` is plane `c` of `src`.
///
/// # Errors
///
/// The same as [`convert`].
pub fn convert_planes<A: Sample, B: Sample>(
    src: Planes<'_, A>,
    mut dst: PlanesMut<'_, '_, B>,
) -> Result<(), BufferError> {
    same_shape(src.layout(), dst.layout(), src.frames(), dst.frames())?;
    for (from, mut to) in src.iter().zip(dst.iter_mut()) {
        for (d, &s) in to.samples_mut().iter_mut().zip(from.samples()) {
            *d = s.convert_to();
        }
    }
    Ok(())
}

/// Interleaves `src`'s planes into `dst`, converting to `dst`'s format: frame
/// `i`, channel `c` of `dst` is `src.plane(c)[i]`.
///
/// # Errors
///
/// The same as [`convert`].
pub fn interleave<A: Sample, B: Sample>(
    src: Planes<'_, A>,
    mut dst: InterleavedMut<'_, B>,
) -> Result<(), BufferError> {
    same_shape(src.layout(), dst.layout(), src.frames(), dst.frames())?;
    let stride = dst.channels();
    for (c, plane) in src.iter().enumerate() {
        let out = dst.samples_mut().iter_mut().skip(c).step_by(stride);
        for (d, &s) in out.zip(plane.samples()) {
            *d = s.convert_to();
        }
    }
    Ok(())
}

/// Deinterleaves `src` into `dst`'s planes, converting to `dst`'s format:
/// `dst.plane(c)[i]` is frame `i`, channel `c` of `src`.
///
/// # Errors
///
/// The same as [`convert`].
pub fn deinterleave<A: Sample, B: Sample>(
    src: Interleaved<'_, A>,
    mut dst: PlanesMut<'_, '_, B>,
) -> Result<(), BufferError> {
    same_shape(src.layout(), dst.layout(), src.frames(), dst.frames())?;
    let stride = src.channels();
    for (c, mut plane) in dst.iter_mut().enumerate() {
        let from = src.samples().iter().skip(c).step_by(stride);
        for (d, &s) in plane.samples_mut().iter_mut().zip(from) {
            *d = s.convert_to();
        }
    }
    Ok(())
}

/// Refuses a width or frame-count mismatch, width first.
pub(crate) fn same_shape(
    src_layout: ChannelLayout,
    dst_layout: ChannelLayout,
    src_frames: Samples,
    dst_frames: Samples,
) -> Result<(), BufferError> {
    if src_layout != dst_layout {
        return Err(BufferError::WidthMismatch {
            src: src_layout,
            dst: dst_layout,
        });
    }
    same_frames(src_frames, dst_frames)
}

/// Refuses a frame-count mismatch.
pub(crate) fn same_frames(src: Samples, dst: Samples) -> Result<(), BufferError> {
    if src == dst {
        Ok(())
    } else {
        Err(BufferError::LengthMismatch { src, dst })
    }
}
