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
    /// The bus places no bed: VBAP pans over a bed's speakers, so a bus of unassigned
    /// channels, an ambisonic one or any other placement has none to pan between.
    #[error("the bus is not a bed, so it has no speakers to pan between")]
    NotABed,
    /// The bed's speakers cannot be panned over: `Bed::Mono` has one.
    #[error("the bed's speakers cannot be panned over: {0}")]
    Geometry(#[from] SpeakerGeometryError),
}

/// `Result` with [`VbapError`] as the failure type.
pub(super) type Result<T> = core::result::Result<T, VbapError>;
