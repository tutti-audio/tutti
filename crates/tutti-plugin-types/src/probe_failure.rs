//! Why a probe failed, typed: what the probe wrote when it refused a plugin
//! ([`ProbeRefusal`]) and the host's whole account of a probe that gave no
//! descriptor ([`ProbeError`]).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{LoadStage, PluginFormat, ProbeDeath, ResultError};

/// Why a probe that ran to its end read no plugin: written to the result file,
/// so a host tells a refusal from a crash by the file.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ProbeRefusal {
    /// The file does not exist.
    #[error("the plugin file does not exist")]
    Missing,
    /// The path names no plugin format.
    #[error("the path names no plugin format")]
    UnsupportedFormat,
    /// The probe was built without the format's cargo feature.
    #[error("format {} is not compiled into this plugin-probe", .0.extension_id())]
    FormatNotCompiled(PluginFormat),
    /// The `--state` file could not be read. The text is the OS's.
    #[error("cannot read the state: {0}")]
    StateUnreadable(String),
    /// The plugin refused the state it was asked to be measured in: a
    /// measurement of a state it would not take is no measurement.
    #[error("{plugin}: the plugin refused the state: {reason}")]
    StateRefused {
        /// The id of the plugin that refused.
        plugin: String,
        /// The plugin's own reason.
        reason: String,
    },
    /// A loader refused the file at `stage`. The text is the loader's.
    #[error("load refused while {stage}: {reason}")]
    Load {
        /// The phase of the load that failed.
        stage: LoadStage,
        /// The loader's own reason.
        reason: String,
    },
    /// A loader refused the file with no phase to name. The text is the loader's.
    #[error("the file would not load: {0}")]
    Describe(String),
    /// The probe's command line is wrong: it loaded nothing.
    #[error("the probe's command line is wrong")]
    Usage,
    /// The probe is not running on its process's main thread.
    #[error("the probe is not running on its process's main thread")]
    NotMainThread,
    /// The file loaded and holds no plugin.
    #[error("the file holds no plugin")]
    Empty,
}

/// A probe that gave no descriptor, as the host that ran it tells it: what the
/// process did, and what it wrote.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProbeError {
    /// `plugin-probe` could not be located, so nothing was looked at.
    #[error("plugin-probe was not found")]
    ProbeNotFound,
    /// `plugin-probe` was found and could not be run or waited for.
    #[error("plugin-probe could not be run or waited for: {0}")]
    Io(std::io::ErrorKind),
    /// The probe ran to its end and refused the plugin.
    #[error("the probe refused the plugin: {0}")]
    Refused(#[from] ProbeRefusal),
    /// The probe ended without a result.
    #[error("the probe died: {0}")]
    Died(ProbeDeath),
    /// The probe did not finish by its deadline, and was killed.
    #[error("the probe did not finish within {after:?}")]
    TimedOut {
        /// The deadline it was given.
        after: Duration,
    },
    /// The probe's result file could not be read as a result.
    #[error("the probe's result is unusable: {0}")]
    Result(#[from] ResultError),
    /// The caller stopped the probe, which was killed.
    #[error("the probe was cancelled")]
    Cancelled,
}
