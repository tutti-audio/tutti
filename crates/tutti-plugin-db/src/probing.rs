//! The subprocess probe, as the probe `tutti-plugin-db` scans with.

use crate::Stop;
use crate::probe::{ProbeState, measure_metadata_with, probe_metadata_with};
use crate::{CatalogConfig, ProbeFailure, probe_plugin_fallback};
use std::path::Path;
use std::time::Duration;
use tracing::debug;
use tutti_plugin_types::Described;
use tutti_plugin_types::Locator;
use tutti_plugin_types::PluginFormat;
use tutti_plugin_types::ProbeError;

/// How long a measuring probe may take, spawn, load and both renders included
/// (two configurations, each up to the render budget of `MAX_NODE_TAIL` and 5 s of
/// audio, which a plugin slower than real time cannot meet in 15 s).
pub const DEFAULT_MEASURE_TIMEOUT: Duration = Duration::from_secs(180);

/// What a probe the catalog hands its scanner does.
#[derive(Debug, Clone)]
pub enum Probing {
    /// Read each plugin's descriptor.
    Describe {
        /// Answer with filename metadata when `plugin-probe` cannot be found. A
        /// scan lists what it found by file name then; a task that was asked
        /// about one file reports the probe's absence instead.
        fallback: bool,
    },
    /// Also render each plugin ([`DEFAULT_MEASURE_TIMEOUT`]); a missing
    /// `plugin-probe` is a failure, since nothing else can measure.
    Measure {
        /// The state a measurement puts one plugin in first.
        state: Option<ProbeState>,
    },
}

/// The probe a scanner calls, as `config` and `how` say: the binary
/// `config.probe_locator` names and the deadlines `config` holds.
pub fn probe_fn(
    config: &CatalogConfig,
    how: Probing,
) -> impl Fn(&Path, PluginFormat, &Stop) -> Result<Vec<Described>, ProbeFailure> + Send + Sync + 'static
{
    let locator: Locator = config.probe_locator.clone();
    let timeout = match how {
        Probing::Describe { .. } => config.probe_timeout,
        Probing::Measure { .. } => config.measure_timeout,
    };
    move |path, format, stop| match &how {
        Probing::Describe { fallback } => interpret_probe(
            probe_metadata_with(&locator, path, format, timeout, stop),
            path,
            *fallback,
        ),
        Probing::Measure { state } => interpret_probe(
            measure_metadata_with(&locator, path, format, timeout, stop, state.as_ref()),
            path,
            false,
        ),
    }
}

/// Decide what a raw probe result means. Split from [`probe_fn`] so the
/// fallback rule is testable without a subprocess — a test that spawns a real
/// probe depends on whether `target/debug/` happens to be warm.
fn interpret_probe(
    result: Result<Vec<Described>, ProbeError>,
    path: &Path,
    fallback: bool,
) -> Result<Vec<Described>, ProbeFailure> {
    match result {
        Ok(plugins) => Ok(plugins),
        // The only error that means "we could not look", as opposed to "we looked
        // and this plugin is bad". Widening it would hide real failures.
        Err(ProbeError::ProbeNotFound) if fallback => {
            debug!(
                "plugin-probe not available, using filename metadata for {:?}",
                path
            );
            Ok(vec![probe_plugin_fallback(path)])
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::{LoadStage, PluginClass, ProbeRefusal};

    /// A missing `plugin-probe` must degrade to filename metadata rather than
    /// fail, and must report `Unknown` when it does: name and id come from the
    /// filename, but nothing inspected the plugin, so claiming a category would be
    /// a fabrication.
    ///
    /// Driven through [`interpret_probe`] rather than by arranging for the probe
    /// to be absent: the search covers the running binary's directories and
    /// `PATH`, so whether it finds a built probe depends on the machine, and
    /// `TUTTI_PLUGIN_PROBE` is no lever either, since a nonexistent path there
    /// falls through to the same search.
    #[test]
    fn probe_without_a_probe_binary_falls_back_to_filename_metadata() {
        let path = Path::new("/plugins/Plain.vst3");
        let plugins = interpret_probe(Err(ProbeError::ProbeNotFound), path, true)
            .expect("a missing plugin-probe is a fallback, not an error");
        let [Described { id, descriptor }] = plugins.as_slice() else {
            panic!("the filename names one plugin: {plugins:?}");
        };

        assert_eq!(descriptor.name, "Plain");
        assert_eq!(id, &None, "no id is read from a file name");
        assert!(
            matches!(descriptor.class, PluginClass::Unknown),
            "an unprobed plugin must not claim a category, got {:?}",
            descriptor.class
        );
    }

    /// The counterpart: only `ProbeNotFound` falls back. A refusal is a real
    /// answer about a real plugin and must surface as an error, or a broken plugin
    /// would be silently catalogued under its filename as though it had been probed.
    #[test]
    fn a_refusal_is_an_error_not_a_filename_fallback() {
        let path = Path::new("/plugins/Broken.vst3");
        let refusal = ProbeRefusal::Load {
            stage: LoadStage::Opening,
            reason: "dlopen failed".into(),
        };
        let failure = interpret_probe(Err(refusal.clone().into()), path, true)
            .expect_err("a refusal must not be reported as successful metadata");

        assert_eq!(failure.error, ProbeError::Refused(refusal));
        assert!(failure.reason().contains("dlopen failed"));
    }

    /// A task about one file reports a missing `plugin-probe`, as a kind a caller
    /// can tell from a crash or a hang, rather than answering with a filename.
    #[test]
    fn a_missing_probe_is_a_failure_of_its_own_kind_without_the_fallback() {
        let failure = interpret_probe(
            Err(ProbeError::ProbeNotFound),
            Path::new("/plugins/Plain.vst3"),
            false,
        )
        .expect_err("no fallback was asked for");

        assert_eq!(failure.error, ProbeError::ProbeNotFound);
        assert!(!failure.blacklistable());
    }

    /// **A measuring probe with no `plugin-probe` fails the file**: nothing else
    /// can measure, so the file is neither listed by its name nor blacklisted,
    /// while a describing probe with the fallback lists it.
    #[test]
    fn a_measuring_probe_without_a_probe_binary_fails_the_file() {
        let config =
            CatalogConfig::new("/pedal").probe_locator(Locator::at("/definitely/not/plugin-probe"));
        let path = Path::new("/plugins/Plain.vst3");
        let stop = Stop::new();

        let measuring = probe_fn(&config, Probing::Measure { state: None });
        let failure = measuring(path, PluginFormat::Vst3, &stop)
            .expect_err("a measurement is not answered with filename metadata");
        assert_eq!(failure.error, ProbeError::ProbeNotFound);
        assert!(!failure.blacklistable());

        let describing = probe_fn(&config, Probing::Describe { fallback: true });
        let listed = describing(path, PluginFormat::Vst3, &stop)
            .expect("a describing scan lists the file by its name");
        assert_eq!(listed.len(), 1);
    }
}
