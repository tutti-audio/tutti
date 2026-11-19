//! Reading a file through the host of its format, built only with a format to host.

use super::{MainThreadMarker, Measuring, Read, StateFor, routing};
use std::path::Path;
use tutti_platform::Instant;
use tutti_plugin_types::{
    Describe, InProcess, MeasureFailure, PluginError, PluginFormat, ProbeRefusal, Unmeasured,
};

mod measure;

use measure::measure;

/// [`run`] with the host of the format the file is read as.
pub(super) fn with_host(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
    measuring: Option<Measuring<'_>>,
) -> Read {
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
    use super::{MainThreadMarker, Measuring, Path, Read};

    pub(super) fn run(
        main: MainThreadMarker,
        path: &Path,
        measuring: Option<Measuring<'_>>,
    ) -> Read {
        super::run::<tutti_au_host::AuLoaded>(main, path, measuring)
    }
}

/// Without macOS or the feature, the format is not compiled in.
#[cfg(not(all(feature = "au", target_os = "macos")))]
mod audio_unit {
    use super::{MainThreadMarker, Measuring, Path, PluginFormat, ProbeRefusal, Read};

    pub(super) fn run(
        _main: MainThreadMarker,
        _path: &Path,
        _measuring: Option<Measuring<'_>>,
    ) -> Read {
        Err(ProbeRefusal::FormatNotCompiled(PluginFormat::AudioUnit))
    }
}

/// Whether the plugin `id` is the one `state` is for: the one named, or, with
/// none named, the file's only plugin (the caller has checked there is one).
fn is_for(state: &StateFor<'_>, id: &str) -> bool {
    state.plugin.is_none_or(|wanted| wanted == id)
}

/// Describes the file's plugins as `L`, and, when `measuring`, measures each one
/// with one load of its own.
fn run<L: InProcess + Describe>(
    main: MainThreadMarker,
    path: &Path,
    measuring: Option<Measuring<'_>>,
) -> Read {
    let mut plugins = L::describe(main, path).map_err(|e| refusal(path, e))?;
    let Some((state, run)) = measuring else {
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
        let state = state.filter(|s| is_for(s, &plugin.id)).map(|s| s.bytes);
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
