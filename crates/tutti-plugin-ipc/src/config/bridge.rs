//! Host-local configuration for a plugin bridge. Not wire data —
//! controls how *this* process connects to and manages a plugin-server.

use serde::{Deserialize, Serialize};
use tutti_buffers::FloatWidth;
use tutti_plugin_types::Timeouts;

/// Low-level settings for one plugin-server connection.
///
/// Most hosts never build one: [`AudioConfig`](crate::AudioConfig)
/// covers the settings an application tunes, and each load derives a
/// `IpcConfig` from it.
///
/// [`new`](Self::new) uses 8192-frame blocks, the default [`Timeouts`] and
/// 32-bit float samples. The server's connection is made per launch (a socket
/// the host hands the server it spawns), so nothing here names it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcConfig {
    /// Largest block, in **frames**, the plugin will be asked to process. Sizes
    /// the slab, so a later block may not exceed it.
    pub max_buffer_size: usize,
    /// How long each wait on the subprocess runs before erroring.
    pub timeouts: Timeouts,
    /// Sample format to request; the subprocess replies with what it negotiated.
    #[serde(default = "crate::protocol::default_format")]
    pub preferred_format: FloatWidth,
    /// Clear the host's dumpable flag (Linux: PR_SET_DUMPABLE 0) before each
    /// server is spawned, so a plugin, which runs as the same user, cannot
    /// `ptrace` the host or write its memory. Off by default: the flag also
    /// stops the host dumping core and an unprivileged debugger or crash
    /// handler attaching to it, which is the app's to choose. Irreversible for the
    /// process: the flag stays cleared, and a later `false` does not undo an
    /// earlier `true`. A launch that asks for it and cannot have it is refused
    /// ([`IpcError::HostMemoryUnprotected`](tutti_plugin_types::IpcError::HostMemoryUnprotected)) and starts no server. Nothing
    /// elsewhere (macOS's hardened runtime is the app's signing).
    #[serde(default)]
    pub protect_host_memory: bool,
}

impl IpcConfig {
    /// A configuration with the defaults: 8192-frame blocks, the default
    /// [`Timeouts`], 32-bit float samples and the host's memory unprotected.
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_buffer_size: 8192,
            timeouts: Timeouts::default(),
            preferred_format: FloatWidth::F32,
            protect_host_memory: false,
        }
    }
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self::new()
    }
}
