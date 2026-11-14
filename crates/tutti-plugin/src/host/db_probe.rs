//! The subprocess probe, as the probe `tutti-plugin-db` scans with.

pub use super::probe::DEFAULT_PROBE_TIMEOUT;
use super::probe::{ProbeError, measure_metadata_with, probe_metadata_with};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::debug;
use tutti_plugin_db::{
    Cancel, PluginFormat, ProbeFailure, ProbeFailureKind, probe_plugin_fallback,
};
use tutti_plugin_ipc::CatalogConfig;
use tutti_plugin_types::BridgeError;
use tutti_plugin_types::Locator;
use tutti_plugin_types::PluginDescriptor;

/// How long a measuring probe may take, spawn, load and both renders included
/// (two configurations, each up to the render budget of `MAX_NODE_TAIL` and 5 s of
/// audio, which a plugin slower than real time cannot meet in 15 s).
pub const DEFAULT_MEASURE_TIMEOUT: Duration = Duration::from_secs(180);

/// What a probe the catalog hands its scanner does.
#[derive(Debug, Clone, Default)]
pub(crate) struct Probing {
    /// Also render each plugin ([`DEFAULT_MEASURE_TIMEOUT`]).
    pub(crate) measure: bool,
    /// The state file a measurement puts the plugin in first.
    pub(crate) state: Option<PathBuf>,
    /// Answer with filename metadata when `plugin-probe` cannot be found. A scan
    /// lists what it found by file name then; a task that was asked about one
    /// file reports the probe's absence instead.
    pub(crate) fallback: bool,
}

/// The probe a scanner calls, as `config` and `how` say: the binary
/// `config.probe_locator` names, the deadlines `config` gives or the defaults.
pub(crate) fn probe_fn(
    config: &CatalogConfig,
    how: Probing,
) -> impl Fn(&Path, PluginFormat, &Cancel) -> Result<Vec<PluginDescriptor>, ProbeFailure>
+ Send
+ Sync
+ 'static {
    let locator: Locator = config.probe_locator.clone();
    let timeout = if how.measure {
        config.measure_timeout.unwrap_or(DEFAULT_MEASURE_TIMEOUT)
    } else {
        config.probe_timeout.unwrap_or(DEFAULT_PROBE_TIMEOUT)
    };
    move |path, format, cancel| {
        let probed = if how.measure {
            measure_metadata_with(&locator, path, timeout, cancel, how.state.as_deref())
        } else {
            probe_metadata_with(&locator, path, timeout, cancel)
        };
        interpret_probe(probed, path, format, how.fallback)
    }
}

/// The blacklist decision for a failed probe: whether the scan may blacklist
/// the plugin, and why. One function, so the tests exercise this code rather
/// than a copy of it.
///
/// The split is "did we learn something about the plugin, or about our own
/// environment?" — not severity. A crash and a broken library are equally
/// informative; a missing `plugin-probe` tells us nothing.
#[must_use]
pub(crate) fn probe_failure(e: BridgeError) -> ProbeFailure {
    let (kind, reason) = match &e {
        BridgeError::ProcessCrashed => (
            ProbeFailureKind::Crashed,
            format!("crashed during probe: {e}"),
        ),
        BridgeError::Timeout { .. } => (
            ProbeFailureKind::TimedOut,
            format!("timed out during probe: {e}"),
        ),
        // The plugin was reached and its library would not load — a stub
        // file, a wrong-arch binary, a broken install. Deterministic, and each
        // retry costs a full subprocess spawn. Carries the loader's own stage
        // and reason so the catalog entry says *why*.
        BridgeError::LoadFailed { .. } => (
            ProbeFailureKind::LoadFailed,
            format!("failed to load during probe: {e}"),
        ),
        BridgeError::ProbeNotFound => (ProbeFailureKind::ProbeNotFound, e.to_string()),
        // IO, a protocol mismatch — the environment's fault, not the plugin's.
        _ => (ProbeFailureKind::Environment, e.to_string()),
    };
    ProbeFailure { reason, kind }
}

