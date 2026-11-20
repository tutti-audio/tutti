//! Hosted plugins through the session: latency, bypass, health, modulation and exports.

#[cfg(feature = "clap")]
#[path = "../support/mod.rs"]
mod support;

// A plugin's parameter values, read through the host's mirror.
#[cfg(feature = "clap")]
#[path = "../support/param_reads.rs"]
mod param_reads;

// Loading a plugin file through the pool, as a test does where an app declares the entry.
#[cfg(feature = "plugin")]
#[path = "../support/pooled.rs"]
mod pooled;

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
#[cfg(feature = "clap")]
mod plugin_executor_drop;
// A plugin as a pool entry, served: lands, swaps, is lost and retried, and held by its node:
// the reference CLAP bundle in a real `plugin-server`.
#[cfg(feature = "clap")]
mod plugin_pool;
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
