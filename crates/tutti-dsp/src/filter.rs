//! Filter algorithms: state-variable, Moog ladder and the Linkwitz-Riley crossover, each a
//! bank of one filter per channel over buffers views ([`SvfBank`], [`LadderBank`],
//! [`CrossoverBank`]), over the [`Lanes`] arithmetic their steps share.

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
