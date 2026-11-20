//! Shared-memory audio transport.

mod slab;

pub use crate::SlabLayout;
pub use slab::{AudioSlab, SlabWriter};
pub use tutti_plugin_ipc_ffi::{RING_SLOTS, SlabHandle, SlabWire};
