//! Spatial panners.
//!
//! - [`VbapPannerNode`]: a stereo-in loudspeaker panner over a `Bed`.
//! - [`VbapError`]: what building a VBAP panner can fail with.
//! - [`HrtfBinauralNode`]: the binaural renderer.
//!   Its error, `tutti_dsp::HrtfBinauralError`, and the sphere rates
//!   (`MIN_SPHERE_RATE`, `MAX_SPHERE_RATE`, `MAX_PREPARED_RATE`) are
//!   `tutti-dsp`'s.
//!
//! - [`AmbisonicEncoderNode`] and [`AmbisonicRotatorNode`]: ambisonics in
//!   the AmbiX convention.
//!
//! A mix of several panned sources is plain fan-in: wire each panner's
//! outputs to the same graph ports and the graph sums them.

mod ambisonic;
mod hrtf;
mod target;
mod vbap;

pub use ambisonic::{
    AmbisonicEncoderConfig, AmbisonicEncoderKernel, AmbisonicEncoderNode, AmbisonicError,
    AmbisonicRotatorConfig, AmbisonicRotatorKernel, AmbisonicRotatorNode, DirectionSet,
};
pub use hrtf::{HrtfBinauralConfig, HrtfBinauralNode};
pub use vbap::{VbapError, VbapPannerConfig, VbapPannerNode};
