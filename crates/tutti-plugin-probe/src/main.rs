//! `plugin-probe <path> <result-file> [--as <format>] [--measure [--state <file>
//! [--plugin <id>]] [--budget <seconds>]]`: load a plugin file just far enough to read the
//! descriptor of every plugin it holds, write the outcome to the result file and exit. The
//! flags come in any order, each at most once.
//!
//! With `--measure` the probe then activates each plugin and renders it: the
//! tail and latency it reports against what it makes of an impulse. The figures are in each
//! descriptor's `measured`; a plugin that could not be measured is named in the
//! result with why. A plugin that crashes or hangs while rendering is a probe that
//! crashed or hung, to the host that spawned it. `--state <file>` puts one plugin
//! in the state the file holds (what the plugin's own `save_state` wrote) before it
//! is measured, so the figure is of that patch: the record carries a hash of it.
//! The plugin is the one `--plugin` names by its id (`PluginId`'s text, `clap.<id>`), else the file's only
//! plugin (a state with no plugin named for a file of several is refused); the
//! others are measured as they load. `--budget` bounds the whole run: each
//! plugin's renders get what the budget leaves, so a slow plugin times out its own
//! rows before the host that spawned the probe gives up on all of it.
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

mod args;

use args::{Args, Measure, USAGE, Usage};
use std::path::Path;
use std::process::ExitCode;
use tutti_platform::Instant;
use tutti_plugin_probe::{StateFor, describe, describe_measured, read_widths};
use tutti_plugin_types::{
    Deadline, MainThreadMarker, ProbeExit, ProbeOutcome, ProbeRefusal, write_result,
};

fn main() -> ExitCode {
    watch_host();
    let args = match args::parse(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(Usage::NoResultFile) => {
            // No result file is named, so nothing can say what was wrong.
            eprintln!("{USAGE}");
            return ExitCode::from(ProbeExit::ResultUnwritable.code());
        }
        Err(Usage::Wrong { result, cause }) => {
            eprintln!("plugin-probe: {cause}");
            return usage(&result);
        }
    };
    let Args {
        path,
        result,
        format,
        measure,
    } = args;
    // The deadline of the whole run, from here: the plugins of a file share it.
    let run = measure
        .as_ref()
        .and_then(|m| m.budget)
        .map_or(Deadline::never(), |budget| {
            Deadline::after(Instant::now(), budget)
        });
    let state = match measure
        .as_ref()
        .and_then(|m| m.state.as_ref())
        .map(|s| std::fs::read(&s.file))
        .transpose()
    {
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
    let described = match &measure {
        Some(Measure { state: named, .. }) => {
            let state = state
                .as_deref()
                .zip(named.as_ref())
                .map(|(bytes, named)| StateFor {
                    plugin: named.plugin.as_ref(),
                    bytes,
                });
            describe_measured(main, &path, format, state, run)
        }
        None => describe(main, &path, format).map(|plugins| (plugins, Vec::new())),
    };
    let outcome = match described {
        Ok((plugins, _)) if plugins.is_empty() => ProbeOutcome::Failed(ProbeRefusal::Empty),
        Ok((mut plugins, unmeasured)) => {
            // The listing is written before any plugin is loaded for its widths,
            // so a plugin that dies loading costs the widths and not the listing:
            // the host reads the result file whatever the exit.
            let listing = ProbeOutcome::Described {
                plugins: plugins.clone(),
                unmeasured: unmeasured.clone(),
            };
            if let Err(code) = write(&result, &listing) {
                return code;
            }
            read_widths(main, &path, format, &mut plugins);
            ProbeOutcome::Described {
                plugins,
                unmeasured,
            }
        }
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
fn usage(result: &Path) -> ExitCode {
    eprintln!("{USAGE}");
    finish(result, ProbeOutcome::Failed(ProbeRefusal::Usage))
}

/// Writes the outcome to the result file.
fn finish(result: &Path, outcome: ProbeOutcome) -> ExitCode {
    match write(result, &outcome) {
        Ok(()) => ExitCode::from(ProbeExit::Written.code()),
        Err(code) => code,
    }
}

/// Writes `outcome` to `result`, or the exit code that says it could not.
fn write(result: &Path, outcome: &ProbeOutcome) -> Result<(), ExitCode> {
    write_result(result, outcome).map_err(|e| {
        eprintln!("plugin-probe: cannot write {}: {e}", result.display());
        ExitCode::from(ProbeExit::ResultUnwritable.code())
    })
}
