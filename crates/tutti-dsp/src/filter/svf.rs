//! The state-variable filter: its model (controls to coefficients and response), its kernel
//! (the coefficients the step reads, and the step), and [`SvfBank`], one filter per channel
//! over buffers views.

mod bank;
mod kernel;
mod model;

pub use bank::SvfBank;
pub use kernel::{SvfCoefficients, SvfCoeffs, SvfType, svf_step};
pub(crate) use kernel::{integrator_gains, svf_taps};
pub use model::{SvfControls, SvfModel};
