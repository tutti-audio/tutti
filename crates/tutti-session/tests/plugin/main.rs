//! Hosted plugins through the session: latency, bypass, health, modulation and exports.
#![allow(
    clippy::disallowed_methods,
    reason = "a test starts the threads it races, outside any executor"
)]

/// An allocation inside an `RtScope` aborts the running test.
#[global_allocator]
static ALLOC: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

#[cfg(feature = "clap")]
#[path = "../support/mod.rs"]
mod support;

// A plugin's parameter values, read through the host's mirror.
#[cfg(feature = "clap")]
#[path = "../support/param_reads.rs"]
mod param_reads;

// Loading a plugin file, as a test does where an app runs the load.
#[cfg(feature = "plugin")]
#[path = "../support/loading.rs"]
mod loading;

// Hosted plugins, their render mode and exports, over the reference CLAP
// plugin in a real `plugin-server`: fails without the server, by design.
#[cfg(all(feature = "clap", feature = "export"))]
mod plugin;
// A hosted plugin's bypass is the graph's `NodeMode` (doc 013 M2's host half):
// the reference CLAP plugin in a real `plugin-server`.
#[cfg(feature = "clap")]
mod plugin_bypass;
// A hosted plugin with a bypass of its own is bypassed by the host writing it:
// the shared test plugin (CLAP) in a real `plugin-server`.
#[cfg(feature = "clap")]
mod plugin_own_bypass;
// Which plugin of a bundle a load opens, and whose state a restore accepts:
// the reference CLAP bundle exports two, in a real `plugin-server`.
#[cfg(feature = "clap")]
mod plugin_identity;
// A session's drop ends a served plugin's lanes while a render feeds them:
// the reference CLAP plugin in a real `plugin-server`.
#[cfg(all(feature = "clap", feature = "device"))]
mod plugin_executor_drop;
// A served plugin through the session: lost, recovered, saved, and swapped at its node:
// the reference CLAP bundle in a real `plugin-server`.
#[cfg(feature = "clap")]
mod plugin_served;
// A plugin as a load: a handle, a node from it, and the plugin closing with the last clone:
// the reference CLAP bundle in a real `plugin-server`.
#[cfg(feature = "clap")]
mod plugin_load;
// What a load asks of the catalog (a blacklisted plugin is refused at load) and
// of the ref's `Hosting`: the reference CLAP plugin.
#[cfg(feature = "clap")]
mod plugin_hosting;
// A hosted plugin's latency is the session's, across a crossfade and a
// restart: the reference CLAP plugin in a real `plugin-server`.
#[cfg(all(feature = "clap", feature = "device", feature = "export"))]
mod plugin_latency;
// Modulation lanes on a hosted plugin's parameters: across a recovery and a
// crossfade, and an export equal to the live render.
#[cfg(all(feature = "clap", feature = "export"))]
mod plugin_modulation;
// The reference VST2 plugin, in process: the project meter it is given.
#[cfg(feature = "vst2")]
mod vst2_meter;
// A plugin's note names and parameter groups through its info:
// the shared test plugin (CLAP), in a real `plugin-server`.
#[cfg(feature = "clap")]
mod plugin_info;
// A plugin's presets through its info and the session's jobs: a list that loads
// by id (VST2, in process), one read for a served plugin (VST3, in a real
// `plugin-server`) and a file that loads by path (CLAP, in a real `plugin-server`).
#[cfg(any(feature = "clap", feature = "vst2", feature = "vst3"))]
mod plugin_presets;
// Diagnostics and buses: the counts either hosting reads the
// same way, an aux input wired through the plugin's bus handles, a server named
// and ended.
#[cfg(any(feature = "clap", feature = "vst2"))]
mod plugin_diagnostics;
// A plugin scan on the session: awaited as a future, dropped to cancel, ended by the
// session's drop. The reference CLAP bundle and a stand-in `plugin-probe` that hangs.
#[cfg(all(feature = "clap", unix))]
mod plugin_scan;

// Starts a bounce, awaits its render and applies it.
#[cfg(all(feature = "clap", feature = "export"))]
#[path = "../support/bounced.rs"]
mod bounced;
