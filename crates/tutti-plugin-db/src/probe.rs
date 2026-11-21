//! One-shot metadata probe: spawn `plugin-probe`, wait for it, read its result
//! file (the descriptor of every plugin the file holds). A timeout kills the child,
//! and so does dropping the probe's future.
//!
//! The wire is the file (`tutti_plugin_types::probe_wire`), not a socket or a
//! stream. The child's stdout and stderr go to the null device, because plugins
//! print from their init code. With the result file the outcomes are
//! unambiguous: a file means the probe finished, with a descriptor or a typed
//! refusal; no file and an exit means it died, and its [`ExitStatus`](std::process::ExitStatus) says how
//! (a signal, a panic, one of its own failing codes, a plugin's `exit`); no file
//! at the deadline means it hung. The host states its pid to the child
//! ([`HOST_PID_ENV`](tutti_plugin_ipc_ffi::HOST_PID_ENV)), which ends itself with the host.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tutti_platform::WeakExecutor;
use tutti_plugin_types::{Described, Locator, PluginFormat, PluginId, ProbeError};

// The one place the platform is chosen: a target that starts no processes answers every
// probe as unsupported.
#[cfg_attr(not(target_arch = "wasm32"), path = "probe/process.rs")]
#[cfg_attr(target_arch = "wasm32", path = "probe/unsupported.rs")]
mod os;

/// How long a probe may take, spawn and the plugin's load included, unless the
/// caller says otherwise. A cold plugin can take seconds to `dlopen`; the old
/// socket probe allowed about twelve.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// What a measuring probe leaves of the host's deadline for what is not rendering:
/// the result's write and the host's own wake. The most of it, and at most a
/// quarter of a short deadline.
const BUDGET_MARGIN: Duration = Duration::from_secs(10);

/// The probe's run budget (`--budget`) under the host's deadline `timeout`: the
/// deadline less [`BUDGET_MARGIN`] (a quarter of it, if that is less), so each
/// plugin's renders time out on their own rows before the host gives up on the
/// whole file and kills the probe.
fn run_budget(timeout: Duration) -> Duration {
    timeout - BUDGET_MARGIN.min(timeout / 4)
}

/// The state a measuring probe puts one plugin in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeState {
    /// A file of the bytes the plugin's own `save_state` wrote.
    pub file: PathBuf,
    /// The plugin the state is for; `None` is the file's first plugin.
    pub plugin: Option<PluginId>,
}

/// Reads the descriptor of every plugin a file holds in a `plugin-probe` subprocess, so a plugin that
/// crashes or hangs in its init does not take the caller down. `locator` names
/// the binary (a test passes [`Locator::at`], so none writes the
/// process-global environment) and `timeout` is the deadline.
///
/// # Errors
///
/// [`ProbeError::ProbeNotFound`] if the binary cannot be located;
/// [`ProbeError::Refused`] if the probe ran to its end and refused the plugin;
/// [`ProbeError::Died`] if it ended without a result, by the status it names;
/// [`ProbeError::TimedOut`] if it did not finish within `timeout` (it is killed);
/// [`ProbeError::ShutDown`] if the executor shut down while it ran (it is killed).
///
/// Dropping the future kills and reaps the child.
pub(crate) async fn probe_metadata_with(
    executor: &WeakExecutor,
    locator: &Locator,
    plugin_path: &Path,
    format: PluginFormat,
    timeout: Duration,
) -> Result<Vec<Described>, ProbeError> {
    os::run(executor, locator, plugin_path, format, timeout, &[]).await
}

