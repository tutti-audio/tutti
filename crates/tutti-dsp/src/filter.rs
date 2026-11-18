//! Filter algorithms: state-variable, Moog ladder and the Linkwitz-Riley crossover, each a
//! bank of one filter per channel over buffers views ([`SvfBank`], [`LadderBank`],
//! [`CrossoverBank`]), over the [`Lanes`] arithmetic their steps share.

/// The share of Nyquist a cutoff is held under, where `tan` of its warp is finite and the
/// bilinear map is well conditioned.
pub(crate) const NYQUIST_MARGIN: f32 = 0.998;

mod biquad;
mod crossover;
mod envelope;
mod ladder;
mod lanes;
mod svf;

pub(crate) use biquad::section_l1;
pub use biquad::{Biquad, DirectForm1};
pub use crossover::{
    CrossoverBank, CrossoverCoefficients, CrossoverControls, CrossoverError, CrossoverModel,
    MAX_SPLITS, SOLVE_CEILING, fit_to_rate,
};
pub use envelope::{EnvelopeFilterBank, EnvelopeFilterControls};
pub use ladder::{
    LadderBank, LadderCoefficients, LadderCoeffs, LadderControls, LadderDesign, LadderModel,
    LadderType, ladder_step,
};
pub use lanes::Lanes;
pub use svf::{SvfBank, SvfCoefficients, SvfCoeffs, SvfControls, SvfModel, SvfType, svf_step};
pub(crate) use svf::{integrator_gains, svf_taps};
