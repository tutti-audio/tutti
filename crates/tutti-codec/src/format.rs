//! The container to write, carrying only the settings it accepts.

use crate::error::{EncodeError, Result};
use crate::pcm::IntPcm;
#[cfg(any(feature = "encode-wav", feature = "encode-aiff"))]
use crate::pcm::Pcm;
use std::path::Path;
use tutti_buffers::ChannelLayout;

/// The container to write, carrying that format's own settings.
///
/// The settings live in the variant, so a combination a format cannot write is
/// not expressible: FLAC holds integer widths only (`pcm` is an
/// [`IntPcm`]).
///
/// Each variant exists only with its cargo feature (`encode-wav`, `encode-aiff`, `encode-flac`), so a format this build cannot write cannot be named.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Format {
    /// Uncompressed RIFF/WAVE at any [`Pcm`] width.
    #[cfg(feature = "encode-wav")]
    Wav(Pcm),
    /// AIFF at an integer width; AIFF-C at a float width, since float samples are
    /// an AIFF-C extension.
    #[cfg(feature = "encode-aiff")]
    Aiff(Pcm),
    /// Lossless FLAC at an integer width.
    #[cfg(feature = "encode-flac")]
    Flac {
        /// The integer sample type: the depth and how a block is brought down to it.
        pcm: IntPcm,
        /// The encoder effort.
        level: FlacLevel,
    },
}

impl Format {
    /// The format a file extension names, with that format's defaults. Case
    /// does not matter, and `aif` aliases `aiff`.
    ///
    /// # Errors
    ///
    /// [`EncodeError::UnknownExtension`] if the extension is not one of `wav`,
    /// `flac`, `aiff` or `aif`, or names a format this build lacks.
    pub fn from_extension(ext: &str) -> Result<Self> {
        let ext = ext.to_lowercase();
        match ext.as_str() {
            #[cfg(feature = "encode-wav")]
            "wav" => Ok(Self::Wav(Pcm::default())),
            #[cfg(feature = "encode-flac")]
            "flac" => Ok(Self::Flac {
                pcm: IntPcm::default(),
                level: FlacLevel::default(),
            }),
            #[cfg(feature = "encode-aiff")]
            "aiff" | "aif" => Ok(Self::Aiff(Pcm::default())),
            _ => Err(EncodeError::UnknownExtension {
                extension: if ext.is_empty() { "(none)".into() } else { ext },
            }),
        }
    }

    /// The format a file path's extension names, with that format's defaults.
    /// See [`from_extension`](Self::from_extension).
    ///
    /// # Errors
    ///
    /// [`EncodeError::UnknownExtension`] if the extension is unsupported or missing.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_extension(
            path.as_ref()
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or(""),
        )
    }

    /// The integer type this format quantizes to, or `None` when it does not
    /// quantize (a float width).
    #[must_use]
    pub fn int_pcm(&self) -> Option<IntPcm> {
        match *self {
            #[cfg(feature = "encode-wav")]
            Self::Wav(pcm) => pcm.int_pcm(),
            #[cfg(feature = "encode-aiff")]
            Self::Aiff(pcm) => pcm.int_pcm(),
            #[cfg(feature = "encode-flac")]
            Self::Flac { pcm, .. } => Some(pcm),
        }
    }

    /// The most whole frames a file of `layout` in this format holds, or
    /// `None` when its size is not bounded by a header field. Only WAV is: its
    /// sizes are 32-bit.
    #[must_use]
    #[cfg_attr(not(feature = "encode-wav"), expect(unused_variables))]
    pub fn max_frames(&self, layout: ChannelLayout) -> Option<u64> {
        match *self {
            #[cfg(feature = "encode-wav")]
            Self::Wav(pcm) => Some(crate::wav::max_data_frames(layout, pcm)),
            #[cfg(feature = "encode-aiff")]
            Self::Aiff(_) => None,
            #[cfg(feature = "encode-flac")]
            Self::Flac { .. } => None,
        }
    }
}

/// How hard the FLAC encoder works. Every level is lossless.
///
/// Modelled loosely on libFLAC's levels 0 to 8, but only these five encode
/// differently: libFLAC's levels 1 and 2, 4 to 6, and 7 and 8 coincide here.
/// The block size is 4096 at every level. All but [`Fastest`](Self::Fastest)
/// try the side and mid/side stereo modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FlacLevel {
    /// Fixed predictors only, left/right stereo only (libFLAC level 0).
    Fastest,
    /// Fixed predictors, with stereo decorrelation (levels 1 and 2).
    Fast,
    /// Adds linear prediction at order 6 (level 3).
    Medium,
    /// Linear prediction at order 8 (levels 4 to 6). The default.
    #[default]
    Standard,
    /// Linear prediction at order 12 (levels 7 and 8).
    Best,
}

#[cfg(test)]
mod tests;
