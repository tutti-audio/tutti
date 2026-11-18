//! One-shot metadata probe: spawn `plugin-probe`, wait for it, read its result
//! file (the descriptor of every plugin the file holds). A timeout kills the child.
//!
//! The wire is the file (`tutti_plugin_types::probe_wire`), not a socket or a
//! stream. The child's stdout and stderr go to the null device, because plugins
//! print from their init code. With the result file the outcomes are
//! unambiguous: a file means the probe finished, with a descriptor or the
//! loader's own failure; no file and a signal or a non-zero exit means it
//! crashed; no file at the deadline means it hung.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tutti_plugin_ipc::Reaped;
use tutti_plugin_types::BridgeError;
use tutti_plugin_types::{
    Helper, Locator, PluginDescriptor, PluginFormat, ProbeOutcome, read_result,
};
use tutti_rt::StopToken;

/// Why a probe returned no descriptors.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProbeError {
    /// The caller's [`StopToken`] was stopped, and the child was killed.
    #[error("the probe was cancelled")]
    Cancelled,
    /// The probe ran, or could not be started, and failed.
    #[error(transparent)]
    Bridge(#[from] BridgeError),
}

/// How long a probe may take, spawn and the plugin's load included, unless the
/// caller says otherwise. A cold plugin can take seconds to `dlopen`; the old
/// socket probe allowed about twelve.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(5);

/// Reads the descriptor of every plugin a file holds in a `plugin-probe` subprocess, so a plugin that
/// crashes or hangs in its init does not take the caller down. `locator` names
/// the binary (a test passes [`Locator::at`], so none writes the
/// process-global environment) and `timeout` is the deadline.
///
/// # Errors
///
/// [`BridgeError::ProbeNotFound`] if the binary cannot be located;
/// [`BridgeError::LoadFailed`] if the probe ran to its end and the plugin could
/// not be read; [`BridgeError::ProcessCrashed`] if it died without a result;
/// [`BridgeError::Timeout`] if it did not finish within `timeout` (it is killed);
/// [`ProbeError::Cancelled`] if `stop` was requested (the child is killed and
/// reaped before this returns).
pub(crate) fn probe_metadata_with(
    locator: &Locator,
    plugin_path: &Path,
    format: PluginFormat,
    timeout: Duration,
    stop: &StopToken,
) -> Result<Vec<PluginDescriptor>, ProbeError> {
    run(locator, plugin_path, format, timeout, stop, &[])
}

/// [`probe_metadata_with`] that also **measures** each plugin (`plugin-probe
/// --measure`): the descriptors come back with
/// [`PluginDescriptor::measured`] set where the plugin could be measured. It
/// renders the plugin, so give it the long deadline
/// ([`DEFAULT_MEASURE_TIMEOUT`](crate::host::db_probe::DEFAULT_MEASURE_TIMEOUT)):
/// a plugin that crashes or hangs while rendering is
/// [`BridgeError::ProcessCrashed`] or [`BridgeError::Timeout`], as in the load.
///
/// `state` is a file of the bytes a plugin's own `save_state` wrote, which puts
/// the plugin in that state before the measurement (`--state`), for a file of one
/// plugin; the record carries a hash of it.
///
/// # Errors
///
/// As [`probe_metadata_with`]; a plugin that refuses `state` is
/// [`BridgeError::LoadFailed`].
pub(crate) fn measure_metadata_with(
    locator: &Locator,
    plugin_path: &Path,
    format: PluginFormat,
    timeout: Duration,
    stop: &StopToken,
    state: Option<&Path>,
) -> Result<Vec<PluginDescriptor>, ProbeError> {
    let mut args = vec!["--measure".as_ref()];
    if let Some(state) = state {
        args.push("--state".as_ref());
        args.push(state.as_os_str());
    }
    run(locator, plugin_path, format, timeout, stop, &args)
}

