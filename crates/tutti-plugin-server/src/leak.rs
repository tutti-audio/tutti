//! What this process left behind, which its exit status then says.
//!
//! A plugin whose Processor never came home from a stuck render thread is not
//! destroyed (destroying it would free memory the render thread still reads):
//! it is leaked. A task that did not stop within its patience is detached and
//! left running. Both happen deep inside a drop, where no result can return
//! them, so they are noted here, per process, and read by
//! [`PluginServer::run`](crate::PluginServer::run) at the end.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static LEAKED: AtomicBool = AtomicBool::new(false);
static DETACHED: AtomicU64 = AtomicU64::new(0);

/// Notes that a plugin was leaked.
pub(crate) fn note() {
    LEAKED.store(true, Ordering::Release);
}

/// Whether a plugin was leaked in this process.
pub(crate) fn happened() -> bool {
    LEAKED.load(Ordering::Acquire)
}

/// Notes that a task did not stop within its patience and was left running.
pub(crate) fn note_detached() {
    DETACHED.fetch_add(1, Ordering::Relaxed);
}

/// How many tasks were left running in this process.
pub(crate) fn detached() -> u64 {
    DETACHED.load(Ordering::Relaxed)
}
