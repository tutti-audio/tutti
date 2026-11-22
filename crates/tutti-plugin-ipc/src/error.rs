//! The error types of the process edge.
//!
//! [`IpcError`] (in `tutti-plugin-types`) is the host-side error most calls
//! return. [`PluginRenderFault`] describes how a forked plugin instance
//! failed while rendering.

use thiserror::Error;
use tutti_plugin_types::{IpcError, PluginFailure};

/// How a forked plugin instance failed while rendering.
///
/// This is the cause behind a `tutti_graph::ForkFault` for a plugin node. After
/// a crash or a timeout the fork renders silence; after a latency change it
/// renders misaligned.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum PluginRenderFault {
    /// The fork's plugin-server died.
    #[error("the forked instance's plugin-server died: {cause}")]
    Crashed {
        /// What the bridge latched when it noticed.
        #[source]
        cause: PluginFailure,
    },
    /// The fork's plugin refused the sample rate it renders at, so it renders
    /// silence: the whole export is wrong from the first block.
    #[error("the forked instance refused its sample rate")]
    RateRefused(#[source] tutti_plugin_types::RateRefusal),
    /// The fork's server did not publish a block within the budget
    /// (`IpcConfig::timeout_ms`). The fork stopped waiting: every later
    /// block is silence, at once.
    #[error(
        "the forked instance did not answer a block within {budget:?}; \
         it renders silence from then on"
    )]
    TimedOut {
        /// The per-block budget it missed.
        budget: std::time::Duration,
    },
    /// The forked instance's latency is not the one its graph was compiled
    /// against: the plugin changed it after the fork's plan was made, so the
    /// render's delay compensation (and an export's latency trim) is off by
    /// the difference. The fork waits for a latency its render-mode and rate
    /// changes cause before its plan is made, so this is a change the plugin
    /// made later, on its own.
    #[error(
        "the forked instance's latency moved from {planned:?} to {now:?} after \
         its graph was compiled; the render is misaligned by the difference"
    )]
    LatencyChanged {
        /// What the fork's graph compensates for.
        planned: tutti_units::Latency,
        /// What the instance declares now.
        now: tutti_units::Latency,
    },
}

/// The launch error of a server that [`tutti_plugin_ipc_ffi::spawn`] refused.
pub(crate) fn spawn_failed(error: tutti_plugin_ipc_ffi::SpawnError) -> IpcError {
    match error {
        tutti_plugin_ipc_ffi::SpawnError::Start(e) => IpcError::Io(e),
        tutti_plugin_ipc_ffi::SpawnError::NotBound(e) => IpcError::ServerNotBound(e),
    }
}

/// Builds an `UnexpectedMessage` error by Debug-formatting the received
/// message.
pub(crate) fn unexpected_message(expected: &'static str, got: &impl std::fmt::Debug) -> IpcError {
    IpcError::UnexpectedMessage {
        expected,
        got: format!("{got:?}").into(),
    }
}

/// A postcard failure as the [`IpcError`] it is on the wire.
pub(crate) fn codec(e: postcard::Error) -> IpcError {
    IpcError::Serialization(e)
}
