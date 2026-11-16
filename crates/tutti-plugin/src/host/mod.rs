//! Host orchestration — the logic that loads, wires, and controls plugins.
//!
//! Format-agnostic (per-format FFI lives in [`crate::format`]) and
//! transport-agnostic (the process edge, the wire and the client of a served
//! plugin are `tutti-plugin-ipc`'s). What's here is the actual plumbing of
//! *being a plugin host*:
//!
//! - [`db_probe`] — the subprocess probe that `tutti-plugin-db` scans with.
//! - [`node`] — the [`PluginClient`](node::PluginClient): the generic plugin
//!   node over the served hosting, its typestates (born `Unbound`, bound into a
//!   graph node) and what the host keeps beside it.
//! - [`plugin`] / [`catalog`] / [`loadable`] — the public load API.
//!
//! The control handle, the node's controls and the host policy written once
//! over a handle live in `tutti-plugin-host`.

pub mod catalog;
pub mod db_probe;
pub mod loadable;
pub mod node;
pub mod plugin;
pub mod plugin_ref;
pub(crate) mod probe;
