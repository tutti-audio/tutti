#![forbid(unsafe_code)]
//! The `plugin-server` binary — one process per hosted plugin.
//!
//! Spawned by the host (`tutti-plugin`), never run by hand: the host hands it the
//! connection (the socket on its standard input, a pipe's name in its arguments on
//! Windows) and it serves exactly one host, then exits. Isolation is the point — a plugin that crashes takes this process
//! down and not the DAW.
//!
//! Logs go to standard error (which the host passes on), at `info` unless
//! `RUST_LOG` says otherwise. The exit status is 0 for a session that ended with
//! every plugin destroyed, [`EXIT_PLUGIN_LEAKED`] for one that had to leak a
//! plugin, [`EXIT_TASKS_DETACHED`] for one that left a task running,
//! [`EXIT_RENDER_LANE_LOST`] for a render lane that panicked or was dropped,
//! `EXIT_HOST_UNWATCHED` when the thread that watches the host could not start,
//! and 1 for any other error.
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

use std::process::ExitCode;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tutti_plugin_server::{
    EXIT_PLUGIN_LEAKED, EXIT_RENDER_LANE_LOST, EXIT_TASKS_DETACHED, Ended, HostEnd, PluginServer,
    adopt_host, failure_status,
};
use tutti_plugin_types::IpcResult;

fn main() -> ExitCode {
    // First, before a thread starts: it replaces the process's standard input.
    let host = match adopt_host() {
        Ok(host) => host,
        Err(error) => {
            eprintln!("plugin-server: {error}");
            return ExitCode::from(failure_status(&error));
        }
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    match serve(host) {
        Ok(Ended::Clean) => ExitCode::SUCCESS,
        Ok(Ended::PluginLeaked) => ExitCode::from(EXIT_PLUGIN_LEAKED),
        Ok(Ended::RenderLaneLost(_)) => ExitCode::from(EXIT_RENDER_LANE_LOST),
        Ok(Ended::TasksDetached(_)) => ExitCode::from(EXIT_TASKS_DETACHED),
        Err(error) => {
            eprintln!("plugin-server: {error}");
            ExitCode::from(failure_status(&error))
        }
    }
}

fn serve(host: HostEnd) -> IpcResult<Ended> {
    PluginServer::new(host)?.run()
}
