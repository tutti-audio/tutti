//! Spatial panners.
//!
//! - [`VbapPannerNode`]: a stereo-in loudspeaker panner over a `Bed`.
//! - [`HrtfBinauralNode`]: the binaural renderer.
//!   Its error, `tutti_dsp::HrtfBinauralError`, and the sphere rates
//!   (`MIN_SPHERE_RATE`, `MAX_SPHERE_RATE`, `MAX_PREPARED_RATE`) are
//!   `tutti-dsp`'s.
//!
//! - [`AmbisonicEncoderNode`], [`AmbisonicRotatorNode`] and [`AmbisonicDecoderNode`]: ambisonics in
//!   the AmbiX convention.
//!
//! A mix of several panned sources is plain fan-in: wire each panner's
//! outputs to the same graph ports and the graph sums them.

mod ambisonic;
mod hrtf;
mod target;
mod vbap;

pub use ambisonic::{
    AmbisonicDecoderConfig, AmbisonicDecoderKernel, AmbisonicDecoderNode, AmbisonicDecoderSet,
    AmbisonicEncoderConfig, AmbisonicEncoderKernel, AmbisonicEncoderNode, AmbisonicRotatorConfig,
    AmbisonicRotatorKernel, AmbisonicRotatorNode, Decoding, DirectionSet,
};
pub use hrtf::{HrtfBinauralConfig, HrtfBinauralNode};
pub use vbap::{VbapPannerConfig, VbapPannerNode};
