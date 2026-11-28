//! What this process left behind, which its exit status then says.
//!
//! A plugin whose Processor never came home from a stuck render thread is not
//! destroyed (destroying it would free memory the render thread still reads):
//! it is leaked. That happens deep inside a drop, where no result can return
//! it, so it is noted here, per process, and read by
//! [`PluginServer::run`](crate::PluginServer::run) at the end. The tasks left
//! running and the render lanes lost are counted by the session
//! ([`Ties`](crate::ties::Ties)), which tells the host as it counts.

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
