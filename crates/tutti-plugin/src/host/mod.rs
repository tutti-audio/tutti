//! Host orchestration — the logic that loads, wires, and controls plugins.
//!
//! Format-agnostic (per-format FFI lives in [`crate::format`]) and
//! transport-agnostic (the process edge, the wire and the client of an out-of-process
//! plugin are `tutti-plugin-ipc`'s). What's here is the actual plumbing of
//! *being a plugin host*:
//!
//! - [`node`] — the [`PluginClient`](node::PluginClient): the generic plugin
//!   node over the out-of-process hosting, its typestates (born `Unbound`, bound into a
//!   graph node) and what the host keeps beside it.
//! - [`plugin`] / [`placement`] — the public load API.
//!
//! The control handle, the node's controls and the host policy written once
//! over a handle live in `tutti-plugin-host`.

pub mod node;
pub mod placement;
pub mod plugin;
