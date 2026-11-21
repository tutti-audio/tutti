#![doc = include_str!("../README.md")]
//! Audio-plugin hosting for Tutti: VST3, CLAP and AU plugins in isolated
//! subprocesses, VST2 in-process, each exposed as a node of Tutti's audio graph.
//!
//! ## What this is
//!
//! Use this crate to load third-party plugins and run them inside a
//! [`tutti_graph`] graph. Every VST3, CLAP and AU plugin runs in its own
//! `plugin-server` subprocess, bridged over shared memory for audio and a local
//! socket for control, so a plugin that crashes takes down only its subprocess:
//! the host keeps running and reports the error. VST2 plugins run in the host
//! process behind the `vst2` feature.
//!
//! The crate also drives discovery (scanning plugin directories, a persistent
//! catalog with incremental rescan and crash blacklisting, in `tutti-plugin-db`)
//! and covers the main-thread control surface (parameters, state, presets, editor windows).
//!
//! ## Quick start
//!
//! Load a plugin and put its node into a tutti graph. `no_run`: the load spawns a
//! subprocess against a real `.vst3` / `.clap` on disk.
//!
//! ```rust,no_run
//! use tutti_buffers::MaxBlock;
//! use tutti_units::SampleRate;
//! use tutti_graph::{Editor, Prepare};
//! use tutti_plugin::catalog::Plugin;
//! use tutti_plugin_db::{Catalog, CatalogConfig};
//! use tutti_plugin_types::PluginId;
//! use tutti_plugin_db::PluginDb;
//! use tutti_plugin_ipc::AudioConfig;
//! use tutti_units::Samples;
//! use tutti_graph::NodeKey;
//! use tutti_timeline::TempoReader;
//! use tutti_units::Bpm;
//!
//! // `sample_rate` is a `SampleRate`, the engine's unit type, not a bare rate
//! // that could be a block size.
//! // A saved id names the plugin; the catalog says where it is on this machine, and
//! // a load takes what the catalog resolved (so its blacklist is asked).
//! let catalog = Catalog::new(
//!     PluginDb::new(), // or the one the app deserialized
//!     CatalogConfig::new("/my/app/state"),
//!     |_db| Ok(()),
//! );
//! let id: PluginId = "vst3.00112233-44556677-8899AABB-CCDDEEFF".parse()?;
//! let loadable = catalog.resolve(&id).bind()?;
//! let (_collector, work) = tutti_plugin_types::MainWork::new();
//! let executor = tutti_platform::Executor::new();
//! let plugin = Plugin::open_served(
//!     &AudioConfig::default(),
//!     &executor,
//!     &work,
//!     &loadable,
//!     SampleRate::SR_48K,
//! )?;
//! println!("{} by {}", plugin.descriptor().name, plugin.descriptor().vendor);
//!
//! // Two handles, one subprocess: keep the control surface before the node goes
//! // into the graph — the plugin dies when the last of either drops.
//! let handle = plugin.handle().clone();
//! println!("reported latency: {:?}", handle.loaded().latency());
//!
//! // Binding the plugin to the project's tempo map (`Plugin::bind`) makes it a
//! // node, an `IntoNode`: inserting it hands back its controls (a subprocess
//! // plugin's `PluginControls`), and a fork source so an export can render it.
//! // A session hands every plugin it adds a reader of its own; with none, a fixed
//! // map does.
//! let tempo = TempoReader::constant(Bpm::of(120.0)).expect("a tempo");
//! let (mut editor, _executor) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(512))));
//! let _controls = editor.insert(NodeKey(1), "plugin", plugin.bind(tempo));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! A host that does not already know where its plugin is discovers one first — see
//! [`Catalog`](tutti_plugin_db::Catalog), which scans, registers and measures plugin files
//! as cancellable tasks, recovers from a crashed scan, and says what is
//! blacklisted (the session's load asks it). It needs no feature flag.
//!
//! ## Architecture
//!
//! Every out-of-process plugin runs in its own `plugin-server` subprocess (the
//! [`tutti-plugin-server`](https://docs.rs/tutti-plugin-server) crate: one generic
//! shell over each served format's `Loaded`, CLAP, VST3 and AU; VST2 is not served,
//! its editor and audio instance being one object, so it runs in the host's
//! process and a VST2 path refuses out-of-process hosting with
//! [`IpcError::NotOutOfProcess`](tutti_plugin_types::IpcError::NotOutOfProcess)).
//! The host talks to it over two channels:
//!
//! - **Control** (Unix socket / named pipe) — editor open/close, parameter reads,
//!   state save/restore. Main-thread, blocking.
//! - **Audio** (shared-memory slab) — audio, MIDI and parameter changes per block.
//!   Audio-thread, lock-free and allocation-free.
//!
//! The host looks for the `plugin-server` binary at the path in the
//! `TUTTI_PLUGIN_SERVER` environment variable, then next to the application
//! executable, then in its parent directory, then on `PATH`.
//!
//! ### Two handles per plugin
//!
//! A loaded [`Plugin`](catalog::Plugin) yields two values that share the plugin's
//! lifetime — it dies only when the last of either drops:
//!
//! - [`PluginClient`] — the client of a served plugin and its audio-graph
//!   node (`tutti-plugin-host`'s generic `PluginNode` over the served hosting).
//!   Loaded `Unbound`; `bind()` makes it a `PluginClient<Bound>`, which holds the
//!   node and is what a graph inserts. `Plugin` does this for you when inserted directly.
//! - [`PluginHandle`](tutti_plugin_host::PluginHandle) (`tutti-plugin-host`'s;
//!   [`PluginClient::handle`] makes one) — the
//!   control surface: the editor, state restore and presets (main thread), and the
//!   parameters, state save and render mode (`PluginHandle::any_thread`, from any
//!   thread: a served plugin answers concurrently, an in-process one hops to the
//!   main thread with a deadline). Cheap to clone (`Arc`-shared).
//!
//! Per-block inputs (MIDI, chord/scale context, parameter automation) are gated
//! on what the plugin reported at load: an accessor answers `None` or `false`
//! when the plugin declined the input. Capabilities are data — a [`Features`]
//! bitset on [`LoadedPlugin`](tutti_plugin_types::LoadedPlugin) — never a check of the
//! plugin's format.
//!
//! ## Main types
//!
//! - [`catalog`] — loading plugins: [`Plugin`](catalog::Plugin) (one loaded plugin)
//!   and the node it binds to. The catalog with incremental rescan and crash recovery
//!   ([`Catalog`](tutti_plugin_db::Catalog)), the scanner and their records are
//!   `tutti-plugin-db`'s, and the identity type is
//!   [`PluginDescriptor`](tutti_plugin_types::PluginDescriptor).
//! - [`PluginClient`] — the client and graph
//!   node of a served plugin. The control surface over it, [`PluginHandle`](tutti_plugin_host::PluginHandle),
//!   the node's [`PluginControls`](tutti_plugin_host::PluginControls), the in-process
//!   node and the host policy written once for every host
//!   ([`PluginParamValues`](tutti_plugin_host::PluginParamValues) and its
//!   [`ParamRead`](tutti_plugin_host::ParamRead) job,
//!   [`Liveness`](tutti_plugin_host::Liveness), the death debounce: 3 unhealthy
//!   observations spanning 50 ms, and
//!   [`needs_notify`](tutti_plugin_host::needs_notify)) are
//!   `tutti-plugin-host`'s.
//! - [`IpcConfig`](tutti_plugin_ipc::IpcConfig) — low-level bridge tuning (buffer size, timeouts).
//! - [`IpcError`](tutti_plugin_types::IpcError) — the error most calls return.
//!
//! ## Features
//!
//! **Nothing is on by default.** Persistence and format support are the embedding
//! app's choices, so the default build pulls no serde and no format FFI.
//! Without any feature, VST3, CLAP, AU and VST2 plugins all load in the
//! subprocess, with no editor support.
//!
//! - `serde` — `PluginRef` and `tutti_plugin_db::PluginDb` derive
//!   `Serialize` and `Deserialize`: the app picks the file format and where it
//!   lives. `Catalog::new` takes the `PluginDb` to start from and a closure that
//!   stores it; a store's failure comes back as a `PersistError`. Deserializing a
//!   `PluginDb` refuses a record of another version.
//! - `vst3`, `clap`, `au` — editor support for that format. Loads the plugin
//!   library a second time in the *host* process for editor rendering only; audio
//!   still runs out of process. The editor instance is therefore not isolated: a
//!   crash in it takes the host down.
//! - `vst2` — in-process VST2 hosting (audio, MIDI, parameters, state, presets and
//!   native editor). VST2's `AEffect` fuses the editor and audio processor into one
//!   instance, so the two cannot live in separate processes; with this feature,
//!   [`Plugin::open`](catalog::Plugin::open) hosts `.vst` plugins in the host
//!   process.
//!
//! Where a plugin runs is its load's [`Hosting`](tutti_plugin_db::Hosting) ([`Loadable::hosting`](tutti_plugin_db::Loadable::hosting)): `Auto` (the
//! default) serves CLAP, VST3 and AU and hosts VST2 in process; `InProcess` hosts a
//! CLAP, VST3 or AU plugin in the host process too (with that format's feature),
//! and `OutOfProcess` on VST2 is refused.
//!
//! ## Capability model
//!
//! ### Functionality the host supports
//!
//! Every host-side capability is one of four kinds:
//!
//! - **Required** — the plugin must satisfy it or the load is refused. Plain trait
//!   methods, no flag: **audio (f32)**, **parameter get/set + enumerate**, **state
//!   save/restore**. All four formats provide these, so requiring them excludes
//!   nothing while protecting the save/load guarantee.
//! - **Negotiated** — the plugin answers once at load; the host adapts. A `Features`
//!   flag: **f64 audio**, **MIDI in/out**, **editor**, **editor resize**, **floating
//!   editor**. (Bus widths and latency are the numeric half — plain fields on
//!   `LoadedPlugin`, not flags.)
//! - **Best-effort** — sent per block only to plugins that advertise the flag, gated
//!   on `Features::CONSUMES` and never on format: **transport**, **parameter
//!   automation**, **note expression**, **sequencer context**.
//! - **Reaction** — an edge-triggered host→plugin call the flag gates, *not* a
//!   per-block feed, so these stay out of `Features::CONSUMES`: **automation
//!   state**, **preset list**, **preset load**, **render mode**.
//!
//! ### Per-format capability table
//!
//! What each format's loader supports. Where a format exposes a query the flag is
//! live-probed at load (VST3 `IProcessContextRequirements`, CLAP note ports);
//! otherwise the loader sets a fixed answer.
//!
//! `●` full · `◐` conditional / probed / advisory · `○` not implemented by our loader · `✕` the format can't (by spec)
//!
//! | Capability | Kind | VST3 | CLAP | AU | VST2 |
//! |---|---|:--:|:--:|:--:|:--:|
//! | Audio (f32) | Required | ● | ● | ● | ● |
//! | Params get/set + enumerate | Required | ● | ● | ● | ● |
//! | State save/restore | Required | ● | ● | ● | ● |
//! | `F64_AUDIO` | Negotiated | ◐ | ◐ | ○ | ◐ |
//! | `MIDI_IN` | Negotiated | ◐ | ◐ | ◐ | ◐ |
//! | `MIDI_OUT` | Negotiated | ◐ | ◐ | ○ | ◐ |
//! | `EDITOR` | Negotiated | ● | ● | ● | ● |
//! | `EDITOR_RESIZE` | Negotiated | ◐ | ◐ | ○ | ○ |
//! | `TRANSPORT` | Best-effort | ◐ | ● | ◐ | ● |
//! | `PARAM_AUTOMATION` | Best-effort | ● | ● | ○ | ○ |
//! | `NOTE_EXPRESSION` | Best-effort | ◐ | ◐ | ✕ | ✕ |
//! | `SEQUENCER_CONTEXT` | Best-effort | ◐ | ✕ | ✕ | ✕ |
//! | `PRESET_LIST` | Reaction | ◐ | ○ | ◐ | ◐ |
//! | `PRESET_LOAD` | Reaction | ◐ | ◐ | ◐ | ◐ |
//! | `AUTOMATION_STATE` | Reaction | ◐ | ◐ | ✕ | ● |
//! | `RENDER_MODE` | Reaction | ● | ◐ | ◐ | ● |
//! | `EDITOR_FLOATING` | Negotiated | ✕ | ◐ | ✕ | ✕ |
//!
//! Notes: `AUTOMATION_STATE` is probed where the plugin may lack the channel (VST3
//! `IAutomationState`, CLAP `param-indication`); the AU specification has no
//! host-to-plugin automation state, and VST2's is the host's answer to
//! `audioMasterGetAutomationState`. The AU loader reports `MIDI_IN`, `EDITOR`, `TRANSPORT`, `RENDER_MODE`
//! and the two preset bits; its f64 path is unimplemented (`○`), not spec-impossible,
//! and it never reports `MIDI_OUT`. AU's `TRANSPORT` is `◐` because it reports whether the unit
//! accepted `kAudioUnitProperty_HostCallbacks`; the loader feeds it every block. AU's `MIDI_IN` is `◐` off the component type (`aumu` /
//! `aumf` / `aumi` receive MIDI, `aufx` does not), which is the same predicate the
//! process path gates its per-block MIDI on. `MIDI_OUT` is not its mirror: the loader
//! installs a MIDI-output callback on a unit that declares MIDI output streams, and the
//! host reads that MIDI back, but it asks the unit nothing about it, so the bit stays
//! unset (`○`). VST2's `F64_AUDIO` is advisory (the `vst` crate is f32 internally).
//! `SEQUENCER_CONTEXT` (chord/scale/per-note text) is a VST3-only concept by spec.
//!
//! The preset bits are two because the formats split there. A VST3 plugin answers
//! both from its program lists: loading one writes the parameter flagged
//! `kIsProgramChange` that owns the list, which the loader does for you. CLAP
//! splits the two: `CLAP_EXT_PRESET_LOAD` loads from a path, while enumeration
//! lives in the factory-level preset-discovery extension this host does not bind.
//! AU backs both from `kAudioUnitProperty_FactoryPresets`, and VST2 from one
//! number, `AEffect::numPrograms`. Like `AUTOMATION_STATE` and `RENDER_MODE`, the
//! preset bits are edge-triggered host→plugin actions, so none of them joins
//! `Features::CONSUMES`.
//!
//! [`PluginHandle::presets`](tutti_plugin_host::PluginHandle::presets) (and `load_preset`) is the cross-format
//! surface, over an opaque [`PresetId`] that carries each format's own identifier
//! (an AU selector, a VST3 `(list, index)` pair, a CLAP path, a VST2 index) — a
//! single index would silently corrupt AU's sparse numbering.
//! [`PluginHandle::preset_support`](tutti_plugin_host::PluginHandle::preset_support)
//! collapses the two bits into what a preset UI can offer.
//!
//! ### `probed` — which capabilities a loader actually asked
//!
//! A clear `Features` bit answers three questions the same way: the plugin declined,
//! this loader never asked, or the format has no query. `LoadedPlugin::probed` is the
//! mask that separates the first from the other two, and `LoadedPlugin::capability()`
//! reads the pair — `Some(false)` for a refusal, `None` for silence. The per-block
//! send-gate deliberately keeps reading `features` directly: it has no way to act on
//! "unknown" and must stay one mask-and-compare.
//!
//! The masks are constants in [`tutti_plugin_types::features::probed`], one per format.
//!
//! `probed` does **not** distinguish `○` from `✕` — both are absent from it, because
//! both mean no plugin spoke. Which one applies is the table above: it describes this
//! codebase, not the plugin, and a runtime bit would go stale the moment a loader
//! grows the missing path. So the AU row's `○`s and `✕`s are all simply unset in
//! `probed::AU`.
//!
//! ## The plugin state machine
//!
//! This section is for readers of the format host crates. The shared vocabulary
//! (`tutti-plugin-types`) describes what a plugin does through
//! [`Control`](tutti_plugin_types::Control) (parameters, state, presets, in any
//! state that has a plugin), [`Processor`](tutti_plugin_types::Processor) (blocks,
//! on the realtime thread) and [`Loaded`](tutti_plugin_types::Loaded) (the
//! transitions), so a lifecycle reaches this crate only as the served and in-process
//! hostings that drive it. A probe never activates at all.
//!
//! What that buys is room. No format crate has to meet another in the middle, so each
//! one models its own lifecycle as tightly as its own contract allows — and left to
//! do that, the four land on three different answers. They are worth reading
//! together, because the differences are not stylistic: each is the format's own rule
//! showing through, and the same reasoning decides the shape of any format added
//! later.
//!
//! All four start from the same two states. A plugin is *loaded* — library mapped,
//! instance created, parameters and editor reachable — and later *activated*, which
//! allocates buffers at a fixed sample rate and block size and makes `process`
//! legal. Everything below is disagreement about what to do with that shape.
//!
//! ### Which model each format gets, and why
//!
//! Three modelling strategies are in use, and the choice is forced by the format's
//! own contract rather than picked for consistency.
//!
//! **Consuming type-state — VST3 and CLAP.** `Vst3Loaded → Vst3Instance<T>` and
//! `ClapLoaded → (ClapProcessor, ClapController)`. Both formats define a large, fully legal
//! pre-activation surface: the parameter tree, units and program lists, note
//! expression, state save/restore and the editor are all reachable before any audio
//! buffer exists, and a host is *expected* to read them there. "Loaded but not
//! processing" is a state a user spends real time in, so it earns a type of its own.
//! The transitions take `self` by value and hand back the other type
//! (`activate(self) -> Result<Active>`, `deactivate(self) -> Loaded`), which is what
//! makes a stale handle to a deactivated plugin unrepresentable rather than merely
//! discouraged — the compiler rejects it, and no runtime `is_active` check is needed
//! on the process path. The `T` parameter fixes the sample width at the same moment,
//! because both formats commit to a sample format in the same call that allocates
//! the buffers.
//!
//! Two details differ, and each is the format's rule showing through. VST3 chooses
//! its `ProcessMode` on the transition rather than on the instance, because
//! `setupProcessing` delivers it exactly once per activation. CLAP's `activate`
//! returns `Err((Self, ClapError))` — the *unconsumed* `ClapLoaded` comes back on
//! refusal, so a plugin that declines 64-bit audio can be retried at `f32` without
//! being reloaded.
//!
//! **The same split — VST2.** `Vst2Loaded` is the plugin after `effOpen`, and its
//! activation (`effSetSampleRate`, `effSetBlockSize`, `effSetProcessPrecision`, then
//! `effMainsChanged(1)`) splits it into a `Vst2Processor` and a `Vst2Controller`,
//! as the other formats'. The Controller's dispatcher calls run beside a rendering
//! Processor by the host convention every VST2 host follows, which the format does
//! not state; a rate change is the rejoin and an activation, like the others'.
//!
//! **AU.** `AuLoaded` implements `Loaded`: `activate` is `AudioUnitInitialize` (with
//! the stream configuration, render mode and input render callback it needs) and
//! splits the unit into an `AuProcessor` and an `AuController`; `deactivate` is
//! `AudioUnitUninitialize`. Both transitions are fallible, and the one case that
//! cannot hand the unit back (the render callback and the compensating uninitialize
//! both refused) is `Refused::plugin == None`. `tutti-au-host`'s own documentation
//! carries the argument.
//!
//! ### Choosing a shape for a fifth format
//!
//! Read together, the three answers reduce to one question asked twice. Do the two
//! states have genuinely different operations, and can the transition between them
//! fail in a way that belongs to neither? Two distinct surfaces and a transition that
//! always lands somewhere is the case a consuming type-state was made for. A
//! pre-activation state with nothing of its own to do should be fused, because the
//! extra type guards nothing. A transition that can fail in both directions has to
//! carry its state as data and pay for the check at runtime, because there is no
//! third type to return.
//!
//! The failure worth naming is the first one: reaching for a compile-time guarantee
//! the underlying contract cannot honour. That is how a type ends up confidently
//! describing a state the plugin is not actually in, which is worse than the runtime
//! check it replaced.
//!
//! ## Related crates
//!
//! - [`tutti-plugin-types`](https://docs.rs/tutti-plugin-types) — the shared value
//!   vocabulary ([`ParameterInfo`](tutti_plugin_types::ParameterInfo),
//!   [`TransportInfo`](tutti_plugin_types::TransportInfo), [`Features`], [`Preset`], …). The
//!   types a caller cannot avoid naming are re-exported at this crate's root.
//! - [`tutti-plugin-server`](https://docs.rs/tutti-plugin-server) — the
//!   subprocess side of the bridge.
//! - `tutti-vst3-host`, `tutti-clap-host`, `tutti-au-host`, `tutti-vst2-host` —
//!   the format hosts. A served plugin arrives already loaded and activated inside
//!   the subprocess, which names the served three (`tutti-vst2-host` runs in this
//!   process).
//! - [`tutti_graph`] — the audio graph a plugin node is inserted into.
//!
//! [`Features`]: tutti_plugin_types::Features
//! [`PresetId`]: tutti_plugin_types::PresetId
//! [`Preset`]: tutti_plugin_types::Preset
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

mod format;
mod host;

/// Loading what the catalog found.
///
/// Discovery, the catalog and its records live in `tutti-plugin-db`: it scans
/// plugin directories on a worker, probes each file in a `plugin-probe`
/// subprocess, persists the result through the app's closure and says what is
/// blacklisted. This module supplies what needs the plugin server:
/// [`Plugin`](catalog::Plugin) opens one resolved
/// [`Loadable`](tutti_plugin_db::Loadable), [`BoundPlugin`](catalog::BoundPlugin) is it
/// as a graph node, and [`PluginRebinder`](catalog::PluginRebinder) builds the node
/// that replaces it when the plugin changes its audio ports.
pub mod catalog {
    pub use crate::host::placement::{ResolvedHosting, placement};
    pub use crate::host::plugin::{BoundPlugin, Plugin, PluginRebinder, Rebuilt, RestoreRefused};
}

// The graph node of a served plugin and its two states. The control handle,
// the node's controls and the host policy over a handle are `tutti-plugin-host`'s.
pub use host::node::{Answer, Bound, PluginAnswers, PluginClient, Unbound};
