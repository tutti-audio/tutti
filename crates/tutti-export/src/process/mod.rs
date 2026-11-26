//! Signal processing between the render and the encoder.
//!
//! One stage, and it streams: resample (`tutti_dsp::StreamingResampler`), an FFT block
//! resampler on the scalar `RealFft` that never needs the whole signal. The dither is the
//! encoder's.
//!
//! There is deliberately no "mastering" type and no whole-signal pass.
//! Normalization is the one step that genuinely needs two passes (measure, then
//! apply), and it is not a stage here: [`crate::normalize`] measures a spill of
//! the render and the write pass applies the `Db` it chooses. Keeping that out
//! is precisely what lets every other output stream.

pub use tutti_dsp::ChunkSize;
