//! Vector Base Amplitude Panning: per-speaker gains from a bearing and height,
//! so a source lands between the speakers nearest its direction.
//!
//! - [`VbapPannerNode`] pans one stereo (or mono) source across a bed's
//!   speakers; its params move it while it renders.
//! - [`VbapError`] is what building a panner can fail with.
//!
//! A VBAP panner is parameterized by a [`Bed`](tutti_buffers::Bed): its output
//! width is the bed's and its speakers sit where the bed's one table puts them.
//! The binaural renderer (`HrtfBinauralNode`) is parameterized by an HRIR
//! dataset instead, always has two outputs and has its own error type.

mod error;
mod node;

pub use error::VbapError;
pub(super) use node::speaker_of_channel;
pub use node::{VbapPannerConfig, VbapPannerNode};
