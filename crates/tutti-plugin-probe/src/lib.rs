#![doc = include_str!("../README.md")]
//! What `plugin-probe` does: read the descriptor of every plugin the file at a path
//! holds, as the format its extension names, through that format's host
//! ([`Describe`]), and with [`describe_measured`] measure each one ([`InProcess`]).
//!
//! The binary (`src/main.rs`) is the command line around them.

#![forbid(unsafe_code)]

use std::path::Path;
use std::time::Instant;
use tutti_plugin_types::{
    Deadline, Describe, InProcess, MainThreadMarker, MeasureFailure, PluginDescriptor, PluginError,
    PluginFormat, ProbeRefusal, Unmeasured,
};

mod measure;

use measure::measure;

/// The format a file is read as: `format` when the caller names one, and the
/// one its extension names otherwise.
fn routing(path: &Path, format: Option<PluginFormat>) -> Option<PluginFormat> {
    format.or_else(|| PluginFormat::from_path(path))
}

/// The descriptor of every plugin the file at `path` holds, read as `format` or,
/// with none, as its extension says, in the file's own order, or why it
/// could not be read. A CLAP factory gives each plugin it lists, a VST3 factory
/// each audio class, an AU bundle each unit it declares, a VST2 shell each
/// sub-plugin; any other file is one plugin.
/// `main` proves the caller is on the main thread, which VST2's load needs and the
/// other formats' probes do not.
///
/// # Errors
///
/// A [`ProbeRefusal`]: the file is missing, the format is not compiled in or
/// unknown, or the loader refused the file (at the stage it names, where it
/// does).
pub fn describe(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
) -> Result<Vec<PluginDescriptor>, ProbeRefusal> {
    with_host(main, path, format, None).map(|(plugins, _)| plugins)
}

/// The plugins a measuring probe was asked about: which one takes the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateFor<'a> {
    /// The id of the plugin the state is for; `None` is the file's first plugin.
    pub plugin: Option<&'a str>,
    /// The bytes the plugin's own `save_state` wrote.
    pub bytes: &'a [u8],
}

impl StateFor<'_> {
    /// Whether the plugin `id` is the one the state is for: the one named, or, with
    /// none named, the file's only plugin (the caller has checked there is one).
    fn is_for(&self, id: &str) -> bool {
        self.plugin.is_none_or(|wanted| wanted == id)
    }
}

/// What a measuring run is asked: the state to put one plugin in, and the
/// deadline of the whole run.
#[derive(Clone, Copy)]
struct Measuring<'a> {
    state: Option<StateFor<'a>>,
    run: Deadline,
}

/// [`describe`], then measure each plugin the file holds: the descriptors come
/// back with [`PluginDescriptor::measured`] set where the plugin could be
/// measured, and the plugins that could not be come back beside them, each with
/// its [`MeasureFailure`].
///
/// `state` puts one plugin in a state before it is measured (bytes the plugin's
/// own `save_state` wrote): the plugin it names, or the file's only plugin when it
/// names none. The others are measured as they load, and without `state` every
/// plugin is. `run` is the deadline of the whole run: each plugin's renders are
/// bounded by what it leaves, and once it has passed the plugins that remain are
/// reported [`MeasureFailure::BudgetSpent`] without being loaded.
///
/// A plugin that crashes or hangs while measuring takes this process with it, so
/// the host that spawned it sees the crash or the timeout of a probe that failed
/// to load.
///
/// # Errors
///
/// As [`describe`]; [`ProbeRefusal::UnknownPlugin`] if `state` names a plugin the
/// file does not hold; [`ProbeRefusal::AmbiguousPlugin`] if it names none and the
/// file holds several; [`ProbeRefusal::StateRefused`] if the plugin took no such state.
pub fn describe_measured(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
    state: Option<StateFor<'_>>,
    run: Deadline,
) -> Result<(Vec<PluginDescriptor>, Vec<Unmeasured>), ProbeRefusal> {
    with_host(main, path, format, Some(Measuring { state, run }))
}

