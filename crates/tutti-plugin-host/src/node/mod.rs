//! The plugin node, over any [`Loaded`](tutti_plugin_types::Loaded) hosting: an
//! [`InProcess`](tutti_plugin_types::InProcess) format ([`load_client`]), or a
//! plugin hosted by another process ([`client_over`]: built over an
//! already-activated plugin, with no main thread).
//!
//! The plugin's `Lifecycle` runs on the control side; its
//! realtime `Processor` is lent to the audio thread through a
//! `tutti_rt::handoff`, which takes no lock. A block with no Processor in hand
//! (one recalled for a rate change, or refused its rate) renders silence and
//! is counted ([`BlockLosses::unlent`](crate::BlockLosses::unlent)); one the
//! Processor failed renders silence too
//! ([`BlockLosses::failed`](crate::BlockLosses::failed)). The handle's control surface is the plugin's
//! [`Control`](tutti_plugin_types::Control).
//!
//! Nothing here names a format: [`load_client`] takes any `InProcess` format the
//! caller names.

mod client;
mod control_backend;
mod fork;
mod graph;
mod host_edits;
mod instance;
mod loader;
#[cfg(test)]
mod midi_map_tests;
#[cfg(test)]
mod mute_tests;
mod off_main_wait;
mod rebind;
mod width_error;

pub use client::{InProcessClient, LiveView, PluginNode, declared_shape};
pub use loader::{
    ClientOverError, LoadClientError, client_over, load_client, load_client_restoring,
};
pub use rebind::{Pending, RebindError, Rebinder, Settlement};
pub use width_error::WidthError;
