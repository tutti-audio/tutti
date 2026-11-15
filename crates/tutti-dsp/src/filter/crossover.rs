//! The Linkwitz-Riley crossover: its model (the band tree's coefficients), its controls,
//! and [`CrossoverBank`], the band tree per channel over buffers views.

mod bands;
mod bank;
mod controls;
mod model;
mod state;

pub use bands::{CrossoverError, MAX_SPLITS, SOLVE_CEILING, fit_to_rate};
pub use bank::CrossoverBank;
pub use controls::CrossoverControls;
pub use model::CrossoverModel;
pub(crate) use state::CrossoverState;
