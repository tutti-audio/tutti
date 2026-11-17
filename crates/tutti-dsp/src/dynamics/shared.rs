//! What the compressor, gate and limiter banks share: the chunk a pass over a block takes,
//! the ramp length of a gain that moves, and the key's level.

use std::ops::Range;

use tutti_buffers::{Float, Planes};
use tutti_types::Seconds;

/// The frames a pass over a block takes at a time: the levels of a stretch become its gains
/// in a stack array.
pub(super) const CHUNK: usize = 64;

/// How long a makeup (or a gate's range) that moves takes to arrive: 5 ms, counted on the
/// stream, so the de-zipper does not depend on the block.
pub const GAIN_GLIDE: Seconds = Seconds(0.005);

/// `x`'s magnitude as a level: a NaN reads as quiet (0) and an infinity as the largest
/// finite level, so a detector never sees a value its dB conversion turns into an infinity.
#[inline]
pub(crate) fn level(x: f32) -> f32 {
    let a = x.abs();
    if a.is_nan() { 0.0 } else { a.min(f32::MAX) }
}

/// The key's level at each frame of `frames`: the loudest channel of `key`, into `out`
/// (as long as `frames`).
#[inline]
pub(crate) fn key_levels<S: Float>(key: &Planes<'_, S>, frames: Range<usize>, out: &mut [f32]) {
    out.fill(0.0);
    for plane in key.iter() {
        for (m, x) in out.iter_mut().zip(&plane.samples()[frames.clone()]) {
            *m = m.max(level(x.convert_to::<f32>()));
        }
    }
}
