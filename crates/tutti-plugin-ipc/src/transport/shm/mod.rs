//! Shared-memory audio transport.

mod slab;

pub use crate::protocol::SlabLayout;
pub use slab::{AudioSlab, SlabLayoutError, SlabWriter};
pub use tutti_plugin_ipc_ffi::RING_SLOTS;
