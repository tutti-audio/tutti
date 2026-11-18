//! The real-input FFT and its working set.
//!
//! [`RealFft`] runs on rustfft's scalar planner, so its arithmetic is the same
//! on every CPU; [`StagedRealFft`] cuts one transform into steps a caller can
//! spread over several host blocks; [`FftScratch`] is the reusable plan and
//! buffers the analysis kernels thread through.

mod correlator;
mod real;
mod scratch;
mod staged;

pub use correlator::Correlator;
pub use real::{Complex, Complex32, Complex64, RealFft};
pub use scratch::FftScratch;
pub(crate) use staged::StagedRealFft;
