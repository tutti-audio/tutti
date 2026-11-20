//! Plugin host configuration, in two layers — each one a different
//! question, so each one its own struct.
//!
//! - [`AudioConfig`] (in [`audio`]) — **how a loaded plugin runs**: sample
//!   format, block size, IPC timeout. Read per load.
//! - [`IpcConfig`] (in [`bridge`]) — the **low-level, per-subprocess**
//!   transport tuning: socket path, shared-memory prefix, buffer size,
//!   timeout, sample format. One drives a single plugin-server connection;
//!   `AudioConfig::to_bridge_config` derives it.
//!
//! Config is a leaf: it describes, the host layer builds.

pub mod audio;
pub mod bridge;

pub use audio::AudioConfig;
pub(crate) use bridge::unique_name;
pub use bridge::{IpcConfig, RandomSourceError, unique_socket_path};
