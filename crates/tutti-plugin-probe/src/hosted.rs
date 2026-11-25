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

/// [`fill_widths`] with the host of the format the file is read as. A format
/// not compiled in has nothing to load, so its plugins keep their widths unread.
pub(super) fn read_widths(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
    plugins: &mut [Described],
) {
    match routing(path, format) {
        #[cfg(feature = "vst3")]
        Some(PluginFormat::Vst3) => fill_widths::<tutti_vst3_host::Vst3Loaded>(main, path, plugins),
        #[cfg(feature = "clap")]
        Some(PluginFormat::Clap) => fill_widths::<tutti_clap_host::ClapLoaded>(main, path, plugins),
        #[cfg(feature = "vst2")]
        Some(PluginFormat::Vst2) => fill_widths::<tutti_vst2_host::Vst2Loaded>(main, path, plugins),
        Some(PluginFormat::AudioUnit) => audio_unit::widths(main, path, plugins),
        _ => {}
    }
}

/// Audio Units are macOS's alone: the one module that differs by platform.
#[cfg(all(feature = "au", target_os = "macos"))]
mod audio_unit {
    use super::{Described, MainThreadMarker, Measuring, Path, Read};

    pub(super) fn run(
        main: MainThreadMarker,
        path: &Path,
        measuring: Option<Measuring<'_>>,
    ) -> Read {
        super::run::<tutti_au_host::AuLoaded>(main, path, measuring)
    }

    pub(super) fn widths(main: MainThreadMarker, path: &Path, plugins: &mut [Described]) {
        super::fill_widths::<tutti_au_host::AuLoaded>(main, path, plugins);
    }
}

/// Without macOS or the feature, the format is not compiled in.
#[cfg(not(all(feature = "au", target_os = "macos")))]
mod audio_unit {
    use super::{Described, MainThreadMarker, Measuring, Path, PluginFormat, ProbeRefusal, Read};

    pub(super) fn run(
        _main: MainThreadMarker,
        _path: &Path,
        _measuring: Option<Measuring<'_>>,
    ) -> Read {
        Err(ProbeRefusal::FormatNotCompiled(PluginFormat::AudioUnit))
    }

    pub(super) fn widths(_main: MainThreadMarker, _path: &Path, _plugins: &mut [Described]) {}
}

/// Whether the plugin `id` is the one `state` is for: the one named, or, with
/// none named, the file's only plugin (the caller has checked there is one).
fn is_for(state: &StateFor<'_>, id: &PluginId) -> bool {
    state.plugin.is_none_or(|wanted| wanted == id)
}

/// The audio widths of the plugin `id` of the file at `path`, read from a load of
/// it as `L` (no activation): `L`'s own [`Loaded::widths`](tutti_plugin_types::Loaded::widths), the answer an
/// activation negotiates against. `None` when the plugin does not load, so the
/// listing keeps it with its widths unread.
fn widths_of<L: InProcess>(main: MainThreadMarker, path: &Path, id: &PluginId) -> Option<Widths> {
    // The plugin closes on this thread when `loaded` drops, so the collector it is
    // given never receives work.
    let (_collector, work) = tutti_plugin_types::MainWork::new();
    L::load_in_process(main, &work, &PluginRef::new(path, id.clone()))
        .ok()
        .map(|mut loaded| loaded.widths(main))
}

/// Reads the widths of each plugin of `plugins` as `L`, one load each.
fn fill_widths<L: InProcess>(main: MainThreadMarker, path: &Path, plugins: &mut [Described]) {
    for plugin in plugins {
        if let Some(id) = &plugin.id {
            plugin.descriptor.widths = widths_of::<L>(main, path, id);
        }
    }
}

/// Describes the file's plugins as `L` and, when `measuring`, measures each one
/// with one load of its own.
fn run<L: InProcess + Describe>(
    main: MainThreadMarker,
    path: &Path,
    measuring: Option<Measuring<'_>>,
) -> Read {
    let mut plugins = L::describe(main, path).map_err(|e| refusal(path, e))?;
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
