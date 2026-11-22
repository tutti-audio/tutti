//! How a loaded plugin runs: [`AudioConfig`], the audio knobs lowered into a
//! [`IpcConfig`] for each plugin subprocess. Read per load; where plugins
//! live and what a scan does is `tutti-plugin-db`'s `CatalogConfig`.

use super::bridge::IpcConfig;
use tutti_buffers::FloatWidth;
use tutti_plugin_types::Timeouts;

/// How a loaded plugin runs: the audio knobs lowered into each plugin
/// subprocess's [`IpcConfig`].
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// Preferred audio sample format.
    pub format: FloatWidth,

    /// Largest block, in **frames** per channel, that will cross the bridge.
    ///
    /// Becomes [`IpcConfig::max_buffer_size`] and sizes the shared slab, so
    /// a later block may not exceed it.
    pub buffer_size: usize,

    /// How long each wait on a subprocess runs before erroring.
    pub timeouts: Timeouts,

    /// Whether the host clears its dumpable flag before it spawns its first
    /// plugin server ([`IpcConfig::protect_host_memory`]). Off by default, and
    /// irreversible for the process: a later `false` does not undo an earlier
    /// `true`.
    pub protect_host_memory: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            format: FloatWidth::F32,
            buffer_size: 8192,
            timeouts: Timeouts::default(),
            protect_host_memory: false,
        }
    }
}

impl AudioConfig {
    /// Creates the default config: 32-bit float samples, 8192-frame blocks and
    /// the default [`Timeouts`] (a 5 s call).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // --- chainable setters ---

    /// Sets the sample format to request from each plugin.
    #[must_use]
    pub fn format(mut self, format: FloatWidth) -> Self {
        self.format = format;
        self
    }

    /// Sets the maximum block size, in **frames** per channel.
    #[must_use]
    pub fn buffer_size(mut self, n: usize) -> Self {
        self.buffer_size = n;
        self
    }

    /// Sets how long each wait on a subprocess runs before erroring.
    #[must_use]
    pub fn timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Sets whether the host protects its memory from the plugins it spawns
    /// ([`IpcConfig::protect_host_memory`]).
    #[must_use]
    pub fn protect_host_memory(mut self, protect: bool) -> Self {
        self.protect_host_memory = protect;
        self
    }

    // --- internal resolution ---

    /// The bridge configuration a plugin loaded under this catalog config gets.
    pub fn to_bridge_config(&self) -> IpcConfig {
        IpcConfig {
            preferred_format: self.format,
            max_buffer_size: self.buffer_size,
            timeouts: self.timeouts,
            protect_host_memory: self.protect_host_memory,
        }
    }
}
