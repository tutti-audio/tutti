#![forbid(unsafe_code)]
//! The `plugin-server` binary — one process per hosted plugin.
//!
//! Spawned by the host (`tutti-plugin`), never run by hand: it takes the
//! host-chosen socket path as its sole argument and serves exactly one host,
//! then exits. Isolation is the point — a plugin that crashes takes this process
//! down and not the DAW.
//!
//! Logs go to standard error (which the host passes on), at `info` unless
//! `RUST_LOG` says otherwise. The exit status is 0 for a session that ended with
//! every plugin destroyed, [`EXIT_PLUGIN_LEAKED`] for one that had to leak a
//! plugin, and 1 for an error.

use std::env;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tutti_plugin_server::{EXIT_PLUGIN_LEAKED, Ended, PluginServer};
use tutti_plugin_types::IpcResult;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    match serve() {
        Ok(Ended::Clean) => ExitCode::SUCCESS,
        Ok(Ended::PluginLeaked) => ExitCode::from(EXIT_PLUGIN_LEAKED),
        Err(error) => {
            eprintln!("plugin-server: {error}");
            ExitCode::FAILURE
        }
    }
}

fn serve() -> IpcResult<Ended> {
    let socket_path = env::args()
        .nth(1)
        .expect("Socket path required as first argument");

    PluginServer::new(socket_path.into())?.run()
}
