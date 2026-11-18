//! `plugin-probe <path> <result-file> [--as <format>] [--measure [--state <file>]]`:
//! load a plugin file just far enough to read the descriptor of every plugin it holds, write the outcome to the result file and exit.
//!
//! With `--measure` the probe then activates each plugin and renders it: the
//! tail and latency it reports against what it makes of an impulse
//! (`tutti_plugin_probe::measure`). The figures are in each
//! descriptor's `measured`. A plugin that crashes or hangs while rendering is a
//! probe that crashed or hung, to the host that spawned it. `--state <file>`
//! puts the plugin in the state the file holds (what the plugin's own
//! `save_state` wrote) before it is measured, so the figure is of that patch:
//! the record carries a hash of it.
//!
//! `--as <format>` (`clap`, `vst3`, `vst2`, `au`) names the format the path is read
//! as, for a file whose own extension does not (a link `Foo.clap` to `libfoo.so`).
//!
//! The result file is the only wire (`tutti_plugin_types::write_result`: a
//! staging file renamed into place). Plugins print from their init code, so
//! this process's stdout and stderr carry nothing; the host that spawns it
//! points them at the null device. A file at `<result-file>` means the probe
//! finished, with a descriptor or a typed refusal; no file means it crashed or
//! hung, which the host tells from the process.
//!
//! Exit status: one of `tutti_plugin_types::ProbeExit`: `0` once a result is
//! written (a plugin that would not load is a result, not an error, and so is a
//! bad command line or a thread that is not the main one), `75` if no result could
//! be written. The small statuses a plugin's own `exit` uses are never the probe's.
//!
//! The probe ends itself when its host ends (`HOST_PID_ENV`, which it trusts only
//! while it names the parent, else the parent).
//!
//! Formats are cargo features (`clap`, `vst3`, `vst2`, and `au` on macOS); a probe
//! built without the one a path names reports that as a refusal.

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;
use tutti_plugin_probe::{describe, describe_measured};
use tutti_plugin_types::{
    MainThreadMarker, PluginFormat, ProbeExit, ProbeOutcome, ProbeRefusal, write_result,
};

const USAGE: &str = "usage: plugin-probe <plugin-path> <result-file> [--as <format>] \
                     [--measure [--state <file>]]";

fn main() -> ExitCode {
    watch_host();
    let mut args = std::env::args_os().skip(1);
    let (Some(path), Some(result)) = (args.next(), args.next()) else {
        // No result file is named, so nothing can say what was wrong.
        eprintln!("{USAGE}");
        return ExitCode::from(ProbeExit::ResultUnwritable.code());
    };
    let mut rest: Vec<_> = args.collect();
    let format = match rest.as_slice() {
        [a, f, ..] if a == "--as" => match f.to_str().and_then(PluginFormat::from_extension_id) {
            Some(format) => Some(format),
            None => return usage(&result),
        },
        _ => None,
    };
    if format.is_some() {
        rest.drain(..2);
    }
    let (measure, state_file) = match rest.as_slice() {
        [] => (false, None),
        [m] if m == "--measure" => (true, None),
        [m, s, file] if m == "--measure" && s == "--state" => (true, Some(file)),
        _ => return usage(&result),
    };
    let state = match state_file.map(std::fs::read).transpose() {
        Ok(state) => state,
        // A bad `--state` is the caller's, not a plugin's: a result, not a crash.
        Err(e) => {
            return finish(
                &result,
                ProbeOutcome::Failed(ProbeRefusal::StateUnreadable(e.to_string())),
            );
        }
    };
    // This process runs one probe on one thread, which is its main thread.
    let Some(main) = MainThreadMarker::new() else {
        return finish(&result, ProbeOutcome::Failed(ProbeRefusal::NotMainThread));
    };
    let path = Path::new(&path);
    let described = if measure {
        describe_measured(main, path, format, state.as_deref())
    } else {
        describe(main, path, format)
    };
    let outcome = match described {
        Ok(plugins) if plugins.is_empty() => ProbeOutcome::Failed(ProbeRefusal::Empty),
        Ok(plugins) => ProbeOutcome::Described(plugins),
        Err(refusal) => ProbeOutcome::Failed(refusal),
    };
    finish(&result, outcome)
}

/// Ends this process at once when its host ends, wherever the plugin has it
/// blocked, and does it first, so a plugin that hangs in its init dies with the
/// host. A probe has nothing to clean up (the host owns the result's directory),
/// and a plugin's exit handlers must not run while the plugin is mid-call. A probe
/// with no known host runs unwatched, and the host's deadline is its only bound.
fn watch_host() {
    match tutti_plugin_ipc_ffi::watch_spawner(|| tutti_plugin_ipc_ffi::terminate_process()) {
        Ok(true) => {}
        Ok(false) => eprintln!("plugin-probe: the host's process id is unknown: running unwatched"),
        Err(e) => eprintln!("plugin-probe: the host watch cannot start: {e}"),
    }
}

/// A command line that is wrong is a result, so it is not read as a crash.
fn usage(result: &OsStr) -> ExitCode {
    eprintln!("{USAGE}");
    finish(result, ProbeOutcome::Failed(ProbeRefusal::Usage))
}

/// Writes the outcome to the result file.
fn finish(result: &OsStr, outcome: ProbeOutcome) -> ExitCode {
    match write_result(Path::new(result), &outcome) {
        Ok(()) => ExitCode::from(ProbeExit::Written.code()),
        Err(e) => {
            eprintln!(
                "plugin-probe: cannot write {}: {e}",
                Path::new(result).display()
            );
            ExitCode::from(ProbeExit::ResultUnwritable.code())
        }
    }
}
