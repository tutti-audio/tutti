//! The short-time Fourier transform's vocabulary: the cosine windows, the frame and
//! hop geometry, the frame-by-bin grid, and the errors the geometry and the grid
//! refuse with. The [`PhaseVocoder`](crate::PhaseVocoder) is built on it and the analysers
//! (`tutti-measure`) share it.

pub(crate) mod error;
pub(crate) mod geometry;
pub(crate) mod grid;
pub(crate) mod vocoder;
pub(crate) mod window;
