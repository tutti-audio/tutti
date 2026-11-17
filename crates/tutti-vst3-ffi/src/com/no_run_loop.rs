//! The run loop off Linux: nothing to lend.
//!
//! Cocoa and Win32 have an ambient run loop, so a host there lends the plugin
//! none and no object answers `IRunLoop`. This module has the surface of the
//! Linux `run_loop.rs` (selected in `com/mod.rs`) with zero-sized types and
//! methods that do nothing, so the objects that hold a run loop, and the
//! callers that pump one, are the same code on every platform.

use std::sync::Arc;
use std::time::Instant;
use tutti_plugin_types::{MainThreadMarker, Ready, Wakeups};
use vst3::Steinberg::IPlugFrame;
use vst3::Steinberg::TUID;
use vst3::Steinberg::Vst::{IHostApplication, IPlugInterfaceSupport};

/// The interfaces [`HostApplication`](super::HostApplication) answers.
pub(crate) type HostApplicationInterfaces = (IHostApplication, IPlugInterfaceSupport);

/// The interfaces [`HostPlugFrame`](super::HostPlugFrame) answers.
pub(crate) type PlugFrameInterfaces = (IPlugFrame,);

/// The interface ids this platform answers beyond the common ones.
pub(crate) const RUN_LOOP_IIDS: &[TUID] = &[];

/// Implements the run-loop interface for a host object: none off Linux.
macro_rules! delegate_run_loop {
    ($ty:ty) => {};
}
pub(crate) use delegate_run_loop;

/// Which plugin instance a registration belongs to: there are none to tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owner;

impl Owner {
    /// The one owner.
    pub(crate) fn next() -> Self {
        Self
    }
}

/// The host's run loop: it holds nothing.
#[derive(Default)]
pub(crate) struct RunLoop;

opaque_debug!(RunLoop);

/// The guard of [`RunLoop::scope_owned`]: it scopes nothing.
pub(crate) struct OwnedScope;

impl RunLoop {
    /// A run loop.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    /// Adds no wakeup: the OS owns the loop.
    pub(crate) fn describe(&self, _out: &mut Wakeups) {}

    /// Does nothing.
    pub(crate) fn run_iteration(&self, _main: MainThreadMarker) {}

    /// Runs nothing.
    pub(crate) fn dispatch(&self, _main: MainThreadMarker, _ready: &[Ready], _now: Instant) {}

    /// Scopes nothing.
    pub(crate) fn scope_owned(
        self: &Arc<Self>,
        _owner: Owner,
        _main: MainThreadMarker,
    ) -> OwnedScope {
        OwnedScope
    }

    /// Forgets every registration: there are none.
    pub(crate) fn clear_all(&self) -> usize {
        0
    }

    /// Takes back `owner`'s registrations: there are none.
    pub(crate) fn purge(&self, _owner: Owner) -> usize {
        0
    }

    /// What the loop registered and fired: nothing.
    #[cfg(feature = "conformance")]
    pub(crate) fn activity(&self) -> crate::RunLoopActivity {
        crate::RunLoopActivity::default()
    }
}
