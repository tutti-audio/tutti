//! Reading a file through the host of its format, built only with a format to host.

use super::{MainThreadMarker, Measuring, Read, StateFor, routing};
use std::path::Path;
use tutti_platform::Instant;
use tutti_plugin_types::{
    Describe, Described, InProcess, MeasureFailure, PluginError, PluginFormat, PluginId, PluginRef,
    ProbeRefusal, Unmeasured, Widths,
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
fn is_for(state: &StateFor<'_>, id: &PluginId) -> bool {
    state.plugin.is_none_or(|wanted| wanted == id)
}

/// The audio widths of the plugin `id` of the file at `path`, read from a load of
/// it as `L` (no activation): `L`'s own [`tutti_plugin_types::Loaded::widths`], the answer an
/// activation negotiates against. `None` when the plugin does not load, so the
/// listing keeps it with its widths unread.
fn widths_of<L: InProcess>(main: MainThreadMarker, path: &Path, id: &PluginId) -> Option<Widths> {
    // The plugin closes on this thread when `loaded` drops, so the collector it is
    // given never receives work.
    let (_collector, work) = tutti_plugin_types::MainWork::new();
    L::load_in_process(main, &work, &PluginRef::new(path, id.clone()))
        .ok()
        .map(|loaded| loaded.widths())
}

/// Describes the file's plugins as `L`, reads each one's widths with one load of
/// its own, and, when `measuring`, measures each one with another.
fn run<L: InProcess + Describe>(
    main: MainThreadMarker,
    path: &Path,
    measuring: Option<Measuring<'_>>,
) -> Read {
    let mut plugins = L::describe(main, path).map_err(|e| refusal(path, e))?;
    for (id, descriptor) in &mut plugins {
        descriptor.widths = widths_of::<L>(main, path, id);
    }
    let Some((state, run)) = measuring else {
        return Ok((listed(plugins), Vec::new()));
    };
    match state.map(|s| s.plugin) {
        Some(Some(wanted)) if !plugins.iter().any(|(id, _)| id == wanted) => {
            return Err(ProbeRefusal::UnknownPlugin(wanted.clone()));
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
    for (id, descriptor) in &mut plugins {
        // A spent budget loads nothing more: the plugin is not loaded, activated
        // or rendered, and a state meant for it is not applied.
        if run.passed(Instant::now()) {
            unmeasured.push(Unmeasured {
                plugin: id.clone(),
                failure: MeasureFailure::BudgetSpent,
            });
            continue;
        }
        let state = state.filter(|s| is_for(s, id)).map(|s| s.bytes);
        match measure::<L>(main, path, id, state, run) {
            Ok(measured) => descriptor.measured = Some(measured),
            Err(MeasureFailure::StateRefused { reason }) => {
                return Err(ProbeRefusal::StateRefused {
                    plugin: id.clone(),
                    reason,
                });
            }
            Err(failure) => unmeasured.push(Unmeasured {
                plugin: id.clone(),
                failure,
            }),
        }
    }
    Ok((listed(plugins), unmeasured))
}

/// The plugins as the result file lists them: each with the id its format named.
fn listed(plugins: Vec<(PluginId, tutti_plugin_types::PluginDescriptor)>) -> Vec<Described> {
    plugins
        .into_iter()
        .map(|(id, descriptor)| Described::new(id, descriptor))
        .collect()
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
