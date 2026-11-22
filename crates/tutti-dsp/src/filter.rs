//! Filter algorithms: state-variable, Moog ladder and the Linkwitz-Riley crossover, each a
//! bank of one filter per channel over buffers views ([`SvfBank`], [`LadderBank`],
//! [`CrossoverBank`]), over the [`SimdLanes`] arithmetic their steps share.

/// The share of Nyquist a cutoff is held under, where `tan` of its warp is finite and the
/// bilinear map is well conditioned.
pub(crate) const NYQUIST_MARGIN: f64 = 0.998;

mod biquad;
mod crossover;
mod envelope;
mod ladder;
mod simd_lanes;
mod svf;

pub use biquad::Biquad;
pub(crate) use biquad::{decay_time, section_l1};
pub use crossover::{
    CrossoverBands, CrossoverBandsError, CrossoverBank, CrossoverBlend, CrossoverControls,
    CrossoverError, CrossoverModel, MAX_SPLITS, SOLVE_CEILING, fit_to_rate,
};
pub use envelope::{EnvelopeFilterBank, EnvelopeFilterControls};
pub use ladder::{
    LadderBank, LadderBlend, LadderCoefficients, LadderControls, LadderDesign, LadderModel,
    LadderType, ladder_step,
};
pub use simd_lanes::SimdLanes;
pub use svf::{
    SvfBank, SvfCoefficients, SvfControls, SvfDesign, SvfDesignError, SvfModel, SvfType, svf_step,
};
pub(crate) use svf::{integrator_gains, svf_taps};
