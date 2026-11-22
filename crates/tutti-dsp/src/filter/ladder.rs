//! The Moog ladder: its model, its kernel (coefficients and step), and [`LadderBank`], one
//! ladder per channel over buffers views.

mod bank;
mod kernel;
mod model;
mod solve;

pub use bank::{LadderBank, LadderBlend};
pub use kernel::{LadderCoefficients, LadderDesign, LadderType, ladder_step};
pub use model::{LadderControls, LadderModel};
