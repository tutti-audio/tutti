//! The parts of a [`Session`](super::Session): each owns one subsystem's bookkeeping and the
//! methods that use it. A part of a feature comes from the module the feature picks: the real
//! one when the feature is on, and a stub with the signatures the feature-independent code calls, doing nothing,
//! when it is off. `Session`'s fields and phases name the part and never the feature.

mod output;
mod pool;
mod reports;
mod transport;

#[cfg(feature = "export")]
pub(crate) use crate::bounce::BounceState as BouncePart;
#[cfg(not(feature = "export"))]
#[path = "bounce_none.rs"]
mod bounce;
#[cfg(feature = "capture")]
pub(crate) use crate::input::Inputs;
#[cfg(not(feature = "capture"))]
#[path = "capture_none.rs"]
mod capture;
#[cfg(feature = "device")]
mod device;
#[cfg(not(feature = "device"))]
#[path = "device_none.rs"]
mod device;
#[cfg(feature = "midi")]
mod midi;
#[cfg(not(feature = "midi"))]
#[path = "midi_none.rs"]
mod midi;
#[cfg(feature = "plugin")]
mod plugin;
#[cfg(not(feature = "plugin"))]
#[path = "plugin_none.rs"]
mod plugin;
#[cfg(feature = "sampler")]
mod sampler;
#[cfg(not(feature = "sampler"))]
#[path = "sampler_none.rs"]
mod sampler;

#[cfg(not(feature = "export"))]
pub(crate) use bounce::BouncePart;
#[cfg(not(feature = "capture"))]
pub(crate) use capture::Inputs;
pub(crate) use device::DevicePart;
#[cfg(feature = "midi")]
pub use midi::MidiNodes;
pub(crate) use midi::MidiPart;
pub(crate) use output::OutputPart;
pub(crate) use plugin::{EarlyWork, Loop, PluginOptions, PluginPart};
pub(crate) use pool::PoolPart;
pub(crate) use reports::Reports;
pub(crate) use sampler::SamplerPart;
pub(crate) use transport::TransportPart;
