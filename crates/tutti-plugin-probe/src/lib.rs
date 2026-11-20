#![doc = include_str!("../README.md")]
//! What `plugin-probe` does: read the descriptor of every plugin the file at a path
//! holds, as the format its extension names, through that format's host
//! ([`Describe`](tutti_plugin_types::Describe)), and with [`describe_measured`] measure each one ([`InProcess`](tutti_plugin_types::InProcess)).
//!
//! The binary (`src/main.rs`) is the command line around them.

#![forbid(unsafe_code)]

use std::path::Path;
use tutti_plugin_types::{
    Deadline, Described, MainThreadMarker, PluginFormat, PluginId, ProbeRefusal, Unmeasured,
};

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
) -> Result<Vec<Described>, ProbeRefusal> {
    with_host(main, path, format, None).map(|(plugins, _)| plugins)
}

/// Reads the audio widths of each plugin in `plugins`, as `format` or, with
/// none, as the file's extension says, and sets
/// [`PluginDescriptor::widths`](tutti_plugin_types::PluginDescriptor::widths):
/// one load of each plugin, without activating it, answered by the format
/// host's own [`Loaded::widths`](tutti_plugin_types::Loaded::widths). A plugin
/// that does not load keeps `None`.
///
/// **A listing never depends on a load.** [`describe`] and [`describe_measured`]
/// read the listing; the loads here come after it, and a caller writes the
/// listing out before it calls this. A plugin that crashes or hangs while it
/// loads takes this process with it, and the host that spawned it then reads
/// the listing without widths, never a probe that failed for the whole file.
/// A plugin that refuses to load is pinned by the shared VST3 plugin's class
/// that refuses `initialize`; a crash or hang is pinned by the host-side
/// `a_listing_written_before_the_probe_died_is_the_result` and by review, since
/// no test plugin can be made to fail only at its creation.
pub fn read_widths(
    main: MainThreadMarker,
    path: &Path,
    format: Option<PluginFormat>,
    plugins: &mut [Described],
) {
    read_widths_with_host(main, path, format, plugins);
}

/// The plugins a measuring probe was asked about: which one takes the state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateFor<'a> {
    /// The plugin the state is for; `None` is the file's first plugin.
    pub plugin: Option<&'a PluginId>,
    /// The bytes the plugin's own `save_state` wrote.
    pub bytes: &'a [u8],
}

/// [`describe`], then measure each plugin the file holds: the descriptors come
/// back with [`PluginDescriptor::measured`](tutti_plugin_types::PluginDescriptor::measured) set where the plugin could be
/// measured, and the plugins that could not be come back beside them, each with
/// its [`MeasureFailure`](tutti_plugin_types::MeasureFailure).
///
/// `state` puts one plugin in a state before it is measured (bytes the plugin's
/// own `save_state` wrote): the plugin it names, or the file's only plugin when it
/// names none. The others are measured as they load, and without `state` every
/// plugin is. `run` is the deadline of the whole run: each plugin's renders are
/// bounded by what it leaves, and once it has passed the plugins that remain are
/// reported [`MeasureFailure::BudgetSpent`](tutti_plugin_types::MeasureFailure::BudgetSpent) without being loaded.
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
) -> Read {
    with_host(main, path, format, Some((state, run)))
}

/// What the probe was asked beyond a description: the state for one plugin, and
/// the deadline of the whole run.
type Measuring<'a> = (Option<StateFor<'a>>, Deadline);

/// What a read of a file answers: its plugins, and those that could not be measured.
type Read = Result<(Vec<Described>, Vec<Unmeasured>), ProbeRefusal>;

/// The one module that differs by build: with a format to host it reads files; with
/// none (no format feature, or only `au` off macOS) every format is refused.
#[cfg(any(
    feature = "clap",
    feature = "vst3",
    feature = "vst2",
    all(feature = "au", target_os = "macos")
))]
mod hosted;
#[cfg(not(any(
    feature = "clap",
    feature = "vst3",
    feature = "vst2",
    all(feature = "au", target_os = "macos")
)))]
mod hosted {
    use super::{
        Described, MainThreadMarker, Measuring, Path, PluginFormat, ProbeRefusal, Read, routing,
    };

    pub(super) fn read_widths(
        _main: MainThreadMarker,
        _path: &Path,
        _format: Option<PluginFormat>,
        _plugins: &mut [Described],
    ) {
    }

    pub(super) fn with_host(
        _main: MainThreadMarker,
        path: &Path,
        format: Option<PluginFormat>,
        _measuring: Option<Measuring<'_>>,
    ) -> Read {
        Err(match routing(path, format) {
            Some(format) => ProbeRefusal::FormatNotCompiled(format),
            None => ProbeRefusal::UnsupportedFormat,
        })
    }
}

use hosted::{read_widths as read_widths_with_host, with_host};
