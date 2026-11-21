//! The subprocess probe, as the probe `tutti-plugin-db` scans with.

use crate::probe::{ProbeState, measure_metadata_with, probe_metadata_with};
use crate::scanner::ProbeFuture;
use crate::{CatalogConfig, ProbeFailure};
use std::path::Path;
use std::time::Duration;
use tutti_platform::WeakExecutor;
use tutti_plugin_types::Locator;
use tutti_plugin_types::PluginFormat;

/// How long a measuring probe may take, spawn, load and both renders included
/// (two configurations, each up to the render budget of `MAX_NODE_TAIL` and 5 s of
/// audio, which a plugin slower than real time cannot meet in 15 s).
pub const DEFAULT_MEASURE_TIMEOUT: Duration = Duration::from_secs(180);

/// What a probe the catalog hands its scanner does.
#[derive(Debug, Clone)]
pub enum Probing {
    /// Read each plugin's descriptor; a missing `plugin-probe` is a failure of
    /// the file.
    Describe,
    /// Also render each plugin ([`DEFAULT_MEASURE_TIMEOUT`]); a missing
    /// `plugin-probe` is a failure of the file as well.
    Measure {
        /// The state a measurement puts one plugin in first.
        state: Option<ProbeState>,
    },
}

/// The probe a scanner calls, as `config` and `how` say: the binary
/// `config.probe_locator` names and the deadlines `config` holds. Its future
/// owns what it needs, so it outlives the call's borrows.
pub(crate) fn probe_fn(
    config: &CatalogConfig,
    how: Probing,
) -> impl Fn(&WeakExecutor, &Path, PluginFormat) -> ProbeFuture + Send + Sync + 'static {
    let locator: Locator = config.probe_locator.clone();
    let timeout = match how {
        Probing::Describe => config.probe_timeout,
        Probing::Measure { .. } => config.measure_timeout,
    };
    move |executor, path, format| {
        let (executor, locator, how, path) = (
            executor.clone(),
            locator.clone(),
            how.clone(),
            path.to_path_buf(),
        );
        Box::pin(async move {
            match &how {
                Probing::Describe => {
                    probe_metadata_with(&executor, &locator, &path, format, timeout).await
                }
                Probing::Measure { state } => {
                    measure_metadata_with(
                        &executor,
                        &locator,
                        &path,
                        format,
                        timeout,
                        state.as_ref(),
                    )
                    .await
                }
            }
            .map_err(ProbeFailure::from)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::ProbeError;

    /// **A probe with no `plugin-probe` fails the file, whether it describes or
    /// measures**: nothing else can look at the plugin, so the file is neither
    /// listed by its name nor blacklisted, and is retried once the probe is found.
    #[test]
    fn a_probe_without_a_probe_binary_fails_the_file() {
        let config =
            CatalogConfig::new("/pedal").probe_locator(Locator::at("/definitely/not/plugin-probe"));
        let path = Path::new("/plugins/Plain.vst3");
        let executor = tutti_fixtures::executor().downgrade();

        for how in [Probing::Describe, Probing::Measure { state: None }] {
            let probe = probe_fn(&config, how.clone());
            let failure =
                futures_lite::future::block_on(probe(&executor, path, PluginFormat::Vst3))
                    .expect_err("nothing else can look at the plugin");
            assert_eq!(failure.error, ProbeError::ProbeNotFound, "{how:?}");
            assert!(!failure.blacklistable(), "{how:?}");
        }
    }
}
