//! User-facing configuration for the `tutti-plugin`'s `Catalog`.
//!
//! Two structs, because they answer two unrelated questions and change at
//! different times:
//!
//! - [`CatalogConfig`] — *where plugins live*. The directory of the scanner's
//!   crash sentinel and the scan directories. Set once at startup, read by discovery.
//! - [`AudioConfig`] — *how a loaded plugin runs*. Sample format, block size,
//!   IPC timeout. Read per `load`, and lowered into a [`BridgeConfig`] for each
//!   plugin subprocess.
//!
//! Two structs rather than one: fused, a caller adjusting the audio block size
//! must restate the sentinel directory, and the combined type appears in discovery
//! signatures that never read a single audio field.
//!
//! The caller supplies `pedal_dir` and `scan_dirs` — this library has no opinion
//! on OS conventions or app names.

use super::bridge::{BridgeConfig, RandomSourceError};
use std::path::PathBuf;
use std::time::Duration;
use tutti_buffers::FloatWidth;
use tutti_plugin_types::{Locator, Timeouts};

/// Where the scanner keeps its crash sentinel and which directories to scan.
#[derive(Debug, Clone)]
pub struct CatalogConfig {
    /// The directory of the scanner's crash sentinel (`.scanning`), which must be
    /// writable. Where the database itself lives is the persistence closure's
    /// business, not the config's.
    pub pedal_dir: PathBuf,

    /// Directories to scan for plugins.
    pub scan_dirs: Vec<PathBuf>,

    /// Where the catalog finds `plugin-probe`, the process every probe runs in:
    /// [`Locator::Search`] unless a host or a test names the binary.
    pub probe_locator: Locator,

    /// How long a probe may take, or `None` for the catalog's default (fifteen
    /// seconds, a cold plugin's load included).
    pub probe_timeout: Option<Duration>,

    /// How long a measuring probe may take, or `None` for the catalog's default
    /// (three minutes: two renders of up to the longest tail).
    pub measure_timeout: Option<Duration>,
}

impl CatalogConfig {
    /// Creates a config with the sentinel directory and no scan directories
    /// ([`scan_dirs`](Self::scan_dirs) names them).
    pub fn new(pedal_dir: impl Into<PathBuf>) -> Self {
        Self {
            pedal_dir: pedal_dir.into(),
            scan_dirs: Vec::new(),
            probe_locator: Locator::Search,
            probe_timeout: None,
            measure_timeout: None,
        }
    }

    // --- chainable setters ---

    /// Sets the directory of the scanner's crash sentinel.
    #[must_use]
    pub fn pedal_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.pedal_dir = dir.into();
        self
    }

    /// Sets the directories a scan walks, replacing any already configured.
    #[must_use]
    pub fn scan_dirs<I, P>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.scan_dirs = dirs.into_iter().map(Into::into).collect();
        self
    }

    /// Sets where the catalog finds `plugin-probe`.
    #[must_use]
    pub fn probe_locator(mut self, locator: Locator) -> Self {
        self.probe_locator = locator;
        self
    }

    /// Sets how long a probe may take.
    #[must_use]
    pub fn probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = Some(timeout);
        self
    }

    /// Sets how long a measuring probe may take.
    #[must_use]
    pub fn measure_timeout(mut self, timeout: Duration) -> Self {
        self.measure_timeout = Some(timeout);
        self
    }

    // --- internal resolution ---

    /// The crash sentinel file, in `pedal_dir`.
    pub fn pedal_path(&self) -> PathBuf {
        self.pedal_dir.join(".scanning")
    }
}

/// How a loaded plugin runs: the audio knobs lowered into each plugin
/// subprocess's [`BridgeConfig`].
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// Preferred audio sample format.
    pub format: FloatWidth,

    /// Largest block, in **frames** per channel, that will cross the bridge.
    ///
    /// Becomes [`BridgeConfig::max_buffer_size`] and sizes the shared slab, so
    /// a later block may not exceed it.
    pub buffer_size: usize,

    /// How long each wait on a subprocess runs before erroring.
    pub timeouts: Timeouts,

    /// Whether the host clears its dumpable flag before it spawns its first
    /// plugin server ([`BridgeConfig::protect_host_memory`]). Off by default, and
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
    /// ([`BridgeConfig::protect_host_memory`]).
    #[must_use]
    pub fn protect_host_memory(mut self, protect: bool) -> Self {
        self.protect_host_memory = protect;
        self
    }

    // --- internal resolution ---

    /// The bridge configuration a plugin loaded under this catalog config gets.
    ///
    /// # Errors
    ///
    /// The operating system's random source cannot be read, so no socket name
    /// can be made.
    pub fn to_bridge_config(&self) -> Result<BridgeConfig, RandomSourceError> {
        Ok(BridgeConfig {
            preferred_format: self.format,
            max_buffer_size: self.buffer_size,
            timeouts: self.timeouts,
            protect_host_memory: self.protect_host_memory,
            ..BridgeConfig::at(crate::unique_socket_path()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A new config scans nothing**, and `scan_dirs` takes any paths and
    /// replaces what was set.
    #[test]
    fn scan_dirs_replaces_the_empty_default() {
        let cfg = CatalogConfig::new("/tmp");
        assert!(cfg.scan_dirs.is_empty());
        assert_eq!(cfg.pedal_dir, PathBuf::from("/tmp"));

        let cfg = cfg.scan_dirs(["/usr/lib/vst3", "/usr/lib/clap"]);
        assert_eq!(
            cfg.scan_dirs,
            [
                PathBuf::from("/usr/lib/vst3"),
                PathBuf::from("/usr/lib/clap")
            ]
        );
    }
}
