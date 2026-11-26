//! The sample widths a PCM file holds, and the one conversion every encoder
//! shares.
//!
//! An encoder takes blocks in any [`Sample`] format and converts each value to
//! the width the file holds by the rule in [`Sample`]: scaled by `2^(n-1)`,
//! rounded half to even, clipped. Every container converts through here, so an
//! exported WAV, AIFF and FLAC agree on a sample.
//!
//! ```
//! use tutti_buffers::FloatWidth;
//! use tutti_codec::{IntDepth, Pcm};
//!
//! assert_eq!(IntDepth::Int16.quantize(1.0f32), 32_767); // clips
//! assert_eq!(IntDepth::Int16.quantize(-1.0f32), -32_768);
//! assert_eq!(IntDepth::Int24.quantize(-2.0f32), -8_388_608); // clips
//! assert_eq!(Pcm::default().bits(), 24);
//! assert_eq!(Pcm::Float(FloatWidth::F64).bits(), 64);
//! ```

use tutti_buffers::{FloatWidth, I24, Sample};
use tutti_dsp::DitherShape;
use tutti_units::Amplitude;

/// An integer sample width: the depths FLAC holds, and the depth of an [`IntPcm`].
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
    ///
    /// # Panics
    ///
    /// Never: both integer formats define a step.
    #[must_use]
    pub fn lsb(self) -> Amplitude {
        let step = match self {
            Self::Int16 => i16::STEP,
            Self::Int24 => I24::STEP,
        };
        Amplitude::new(step.expect("an integer format has a step"))
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

/// An integer PCM file's sample type: its depth, and how a block that has more resolution than
/// the depth is brought down to it.
///
/// A block is rounded to the depth with triangular dither ([`DitherShape::Triangular`]) and no
/// noise shaping, unless the options say otherwise. A float file ([`Pcm::Float`]) has no such
/// options: its samples are written as they are, so a dither on one cannot be written.
///
/// ```
/// use tutti_codec::{IntDepth, IntPcm};
/// use tutti_dsp::DitherShape;
///
/// let cd = IntPcm::new(IntDepth::Int16).dither(DitherShape::Rectangular).noise_shaping();
/// assert_eq!(cd.depth(), IntDepth::Int16);
/// assert_eq!(cd.dither_shape(), Some(DitherShape::Rectangular));
/// assert!(cd.is_noise_shaped());
/// assert_eq!(IntPcm::new(IntDepth::Int24).no_dither().dither_shape(), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IntPcm {
    depth: IntDepth,
    dither: Option<DitherShape>,
    noise_shaping: bool,
}

impl Default for IntPcm {
    fn default() -> Self {
        Self::new(IntDepth::default())
    }
}

impl IntPcm {
    /// An integer file of `depth`, dithered with triangular noise and not noise-shaped.
    #[must_use]
    pub const fn new(depth: IntDepth) -> Self {
        Self {
            depth,
            dither: Some(DitherShape::Triangular),
            noise_shaping: false,
        }
    }

    /// Dithers with noise of `shape`.
    #[must_use]
    pub const fn dither(self, shape: DitherShape) -> Self {
        Self {
            dither: Some(shape),
            ..self
        }
    }

    /// Rounds without dither: a block whose floats already sit on the file's grid is written
    /// bit for bit, and any other is rounded to the nearest step.
    #[must_use]
    pub const fn no_dither(self) -> Self {
        Self {
            dither: None,
            ..self
        }
    }

    /// Runs the quantizer as error feedback through a [`NoiseShaping`](tutti_dsp::NoiseShaping)
    /// designed for the file's rate, which moves the quantization noise to where the ear is
    /// least sensitive.
    #[must_use]
    pub const fn noise_shaping(self) -> Self {
        Self {
            noise_shaping: true,
            ..self
        }
    }

    /// The depth.
    #[must_use]
    pub const fn depth(self) -> IntDepth {
        self.depth
    }

    /// The noise's shape, or `None` when the file is rounded without dither.
    #[must_use]
    pub const fn dither_shape(self) -> Option<DitherShape> {
        self.dither
    }

    /// Whether the quantizer is run through a noise-shaping filter.
    #[must_use]
    pub const fn is_noise_shaped(self) -> bool {
        self.noise_shaping
    }
}

/// The sample type a PCM file holds.
///
/// `Int` at 24 bits with triangular dither is the default, because that is what a file export
/// wants; a live capture path typically picks a float width explicitly (no quantization, no
/// clipping to worry about mid-take).
///
/// Deliberately not `#[non_exhaustive]`: encoders match it, and a `_` arm is
/// what must not exist there. A new width an encoder silently ignored would
/// write a file whose header and data disagree, so adding one fails to compile
/// everywhere it is handled.
///
/// The dither is an option of the integer type, so a float file cannot be given one:
///
/// ```compile_fail,E0599
/// use tutti_buffers::FloatWidth;
/// use tutti_codec::Pcm;
/// use tutti_dsp::DitherShape;
///
/// let _ = Pcm::Float(FloatWidth::F32).dither(DitherShape::Triangular);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pcm {
    /// An integer width, rounded to with the dither of its [`IntPcm`].
    Int(IntPcm),
    /// A float width, written unquantized: no dither and no clipping decision.
    Float(FloatWidth),
}

impl Default for Pcm {
    fn default() -> Self {
        Self::Int(IntPcm::default())
    }
}

impl Pcm {
    /// The bits per sample, as written to a file header.
    #[must_use]
    pub const fn bits(self) -> u16 {
        match self {
            Self::Int(pcm) => pcm.depth().bits(),
            Self::Float(FloatWidth::F32) => 32,
            Self::Float(FloatWidth::F64) => 64,
        }
    }

    /// The integer type, or `None` for a float width, which does not quantize.
    #[must_use]
    pub const fn int_pcm(self) -> Option<IntPcm> {
        match self {
            Self::Int(pcm) => Some(pcm),
            Self::Float(_) => None,
        }
    }
}

#[cfg(test)]
mod tests;
