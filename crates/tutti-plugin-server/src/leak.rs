//! Whether this process leaked a plugin, which its exit status then says.
//!
//! A plugin whose Processor never came home from a stuck render thread is not
//! destroyed (destroying it would free memory the render thread still reads):
//! it is leaked. That happens deep inside the served plugin's drop, where no
//! result can return it, so it is noted here, once per process, and read by
//! [`PluginServer::run`](crate::PluginServer::run) at the end.

use std::sync::atomic::{AtomicBool, Ordering};

static LEAKED: AtomicBool = AtomicBool::new(false);

/// Notes that a plugin was leaked.
pub(crate) fn note() {
    LEAKED.store(true, Ordering::Release);
}

/// Whether a plugin was leaked in this process.
pub(crate) fn happened() -> bool {
    LEAKED.load(Ordering::Acquire)
}
