//! Shared-memory audio transport.

mod slab;

pub use crate::SlabLayout;
pub use slab::{AudioSlab, SlabWriter};
pub use tutti_plugin_ipc_ffi::{BlobWire, RING_SLOTS, SlabHandle, SlabWire};