fn run(
    locator: &Locator,
    plugin_path: &Path,
    format: PluginFormat,
    timeout: Duration,
    stop: &StopToken,
    flags: &[&std::ffi::OsStr],
) -> Result<Vec<PluginDescriptor>, ProbeError> {
    let probe = locator
        .resolve(Helper::Probe)
        .ok_or(BridgeError::ProbeNotFound)?;
    // A directory of the probe's own (mode 0700), removed when it drops, on a
    // panic as well: the result path is not a name in a shared directory.
    let dir = tempfile::Builder::new()
        .prefix("tutti-probe-")
        .tempdir()
        .map_err(BridgeError::Io)?;
    let result = dir.path().join("result");

    let child = Command::new(&probe)
        .arg(plugin_path)
        .arg(&result)
        // The format the caller knows the file by, which a link's target need
        // not name in its own extension.
        .args(["--as", format.extension_id()])
        .args(flags)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(BridgeError::Io)?;
    let mut child = Reaped::new(child);
    let status = wait(&mut child, timeout, stop);
    // The file is read even after a timeout or a bad exit: it is the only proof
    // of finishing, and a probe that finished at the deadline did.
    let file = read_result(&result);
    let outcome = match (file, status) {
        (Ok(Some(outcome)), _) => outcome,
        (Err(e), _) => return Err(BridgeError::from(e).into()),
        (Ok(None), Waited::Exited) => return Err(BridgeError::ProcessCrashed.into()),
        (Ok(None), Waited::TimedOut) => {
            return Err(BridgeError::Timeout {
                operation: "probe",
                after: timeout,
                partial: false,
            }
            .into());
        }
        (Ok(None), Waited::Lost(e)) => return Err(BridgeError::Io(e).into()),
        (Ok(None), Waited::Cancelled) => return Err(ProbeError::Cancelled),
    };
    match outcome {
        ProbeOutcome::Described(plugins) if plugins.is_empty() => Err(
            BridgeError::load_from_server(plugin_path, "the file holds no plugin".to_string())
                .into(),
        ),
        ProbeOutcome::Described(plugins) => Ok(plugins),
        ProbeOutcome::Failed(message) => {
            Err(BridgeError::load_from_server(plugin_path, message).into())
        }
        // `ProbeOutcome` is `#[non_exhaustive]`: an outcome from a newer probe.
        other => Err(BridgeError::UnexpectedMessage {
            expected: "a described or failed probe outcome",
            got: format!("{other:?}").into(),
        }
        .into()),
    }
}

/// When a probe's time is up: a rule over clock readings, which its caller takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Deadline(Option<Instant>);

impl Deadline {
    /// The deadline `timeout` after `now`; one the clock cannot represent never
    /// passes.
    fn after(now: Instant, timeout: Duration) -> Self {
        Self(now.checked_add(timeout))
    }

    /// Whether the time is up at `now`: at the deadline itself, and after.
    fn passed(self, now: Instant) -> bool {
        self.0.is_some_and(|at| now >= at)
    }
}

enum Waited {
    Exited,
    TimedOut,
    Cancelled,
    Lost(std::io::Error),
}

