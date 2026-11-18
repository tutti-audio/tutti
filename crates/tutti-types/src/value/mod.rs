//! Value vocabulary: what a parameter is and how it is carried, over the unit
//! newtypes of `tutti_units`.
//!
//! - [`param`]: [`Param`], a lock-free atomic cell holding a `Unit` value,
//!   shareable with the audio thread (load / store / handle), and
//!   [`Param64`], its `f64` neighbour.
//! - [`atomic_rate`]: [`AtomicSampleRate`] and [`ParkedSampleRate`], a
//!   `SampleRate` shared with another thread as one atomic word.
//! - [`max_block`]: [`MaxBlock`], the largest block a node is asked to render.

pub mod atomic_rate;
pub mod max_block;
pub mod param;

pub use atomic_rate::{AtomicSampleRate, ParkedSampleRate};
pub use max_block::{InvalidMaxBlock, MaxBlock};
pub use param::{Param, Param64, ParamCell, ParamRaw};