/// Decide what a raw probe result means. Split from [`probe_fn`] so the
/// fallback rule is testable without a subprocess — a test that spawns a real
/// probe depends on whether `target/debug/` happens to be warm.
fn interpret_probe(
    result: Result<Vec<PluginDescriptor>, ProbeError>,
    path: &Path,
    format: PluginFormat,
    fallback: bool,
) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
    match result {
        Ok(plugins) => Ok(plugins),
        Err(ProbeError::Cancelled) => Err(ProbeFailure {
            reason: "the probe was cancelled".to_string(),
            kind: ProbeFailureKind::Cancelled,
        }),
        // The only error that means "we could not look", as opposed to "we looked
        // and this plugin is bad". Widening it would hide real failures.
        Err(ProbeError::Bridge(BridgeError::ProbeNotFound)) if fallback => {
            debug!(
                "plugin-probe not available, using filename metadata for {:?}",
                path
            );
            Ok(vec![probe_plugin_fallback(path, format)])
        }
        Err(ProbeError::Bridge(e)) => Err(probe_failure(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tutti_plugin_types::PluginClass;

    /// A failure that is a property of the plugin must earn a catalog entry, not
    /// just a `warn!` — without one `needs_rescan` stays true and the plugin is
    /// re-probed at full cost (the crash, the 5 s stall, the failing `dlopen`) on
    /// every scan, forever.
    ///
    /// The dividing line is whether the failure taught us about the *plugin* or
    /// about the *environment*, not severity: a broken library is as informative
    /// as a crash, while a missing `plugin-probe` is not informative at all.
    #[test]
    fn plugin_failures_are_blacklistable_environmental_ones_are_not() {
        use tutti_plugin_types::BridgeError;

        // Exercises the *production* classifier, not a copy of it.
        let crashed = probe_failure(BridgeError::ProcessCrashed);
        assert!(
            crashed.blacklistable(),
            "ProcessCrashed must be blacklistable"
        );
        assert!(crashed.reason.contains("crashed"));

        let timed_out = probe_failure(BridgeError::Timeout {
            operation: "probe".into(),
            duration_ms: 5000,
            // Irrelevant to the blacklist verdict, which is what this asserts:
            // a probe that timed out is unusable either way.
            partial: false,
        });
        assert!(timed_out.blacklistable(), "Timeout must be blacklistable");
        assert!(timed_out.reason.contains("timed out"));

        // A load failure was the one path that recorded nothing.
        let load_failed = probe_failure(BridgeError::LoadFailed {
            path: PathBuf::from("/plugins/Broken.vst3"),
            stage: tutti_plugin_types::LoadStage::Opening,
            reason: "dlopen failed".into(),
        });
        assert!(
            load_failed.blacklistable(),
            "a library that will not open fails identically on every scan, at a \
             full subprocess spawn each time — it must be recorded"
        );
        assert!(
            load_failed.reason.contains("dlopen failed"),
            "the loader's own reason must survive into the catalog entry, or the \
             user sees a hidden plugin with no explanation: {}",
            load_failed.reason
        );

        // Environmental failures say nothing about the plugin — never hide it.
        assert!(!probe_failure(BridgeError::IpcError("socket".into())).blacklistable());
        assert!(
            !probe_failure(BridgeError::ProtocolMismatch {
                expected: 2,
                got: 1
            })
            .blacklistable()
        );
        assert!(
            !probe_failure(BridgeError::ProbeNotFound).blacklistable(),
            "a missing plugin-probe must never blacklist a plugin"
        );
    }

    /// A missing `plugin-probe` must degrade to filename metadata rather than
    /// fail, and must report `Unknown` when it does: name and id come from the
    /// filename, but nothing inspected the plugin, so claiming a category would be
    /// a fabrication.
    ///
    /// Driven through [`interpret_probe`] rather than by arranging for the probe
    /// to be absent. An earlier version called `probe_plugin` and relied on the
    /// binary not being built — but the search covers
    /// `current_exe().parent().parent()`, which for a test binary in
    /// `target/debug/deps/` is `target/debug/`, where the binary lands as soon as
    /// anything in the workspace builds it. So it passed on a cold target dir and
    /// failed on a warm one, having found a real binary that then reported
    /// `LoadFailed` on the fake plugin. `TUTTI_PLUGIN_PROBE` is no lever either:
    /// a nonexistent path there warns and falls through to the same search.
    #[test]
    fn probe_without_a_probe_binary_falls_back_to_filename_metadata() {
        use tutti_plugin_types::BridgeError;

        let path = Path::new("/plugins/Plain.vst3");
        let plugins = interpret_probe(
            Err(BridgeError::ProbeNotFound.into()),
            path,
            PluginFormat::Vst3,
            true,
        )
        .expect("a missing plugin-probe is a fallback, not an error");
        let [descriptor] = plugins.as_slice() else {
            panic!("the filename names one plugin: {plugins:?}");
        };

        assert_eq!(descriptor.name, "Plain");
        assert_eq!(descriptor.id, "vst3.plain");
        assert!(
            matches!(descriptor.class, PluginClass::Unknown),
            "an unprobed plugin must not claim a category, got {:?}",
            descriptor.class
        );
    }

    /// The counterpart: only `ProbeNotFound` falls back. A load failure is a real
    /// answer about a real plugin and must surface as an error, or a broken plugin
    /// would be silently catalogued under its filename as though it had been probed.
    #[test]
    fn a_failed_load_is_an_error_not_a_filename_fallback() {
        use tutti_plugin_types::BridgeError;

        let path = Path::new("/plugins/Broken.vst3");
        let failure = interpret_probe(
            Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: tutti_plugin_types::LoadStage::Opening,
                reason: "dlopen failed".into(),
            }
            .into()),
            path,
            PluginFormat::Vst3,
            true,
        )
        .expect_err("a load failure must not be reported as successful metadata");

        assert!(failure.reason.contains("dlopen failed"));
    }

    /// A task about one file reports a missing `plugin-probe`, as a kind a caller
    /// can tell from a crash or a hang, rather than answering with a filename.
    #[test]
    fn a_missing_probe_is_a_failure_of_its_own_kind_without_the_fallback() {
        let failure = interpret_probe(
            Err(BridgeError::ProbeNotFound.into()),
            Path::new("/plugins/Plain.vst3"),
            PluginFormat::Vst3,
            false,
        )
        .expect_err("no fallback was asked for");

        assert_eq!(failure.kind, ProbeFailureKind::ProbeNotFound);
        assert!(!failure.blacklistable());
    }
}
