//! [`VbapError`] and the module-local [`Result`] alias.
//!
//! Scoped to `vbap`. The binaural side owns `HrtfBinauralError`; there is no
//! crate-level error type to unify them, and adding one would claim a shared
//! failure mode that does not exist.

use thiserror::Error;
use tutti_dsp::SpeakerGeometryError;

/// The error returned when a VBAP panner cannot be built.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum VbapError {
    /// The bed's speakers cannot be panned over: `Bed::Mono` has one.
    #[error("the bed's speakers cannot be panned over: {0}")]
    Geometry(#[from] SpeakerGeometryError),
}

/// `Result` with [`VbapError`] as the failure type.
pub type Result<T> = core::result::Result<T, VbapError>;
