//! Why a probe failed, typed: what the probe wrote when it refused a plugin
//! ([`ProbeRefusal`]), what it wrote for a plugin it could not measure
//! ([`MeasureFailure`]), and the host's whole account of a probe that gave no
//! descriptor ([`ProbeError`]).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tutti_buffers::MaxBlock;
use tutti_units::SampleRate;

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
        plugin: crate::PluginId,
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
    /// `--plugin` names a plugin the file does not hold.
    #[error("the file holds no plugin {0}")]
    UnknownPlugin(crate::PluginId),
    /// A state is given with no plugin named, and the file holds several: it would
    /// go to one of them by position, which no caller asked for.
    #[error("a state with no plugin named, and the file holds {count} plugins")]
    AmbiguousPlugin {
        /// How many plugins the file holds.
        count: usize,
    },
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

/// Why a plugin the probe described has no measurement. Each is a fact about
/// that plugin at one configuration; the other plugins of the file are
/// unaffected. The texts are the plugin host's.
#[derive(Debug, Clone, PartialEq, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MeasureFailure {
    /// The plugin would not load for a render.
    #[error("the plugin would not load for a render: {reason}")]
    Load {
        /// The loader's reason.
        reason: String,
    },
    /// The plugin refused the state it was asked to be measured in.
    #[error("the plugin refused the state: {reason}")]
    StateRefused {
        /// The plugin's reason.
        reason: String,
    },
    /// The plugin would not say what state it is in, so the figure has no state
    /// to be recorded against.
    #[error("the plugin would not save its state: {reason}")]
    SaveState {
        /// The plugin's reason.
        reason: String,
    },
    /// The plugin has no audio output.
    #[error("the plugin has no audio output")]
    NoAudioOutput,
    /// The plugin would not activate.
    #[error("activating at {rate} Hz with blocks of {block:?} failed: {reason}")]
    Activate {
        /// The rate asked for.
        rate: SampleRate,
        /// The block asked for.
        block: MaxBlock,
        /// The plugin's reason.
        reason: String,
    },
    /// A block failed to render.
    #[error("a block at {rate} Hz with blocks of {block:?} failed: {reason}")]
    Render {
        /// The rate rendered at.
        rate: SampleRate,
        /// The block rendered at.
        block: MaxBlock,
        /// The plugin's reason.
        reason: String,
    },
    /// The plugin would not deactivate.
    #[error("deactivating at {rate} Hz with blocks of {block:?} failed: {reason}")]
    Deactivate {
        /// The rate it was activated at.
        rate: SampleRate,
        /// The block it was activated at.
        block: MaxBlock,
        /// The plugin's reason.
        reason: String,
    },
    /// The probe's run budget was spent before this plugin's turn: it was not
    /// loaded, activated or rendered.
    #[error("the probe's run budget was spent before this plugin's turn")]
    BudgetSpent,
    /// The render ran past its deadline: a plugin slower than the budget allows.
    #[error("rendering at {rate} Hz with blocks of {block:?} passed its deadline")]
    TimedOut {
        /// The rate rendered at.
        rate: SampleRate,
        /// The block rendered at.
        block: MaxBlock,
    },
}

/// A plugin of the file the probe described and could not measure, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unmeasured {
    /// The plugin's id.
    pub plugin: crate::PluginId,
    /// Why it has no measurement.
    pub failure: MeasureFailure,
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
    /// The executor the probe waited on shut down under it; the probe was killed.
    #[error("the executor shut down during the probe")]
    ShutDown,
}
