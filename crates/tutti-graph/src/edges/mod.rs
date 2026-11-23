//! Where audio and events cross the graph's boundary: taps copy out of it,
//! inlets copy into it, meters read levels from it, and the real-time log
//! carries a node's records off the audio thread.

pub(crate) mod alive;
pub(crate) mod event_inlet;
pub(crate) mod event_tap;
pub(crate) mod inlet;
pub(crate) mod meter;
pub(crate) mod rtlog;
pub(crate) mod tap;
