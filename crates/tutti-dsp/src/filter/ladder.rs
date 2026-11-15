//! The Moog ladder: its model, its kernel (coefficients and step), and [`LadderBank`], one
//! ladder per channel over buffers views.

mod bank;
mod kernel;
mod model;

pub use bank::{LadderBank, LadderDesign};
pub use kernel::{LadderCoefficients, LadderCoeffs, LadderType, ladder_step};
pub use model::{LadderControls, LadderModel};