/// [`run`] with the host of the format the file is read as.
fn with_host(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
    measuring: Option<Measuring<'_>>,
) -> Result<(Vec<PluginDescriptor>, Vec<Unmeasured>), ProbeRefusal> {
    match routing(path, format) {
        #[cfg(feature = "vst3")]
        Some(PluginFormat::Vst3) => run::<tutti_vst3_host::Vst3Loaded>(main, path, measuring),
        #[cfg(feature = "clap")]
        Some(PluginFormat::Clap) => run::<tutti_clap_host::ClapLoaded>(main, path, measuring),
        #[cfg(feature = "vst2")]
        Some(PluginFormat::Vst2) => run::<tutti_vst2_host::Vst2Loaded>(main, path, measuring),
        Some(PluginFormat::AudioUnit) => audio_unit::run(main, path, measuring),
        Some(format) => Err(ProbeRefusal::FormatNotCompiled(format)),
        None => Err(ProbeRefusal::UnsupportedFormat),
    }
}

/// Audio Units are macOS's alone: the one module that differs by platform.
#[cfg(all(feature = "au", target_os = "macos"))]
mod audio_unit {
    use super::{MainThreadMarker, Measuring, Path, PluginDescriptor, ProbeRefusal, Unmeasured};

    pub(super) fn run(
        main: MainThreadMarker,
        path: &Path,
        measuring: Option<Measuring<'_>>,
    ) -> Result<(Vec<PluginDescriptor>, Vec<Unmeasured>), ProbeRefusal> {
        super::run::<tutti_au_host::AuLoaded>(main, path, measuring)
    }
}

/// Without macOS or the feature, the format is not compiled in.
#[cfg(not(all(feature = "au", target_os = "macos")))]
mod audio_unit {
    use super::{
        MainThreadMarker, Measuring, Path, PluginDescriptor, PluginFormat, ProbeRefusal, Unmeasured,
    };

    pub(super) fn run(
        _main: MainThreadMarker,
        _path: &Path,
        _measuring: Option<Measuring<'_>>,
    ) -> Result<(Vec<PluginDescriptor>, Vec<Unmeasured>), ProbeRefusal> {
        Err(ProbeRefusal::FormatNotCompiled(PluginFormat::AudioUnit))
    }
}

/// Describes the file's plugins as `L`, and, when `measuring`, measures each one
/// with one load of its own.
fn run<L: InProcess + Describe>(
    main: MainThreadMarker,
    path: &Path,
    measuring: Option<Measuring<'_>>,
) -> Result<(Vec<PluginDescriptor>, Vec<Unmeasured>), ProbeRefusal> {
    let mut plugins = L::describe(main, path).map_err(|e| refusal(path, e))?;
    let Some(Measuring { state, run }) = measuring else {
        return Ok((plugins, Vec::new()));
    };
    match state.map(|s| s.plugin) {
        Some(Some(wanted)) if !plugins.iter().any(|p| p.id == wanted) => {
            return Err(ProbeRefusal::UnknownPlugin(wanted.to_owned()));
        }
        // A state with no plugin named is for the file's plugin, which must be one.
        Some(None) if plugins.len() != 1 => {
            return Err(ProbeRefusal::AmbiguousPlugin {
                count: plugins.len(),
            });
        }
        _ => {}
    }
    let mut unmeasured = Vec::new();
    for plugin in &mut plugins {
        // A spent budget loads nothing more: the plugin is not loaded, activated
        // or rendered, and a state meant for it is not applied.
        if run.passed(Instant::now()) {
            unmeasured.push(Unmeasured {
                plugin: plugin.id.clone(),
                failure: MeasureFailure::BudgetSpent,
            });
            continue;
        }
        let state = state.filter(|s| s.is_for(&plugin.id)).map(|s| s.bytes);
        match measure::<L>(main, path, plugin, state, run) {
            Ok(measured) => plugin.measured = Some(measured),
            Err(MeasureFailure::StateRefused { reason }) => {
                return Err(ProbeRefusal::StateRefused {
                    plugin: plugin.id.clone(),
                    reason,
                });
            }
            Err(failure) => unmeasured.push(Unmeasured {
                plugin: plugin.id.clone(),
                failure,
            }),
        }
    }
    Ok((plugins, unmeasured))
}

/// Why the host `error` read the file at `path` as no plugins: a file that is
/// not there is [`ProbeRefusal::Missing`], and otherwise the loader's stage and
/// text.
fn refusal(path: &Path, error: PluginError) -> ProbeRefusal {
    if !path.exists() {
        return ProbeRefusal::Missing;
    }
    match error {
        PluginError::Load { stage, reason } => ProbeRefusal::Load { stage, reason },
        other => ProbeRefusal::Describe(other.to_string()),
    }
}
