//! The control rate: [`COEFF_INTERVAL`], the grid a moving parameter's coefficients are solved on.

/// Samples between coefficient solves on a parameter that moves inside a block.
///
/// A swept filter solves its coefficients (a `tan` each) on a grid of
/// `COEFF_INTERVAL` frames of the stream, not from a block's start, and
/// interpolates them linearly between solves; a control is therefore in the
/// render up to `COEFF_INTERVAL` frames after it was sampled, wherever the blocks
/// fall. 16 samples is 0.33 ms at 48 kHz.
pub const COEFF_INTERVAL: usize = 16;
