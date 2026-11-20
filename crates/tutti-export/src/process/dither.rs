//! The dither a file target calls for; the stage that adds it and converts is
//! `tutti_dsp`'s [`DitherSink`](tutti_dsp::DitherSink).

use crate::output::FileTarget;
use tutti_dsp::DitherShape;

/// The dither `target` calls for, or `None` when nothing quantizes.
///
/// A format with no integer width never dithers: dither decorrelates
/// *quantization* error, and a float output does not quantize.
pub(crate) fn dither_shape(target: &FileTarget) -> Option<DitherShape> {
    target.format().int_depth()?;
    target.dither
}

#[cfg(test)]
mod tests;
