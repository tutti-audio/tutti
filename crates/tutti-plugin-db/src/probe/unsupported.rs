//! The probe on a platform that starts no processes (the web): every probe is
//! unsupported, so a scan lists no plugin and records nothing.

use crate::scanner::ProbeListing;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;
use tutti_platform::WeakExecutor;
use tutti_plugin_types::{Locator, PluginFormat, ProbeError};

/// Always [`ProbeError::Io`] of [`Unsupported`](std::io::ErrorKind::Unsupported).
pub(super) async fn run(
    _executor: &WeakExecutor,
    _locator: &Locator,
    _plugin_path: &Path,
    _format: PluginFormat,
    _timeout: Duration,
    _flags: &[&OsStr],
) -> Result<ProbeListing, ProbeError> {
    Err(ProbeError::Io(std::io::ErrorKind::Unsupported))
}