/// [`probe_metadata_with`] that also **measures** each plugin (`plugin-probe
/// --measure`): the descriptors come back with
/// [`PluginDescriptor::measured`](tutti_plugin_types::PluginDescriptor::measured) set where the plugin could be measured, and a
/// plugin that could not be is logged with why. It renders the plugin, so give it
/// the long deadline
/// ([`DEFAULT_MEASURE_TIMEOUT`](crate::DEFAULT_MEASURE_TIMEOUT)):
/// a plugin that crashes or hangs while rendering is [`ProbeError::Died`] or
/// [`ProbeError::TimedOut`], as in the load. The probe bounds each plugin's render
/// itself, so a slow plugin loses its own row and not the file's.
///
/// `state` puts one plugin in the state a file holds before the measurement
/// (`--state`, `--plugin`); the record carries a hash of it.
///
/// The probe is given one budget for its whole run, `timeout` less a margin
/// (`--budget`), which the plugins of a file share: a plugin that
/// is slow, or one that follows slow ones, times out its own rows (logged, no
/// measurement) while the file's others keep theirs, and the probe ends before
/// `timeout` and is not killed as a hang. A plugin that never returns from a block
/// is still the host's `timeout`, and the whole file.
///
/// # Errors
///
/// As [`probe_metadata_with`]; a plugin that refuses `state` is
/// [`ProbeRefusal::StateRefused`](tutti_plugin_types::ProbeRefusal::StateRefused).
pub(crate) async fn measure_metadata_with(
    executor: &WeakExecutor,
    locator: &Locator,
    plugin_path: &Path,
    format: PluginFormat,
    timeout: Duration,
    state: Option<&ProbeState>,
) -> Result<Vec<Described>, ProbeError> {
    let budget = run_budget(timeout).as_secs().to_string();
    let mut args = vec!["--measure".as_ref(), "--budget".as_ref(), budget.as_ref()];
    // The command line carries the id as text (`PluginId`'s `Display`), which
    // the probe reads back with `FromStr`.
    let chosen = state
        .and_then(|state| state.plugin.as_ref())
        .map(ToString::to_string);
    if let Some(state) = state {
        args.push("--state".as_ref());
        args.push(state.file.as_os_str());
        if let Some(plugin) = &chosen {
            args.push("--plugin".as_ref());
            args.push(plugin.as_ref());
        }
    }
    os::run(executor, locator, plugin_path, format, timeout, &args).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use tempfile::TempDir;
    use tutti_plugin_ipc_ffi::HOST_PID_ENV;
    use tutti_plugin_types::{ProbeDeath, ProbeExit, ResultError};

    /// A `plugin-probe` stand-in: a shell script given the plugin path as `$1`
    /// and the result file as `$2`, as the real one is.
    fn stand_in(dir: &TempDir, body: &str) -> Locator {
        let path = dir.path().join("plugin-probe");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Locator::at(path)
    }

    fn probe(locator: &Locator, timeout: Duration) -> Result<Vec<Described>, ProbeError> {
        let executor = tutti_fixtures::executor().downgrade();
        futures_lite::future::block_on(probe_metadata_with(
            &executor,
            locator,
            Path::new("/plugins/Some.clap"),
            PluginFormat::Clap,
            timeout,
        ))
    }

    /// **Dropping the probe's future kills and reaps its child**: the future loses a
    /// race to the child's pid arriving, and the child is gone when the race ends.
    #[test]
    fn a_dropped_probe_is_killed_and_reaped() {
        let dir = TempDir::new().unwrap();
        let fifo = dir.path().join("pid");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success(),
            "mkfifo"
        );
        let locator = stand_in(
            &dir,
            &format!("echo $$ > {}\nexec sleep 300", fifo.display()),
        );
        // Opening and reading the fifo block until the child is running and has
        // written its pid.
        let (sent, arrived) = futures_channel::oneshot::channel();
        let reader = tutti_platform::spawn_named("probe-pid", None, move |_| {
            let pid = std::fs::read_to_string(&fifo).expect("the child writes its pid");
            let _ = sent.send(pid);
        })
        .expect("the reader starts");

        let executor = tutti_fixtures::executor().downgrade();
        let pid = futures_lite::future::block_on(futures_lite::future::or(
            async {
                let result = probe_metadata_with(
                    &executor,
                    &locator,
                    Path::new("/plugins/Some.clap"),
                    PluginFormat::Clap,
                    Duration::from_secs(300),
                )
                .await;
                panic!("the probe ended before it was dropped: {result:?}");
            },
            async { arrived.await.expect("the reader sends") },
        ));
        reader.join().unwrap();

        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(
            !alive,
            "the dropped probe (pid {}) is still running",
            pid.trim()
        );
    }

    /// **No result file and an exit is a death the host tells by its status**: a
    /// signal, a panic's status, a code of the probe's own and a plugin's `exit` are
    /// four different ends, and the probe's own failing codes are its own.
    #[test]
    fn exiting_without_a_result_is_told_by_its_status() {
        let cases = [
            ("exit 7", ProbeDeath::Status(7)),
            ("exit 0", ProbeDeath::Exited(ProbeExit::Written)),
            ("exit 75", ProbeDeath::Exited(ProbeExit::ResultUnwritable)),
            // A plugin's own `exit`, by the codes the probe once used.
            ("exit 2", ProbeDeath::Status(2)),
            ("exit 3", ProbeDeath::Status(3)),
            ("exit 101", ProbeDeath::Panicked),
            ("kill -SEGV $$", ProbeDeath::Signal(11)),
        ];
        for (body, death) in cases {
            let dir = TempDir::new().unwrap();
            let result = probe(&stand_in(&dir, body), Duration::from_secs(10));
            assert_eq!(result.unwrap_err(), ProbeError::Died(death), "`{body}`");
        }
    }

    /// **A listing the probe wrote survives the probe's death after it**: the
    /// probe writes the listing, then loads each plugin for its widths, and a
    /// plugin that crashes or hangs there ends the process. The host reads the
    /// result file whatever the exit, so the plugins still list, with the widths
    /// that load would have read left unread.
    #[test]
    fn a_listing_written_before_the_probe_died_is_the_result() {
        use tutti_plugin_types::{ProbeOutcome, write_result};
        let listed = vec![Described::new(
            PluginId::Clap("a.plugin".into()),
            tutti_plugin_types::PluginDescriptor::new(
                "A",
                tutti_plugin_types::PluginClass::Unknown,
            ),
        )];
        // A crash ends the probe by itself, so its window only guards a hang; a hang
        // ends at the window, so it is short.
        for (end, window) in [
            ("kill -SEGV $$", Duration::from_secs(60)),
            ("exec sleep 300", Duration::from_secs(2)),
        ] {
            let dir = TempDir::new().unwrap();
            let prepared = dir.path().join("listing");
            write_result(
                &prepared,
                &ProbeOutcome::Described {
                    plugins: listed.clone(),
                    unmeasured: Vec::new(),
                },
            )
            .unwrap();
            let locator = stand_in(&dir, &format!("cp {} \"$2\"\n{end}", prepared.display()));
            let result = probe(&locator, window);
            assert_eq!(result, Ok(listed.clone()), "after `{end}`");
        }
    }

    /// **The host states its pid to the probe**, as it does to a plugin-server, so
    /// the probe watches the host and not whatever its parent is by then.
    #[test]
    fn the_probe_is_told_the_hosts_pid() {
        let dir = TempDir::new().unwrap();
        let seen = dir.path().join("seen");
        let locator = stand_in(
            &dir,
            &format!("echo \"${HOST_PID_ENV}\" > {}", seen.display()),
        );
        let _ = probe(&locator, Duration::from_secs(10));
        let told = std::fs::read_to_string(&seen).unwrap();
        assert_eq!(told.trim(), std::process::id().to_string());
    }

    /// **A partial result is never read as a result**: a staging file left by a
    /// probe that died mid-write is not one (it is a crash), and a cut-short
    /// file at the result path is refused as a protocol fault, not decoded.
    #[test]
    fn a_partial_result_file_is_not_a_result() {
        let dir = TempDir::new().unwrap();
        let staged = stand_in(&dir, "printf 'TPRB\\001' > \"$2.tmp\"");
        let result = probe(&staged, Duration::from_secs(10));
        assert_eq!(
            result.unwrap_err(),
            ProbeError::Died(ProbeDeath::Exited(ProbeExit::Written))
        );

        let dir = TempDir::new().unwrap();
        let cut = stand_in(&dir, "printf 'TPR' > \"$2\"");
        let result = probe(&cut, Duration::from_secs(10));
        assert_eq!(
            result.unwrap_err(),
            ProbeError::Result(ResultError::Truncated)
        );
    }

    /// A probe binary that is not there is `ProbeNotFound`, the one error the
    /// scan answers with filename metadata.
    #[test]
    fn a_missing_probe_binary_is_probe_not_found() {
        let missing = Locator::at("/definitely/not/plugin-probe");
        assert_eq!(
            probe(&missing, Duration::from_secs(1)).unwrap_err(),
            ProbeError::ProbeNotFound
        );
    }

    /// The deadline is the caller's: a probe that does not exit times out under
    /// a deadline already up, and one that exits is a plain crash under a long one.
    #[test]
    fn the_deadline_is_the_callers() {
        let dir = TempDir::new().unwrap();
        let hangs = stand_in(&dir, "exec sleep 300");
        let up = probe(&hangs, Duration::ZERO);
        assert_eq!(
            up.unwrap_err(),
            ProbeError::TimedOut {
                after: Duration::ZERO
            }
        );

        let dir = TempDir::new().unwrap();
        let exits = stand_in(&dir, "exit 1");
        let long = probe(&exits, Duration::from_secs(10));
        assert_eq!(long.unwrap_err(), ProbeError::Died(ProbeDeath::Status(1)));
    }

    /// The run budget leaves a margin under the host's deadline, so a probe whose
    /// plugins are slow finishes before the host kills it.
    #[test]
    fn the_run_budget_is_the_deadline_less_a_margin() {
        let secs = Duration::from_secs;
        assert_eq!(run_budget(secs(180)), secs(170));
        assert_eq!(run_budget(secs(15)), secs(15) - secs(15) / 4);
        assert_eq!(run_budget(secs(4)), secs(3));
        assert_eq!(run_budget(Duration::ZERO), Duration::ZERO);
        for timeout in [secs(1), secs(15), secs(180), secs(3600)] {
            assert!(run_budget(timeout) < timeout, "{timeout:?}");
        }
    }

    /// The result file is in a directory of the probe's own, not a predictable
    /// name in the shared temp directory, and the directory is gone after.
    #[test]
    fn the_result_lives_in_a_private_directory_that_is_removed() {
        let dir = TempDir::new().unwrap();
        let seen = dir.path().join("seen");
        let locator = stand_in(&dir, &format!("echo \"$2\" > {}", seen.display()));
        let _ = probe(&locator, Duration::from_secs(10));
        let result = PathBuf::from(std::fs::read_to_string(&seen).unwrap().trim());
        let parent = result.parent().unwrap();
        assert_ne!(parent, std::env::temp_dir(), "a shared directory");
        assert!(
            parent
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("tutti-probe-")
        );
        assert!(!parent.exists(), "the probe's directory outlived it");
    }
}