/// Waits for `child` to exit, killing and reaping it at `timeout` or when
/// `stop` is requested.
fn wait(child: &mut Child, timeout: Duration, stop: &StopToken) -> Waited {
    let deadline = Deadline::after(Instant::now(), timeout);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Waited::Exited,
            Ok(None) if stop.is_stopped() => {
                let _ = child.kill();
                let _ = child.wait();
                return Waited::Cancelled;
            }
            Ok(None) if deadline.passed(Instant::now()) => {
                let _ = child.kill();
                let _ = child.wait();
                return Waited::TimedOut;
            }
            Ok(None) => {
                // Wakes at once on a stop; the child's exit has no wake to wait on.
                let _ = stop.wait(POLL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Waited::Lost(e);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tutti_plugin_types::BridgeResult;

    /// A `plugin-probe` stand-in: a shell script given the plugin path as `$1`
    /// and the result file as `$2`, as the real one is.
    fn stand_in(dir: &TempDir, body: &str) -> Locator {
        let path = dir.path().join("plugin-probe");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Locator::at(path)
    }

    fn probe(locator: &Locator, timeout: Duration) -> BridgeResult<Vec<PluginDescriptor>> {
        probe_metadata_with(
            locator,
            Path::new("/plugins/Some.clap"),
            PluginFormat::Clap,
            timeout,
            &StopToken::new(),
        )
        .map_err(|e| match e {
            ProbeError::Bridge(e) => e,
            ProbeError::Cancelled => panic!("nothing cancelled this probe"),
        })
    }

    /// A probe that does not finish by the deadline is killed and reaped, and the
    /// caller is told it timed out.
    #[test]
    fn a_probe_that_hangs_is_killed_at_the_deadline() {
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
        let stop = StopToken::new();
        // Opening and reading the fifo block until the child is running and has
        // written its pid, so the stop comes while the probe waits on the child.
        let raiser = {
            let stop = stop.clone();
            tutti_rt::spawn_named("probe-raiser", None, move |_| {
                let pid = std::fs::read_to_string(&fifo).expect("the child writes its pid");
                stop.stop();
                pid
            })
            .expect("the raiser starts")
        };

        let result = probe_metadata_with(
            &locator,
            Path::new("/plugins/Some.clap"),
            PluginFormat::Clap,
            Duration::from_secs(300),
            &stop,
        );
        let pid = raiser.join().unwrap();

        assert!(matches!(&result, Err(ProbeError::Cancelled)), "{result:?}");
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(
            !alive,
            "the cancelled probe (pid {}) is still running",
            pid.trim()
        );
    }

    /// No result file and an exit, by status or by signal, is a crash.
    #[test]
    fn exiting_without_a_result_is_a_crash() {
        for body in ["exit 7", "exit 0", "kill -SEGV $$"] {
            let dir = TempDir::new().unwrap();
            let result = probe(&stand_in(&dir, body), Duration::from_secs(10));
            assert!(
                matches!(result, Err(BridgeError::ProcessCrashed)),
                "`{body}`: {result:?}"
            );
        }
    }

    /// **A partial result is never read as a result**: a staging file left by a
    /// probe that died mid-write is not one (it is a crash), and a cut-short
    /// file at the result path is refused as a protocol fault, not decoded.
    #[test]
    fn a_partial_result_file_is_not_a_result() {
        let dir = TempDir::new().unwrap();
        let staged = stand_in(&dir, "printf 'TPRB\\001' > \"$2.tmp\"");
        let result = probe(&staged, Duration::from_secs(10));
        assert!(
            matches!(result, Err(BridgeError::ProcessCrashed)),
            "{result:?}"
        );

        let dir = TempDir::new().unwrap();
        let cut = stand_in(&dir, "printf 'TPRB\\001' > \"$2\"");
        let result = probe(&cut, Duration::from_secs(10));
        assert!(
            matches!(result, Err(BridgeError::ProbeResult(_))),
            "{result:?}"
        );
    }

    /// A probe binary that is not there is `ProbeNotFound`, the one error the
    /// scan answers with filename metadata.
    #[test]
    fn a_missing_probe_binary_is_probe_not_found() {
        let missing = Locator::at("/definitely/not/plugin-probe");
        assert!(matches!(
            probe(&missing, Duration::from_secs(1)),
            Err(BridgeError::ProbeNotFound)
        ));
    }

    /// The deadline is the caller's: a probe that does not exit times out under
    /// a deadline already up, and one that exits is a plain crash under a long one.
    #[test]
    fn the_deadline_is_the_callers() {
        let dir = TempDir::new().unwrap();
        let hangs = stand_in(&dir, "exec sleep 300");
        let up = probe(&hangs, Duration::ZERO);
        assert!(matches!(up, Err(BridgeError::Timeout { .. })), "{up:?}");

        let dir = TempDir::new().unwrap();
        let exits = stand_in(&dir, "exit 1");
        let long = probe(&exits, Duration::from_secs(10));
        assert!(matches!(long, Err(BridgeError::ProcessCrashed)), "{long:?}");
    }

    /// A deadline is up at its instant and after, and not before; one the clock
    /// cannot hold never is.
    #[test]
    fn a_deadline_is_up_at_its_instant_and_after() {
        let start = Instant::now();
        let deadline = Deadline::after(start, Duration::from_secs(15));
        assert!(!deadline.passed(start));
        assert!(!deadline.passed(start + Duration::from_millis(14_999)));
        assert!(deadline.passed(start + Duration::from_secs(15)));
        assert!(deadline.passed(start + Duration::from_secs(16)));
        assert!(Deadline::after(start, Duration::ZERO).passed(start));
        assert!(
            !Deadline::after(start, Duration::MAX).passed(start + Duration::from_secs(1 << 30))
        );
    }

    /// The default allows a cold plugin well past the old five seconds.
    #[test]
    fn the_default_deadline_is_fifteen_seconds() {
        assert_eq!(DEFAULT_PROBE_TIMEOUT, Duration::from_secs(15));
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
