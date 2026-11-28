//! FFT-based convolution reverb.
//!
//! A partitioned FFT convolution ([`Convolver`](tutti_dsp::Convolver), over an IR's shared
//! [`IrSpectra`](tutti_dsp::IrSpectra): uniform partitions for a short IR, a head and larger tail
//! partitions for a long one; a [`MatrixConvolver`](tutti_dsp::MatrixConvolver) of those for
//! true stereo and ambisonic responses) as a graph node ([`ConvolverNode`], its
//! responses laid out by an [`IrLayout`]), so a
//! convolution reverb lives in a graph alongside the built-in delay,
//! modulation, dynamics, and spatial nodes.
//!
//! IR samples are supplied by the caller — usually via `tutti-sampler`,
//! which already owns WAV / FLAC / MP3 / OGG loading. This module is
//! deliberately format-agnostic.

mod layout;
mod node;
mod responses;

#[cfg(test)]
mod fir_convolver_tests;
#[cfg(test)]
mod test_nodes;

pub use layout::IrLayout;
pub use node::{ConvolverConfig, ConvolverError, ConvolverNode, FirConvolverConfig};
pub use responses::ImpulseResponses;
