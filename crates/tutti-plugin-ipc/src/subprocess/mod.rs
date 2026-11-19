//! Subprocess lifecycle: locate, spawn and hand-shake the `plugin-server` child
//! process.

mod launch;
mod reaped;

pub(crate) use launch::launch;
pub(crate) use launch::swap_slab;
pub use reaped::Reaped;

// The `*_with` entry points are deliberately NOT re-exported. They exist so a
// test can name the server binary with a `tutti_plugin_types::Locator` instead
// of through the `TUTTI_PLUGIN_SERVER` process-global; no production caller
// chooses a binary. `launch` is the shipped surface, and it keeps the
// environment-and-`PATH` search.
