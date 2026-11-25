//! Plugin bridge layer.
//!
//! [`audio::AudioBridge`] is the RT-safe command/response bus to the
//! plugin-server subprocess. [`plugin_bridge::PluginBridge`] pairs it with an
//! [`editor_site::EditorSite`]: an in-process GUI instance (from the host's
//! [`EditorLoader`]), or with no loader the editor in the server
//! ([`ipc_editor`]'s `IpcEditor`).

pub(crate) mod audio;
pub(crate) mod editor_site;
#[cfg(test)]
pub(crate) mod hostile_peer_tests;
mod ipc_editor;
pub(crate) mod plugin_bridge;

pub use editor_site::EditorSite;
pub use plugin_bridge::{EditorLoader, PluginBridge};
