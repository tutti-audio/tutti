//! The sample widths a PCM file holds, and the one conversion every encoder
//! shares.
//!
//! An encoder takes blocks in any [`Sample`] format and converts each value to
//! the width the file holds by the rule in [`Sample`]: scaled by `2^(n-1)`,
//! rounded half to even, clipped. Every container converts through here, so an
//! exported WAV, AIFF and FLAC agree on a sample.
//!
//! ```
//! use tutti_codec::{IntDepth, Pcm};
//!
//! assert_eq!(IntDepth::Int16.quantize(1.0f32), 32_767); // clips
//! assert_eq!(IntDepth::Int16.quantize(-1.0f32), -32_768);
//! assert_eq!(IntDepth::Int24.quantize(-2.0f32), -8_388_608); // clips
//! assert_eq!(Pcm::default().bits(), 24);
//! assert_eq!(Pcm::Float64.bits(), 64);
//! ```

use tutti_buffers::{I24, Sample};
use tutti_types::Amplitude;

/// An integer sample width: the depths FLAC holds, and the integer half of
/// [`Pcm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IntDepth {
    /// 16-bit signed integer, CD depth.
    Int16,
    /// 24-bit signed integer, packed three bytes per sample. The default.
    #[default]
    Int24,
}

impl IntDepth {
    /// The bits per sample, as written to a file header.
    #[must_use]
    pub const fn bits(self) -> u16 {
        match self {
            Self::Int16 => 16,
            Self::Int24 => 24,
        }
    }

    /// One least significant bit in the `[-1, 1]` float domain: the format's
    /// [`Sample::STEP`]. The size dither noise is scaled to before this depth
    /// quantizes.
    #[must_use]
    pub fn lsb(self) -> Amplitude {
        let step = match self {
            Self::Int16 => i16::STEP,
            Self::Int24 => I24::STEP,
        };
        step.map_or(Amplitude::SILENT, |step| Amplitude(step as f32))
    }

    /// One sample as this depth's integer, by the rule in [`Sample`]: scaled
    /// by `2^(n-1)`, rounded half to even and clipped.
    #[inline]
    #[must_use]
    pub fn quantize<S: Sample>(self, sample: S) -> i32 {
        match self {
            Self::Int16 => i32::from(sample.convert_to::<i16>()),
            Self::Int24 => sample.convert_to::<I24>().get(),
        }
    }
}

/// The sample width a PCM file holds.
///
/// `Int24` is the default because that is what a file export wants; a live
/// capture path typically picks a float width explicitly (no quantization, no
/// clipping to worry about mid-take).
///
/// Deliberately not `#[non_exhaustive]`: encoders match it, and a `_` arm is
/// what must not exist there. A new width an encoder silently ignored would
/// write a file whose header and data disagree, so adding one fails to compile
/// everywhere it is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Pcm {
    /// 16-bit signed integer, CD depth.
    Int16,
    /// 24-bit signed integer, packed three bytes per sample. The default.
    #[default]
    Int24,
    /// 32-bit float, written unquantized: no dither and no clipping decision.
    Float32,
    /// 64-bit float, written unquantized.
    Float64,
}

impl From<IntDepth> for Pcm {
    fn from(depth: IntDepth) -> Self {
        match depth {
            IntDepth::Int16 => Self::Int16,
            IntDepth::Int24 => Self::Int24,
        }
    }
}

impl Pcm {
    /// The bits per sample, as written to a file header.
    #[must_use]
    pub const fn bits(self) -> u16 {
        match self {
            Self::Int16 => 16,
            Self::Int24 => 24,
            Self::Float32 => 32,
            Self::Float64 => 64,
        }
    }

    /// The integer width, or `None` for a float width, which does not quantize:
    /// dithering it is a no-op and clipping is the caller's concern.
    #[must_use]
    pub const fn int_depth(self) -> Option<IntDepth> {
        match self {
            Self::Int16 => Some(IntDepth::Int16),
            Self::Int24 => Some(IntDepth::Int24),
            Self::Float32 | Self::Float64 => None,
        }
    }
}

#[cfg(test)]
mod tests;
