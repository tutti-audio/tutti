//! [`CatalogConfig`]: where plugins live and what a scan does with them.
//!
//! The caller supplies `pedal_dir` and `scan_dirs`: this library has no opinion
//! on OS conventions or app names.

use crate::probe::DEFAULT_PROBE_TIMEOUT;
use crate::probing::DEFAULT_MEASURE_TIMEOUT;
use std::path::PathBuf;
use std::time::Duration;
use tutti_plugin_types::Locator;

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

    /// How long a probe may take, a cold plugin's load included
    /// ([`DEFAULT_PROBE_TIMEOUT`]).
    pub probe_timeout: Duration,

    /// How long a measuring probe may take, its renders included
    /// ([`DEFAULT_MEASURE_TIMEOUT`]).
    pub measure_timeout: Duration,

    /// Whether scans and registrations also measure the plugins they probe
    /// ([`measuring`](Self::measuring())).
    pub measuring: bool,
}

impl CatalogConfig {
    /// Creates a config with the sentinel directory and no scan directories
    /// ([`scan_dirs`](Self::scan_dirs) names them).
    pub fn new(pedal_dir: impl Into<PathBuf>) -> Self {
        Self {
            pedal_dir: pedal_dir.into(),
            scan_dirs: Vec::new(),
            probe_locator: Locator::Search,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            measure_timeout: DEFAULT_MEASURE_TIMEOUT,
            measuring: false,
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
        self.probe_timeout = timeout;
        self
    }

    /// Sets how long a measuring probe may take.
    #[must_use]
    pub fn measure_timeout(mut self, timeout: Duration) -> Self {
        self.measure_timeout = timeout;
        self
    }

    /// Makes scans **measure** the plugins they probe: each new or changed file
    /// is also rendered, in the probe's own process, and its descriptors carry
    /// [`measured`](tutti_plugin_types::PluginDescriptor::measured).
    /// Off by default: measuring renders each plugin at two configurations, which
    /// costs seconds a plain probe does not, and a plugin that crashes or hangs
    /// while rendering is blacklisted like one that crashes in its load. A scan
    /// skips a file whose time has not changed, so it never measures one it has
    /// already probed: a catalog's `measure` does.
    #[must_use]
    pub fn measuring(mut self) -> Self {
        self.measuring = true;
        self
    }

    // --- internal resolution ---

    /// The crash sentinel file, in `pedal_dir`.
    pub fn pedal_path(&self) -> PathBuf {
        self.pedal_dir.join(".scanning")
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

    /// **A config measures only when asked**: off by default, on after
    /// `measuring`.
    #[test]
    fn measuring_is_off_until_asked() {
        assert!(!CatalogConfig::new("/tmp").measuring);
        assert!(CatalogConfig::new("/tmp").measuring().measuring);
    }
}
