//! The level below which input is treated as silence.

use tutti_units::Db;

/// The peak level below which input is silence: spectral flux, pitch peaks and
/// onsets of a buffer this quiet are rounding noise, so tempo, chroma and note
/// transcription read it as empty.
pub(crate) const SILENCE_FLOOR: Db = Db(-90.0);
