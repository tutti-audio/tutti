#![doc = include_str!("../README.md")]
//! Safe hosting for [CLAP](https://cleveraudio.org/) audio plugins — the FFI is
//! wrapped so a caller writes no `unsafe` of its own.
//!
//! ## What this is
//!
//! The CLAP host among tutti's four plugin-format hosts. It loads a `.clap` bundle,
//! drives audio and MIDI processing, exposes the parameter tree, audio- and
//! note-port topology, state save/restore with CLAP's save contexts, the plugin's
//! editor, and the host-callback polling surface.
//!
//! Most applications reach it through `tutti-plugin` (feature `clap`) or
//! `tutti-plugin-server`, which give every format one interface. Use it directly
//! when you want a CLAP host without the rest of the engine.
//!
//! Three types carry the lifecycle: [`ClapLoaded`] is the mapped, instantiated
//! plugin, which is not rendering. Activating it takes `self` by value and splits
//! it into a [`ClapProcessor`], the realtime half, where
//! [`process`][`ClapProcessor::process`] lives, and a [`ClapController`], the
//! main-thread half. They implement the shared `Loaded`, `Processor` and
//! `Controller` traits of `tutti-plugin-types`, which is what `Lifecycle`
//! drives.
//!
//! The other types you will meet first:
//!
//! - `InputOutput<S>` (`tutti-buffers`): the per-block audio a
//!   [`process`][`ClapProcessor::process`] call reads and writes; its width `S` is
//!   the processor's.
//! - [`ClapProcessContext`]: per-block MIDI, parameter changes and transport.
//! - [`PluginInfo`]: the descriptor (id, name, vendor, features) read at load.
//! - [`ClapError`]: the error type, with [`LoadStage`] for load failures.
//!
//! ## What it does not own
//!
//! **The shared vocabulary.** [`ParameterChanges`], [`TransportInfo`],
//! [`WindowHandle`] and the `ParameterInfo` that
//! [`parameter_list`][`ClapLoaded::parameter_list`] hands back are
//! [`tutti-plugin-types`](../tutti-plugin-types)' — re-exported here so a caller
//! can stay format-agnostic, not defined here. MIDI is the workspace-wide UMP: an event in is a
//! `(tutti_graph::Offset, tutti_ump::Ump)`, an event out is a [`WireMidi`] (the
//! packet and the raw time the plugin reported); there is **no
//! CLAP-specific MIDI event trait to implement**. What *is* CLAP-native is
//! [`ClapNoteExpression`], the per-voice expression type, which keeps its own name
//! precisely so it does not shadow the shared one.
//!
//! **Subprocess isolation.** This crate hosts in-process; `tutti-plugin-server` puts
//! it in a subprocess and [`tutti-plugin`](../tutti-plugin) is the host side.
//!
//! ## Quick start
//!
//! Load, activate, render one block. `no_run`: the load needs a real `.clap` bundle
//! on disk.
//!
//! ```rust,no_run
//! # let main = tutti_plugin_types::MainThreadMarker::new().expect("on the UI thread");
//! # let (_collector, work) = tutti_plugin_types::MainWork::new();
//! use tutti_graph::Offset;
//! use tutti_fixtures::midi;
//! use tutti_ump::{MidiChannel, NoteVelocity};
//! use tutti_buffers::{InputOutput, Planes, PlanesMut};
//! use tutti_clap_host::{ClapLoaded, ClapProcessContext, LoadOptions, TransportInfo};
//! use tutti_plugin_types::{Loaded, MaxBlock, Prepare, SampleRate, Samples};
//!
//! // `ClapLoaded` is the GUI / parameter / state stage. The rate and the block
//! // length are not load parameters: they are the activation's.
//! let loaded = ClapLoaded::load(main, &work, "/usr/lib/clap/MyPlugin.clap", LoadOptions::default())?;
//! println!("{} by {}", loaded.info().name, loaded.info().vendor);
//!
//! // A refusal hands the plugin back in the error, so a plugin that declines f64
//! // can be retried at f32 without reloading.
//! let prepare = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(512)));
//! let (mut processor, controller) = loaded
//!     .activate(main, prepare)
//!     .map_err(|refused| tutti_clap_host::ClapError::ProcessError(refused.reason.to_string()))?;
//!
//! // 512 is the block length in FRAMES; each channel slice holds that many.
//! let silence = vec![0.0f32; 512];
//! let inputs: [&[f32]; 2] = [&silence, &silence];
//! let (mut left, mut right) = (vec![0.0f32; 512], vec![0.0f32; 512]);
//! let mut outputs: [&mut [f32]; 2] = [&mut left, &mut right];
//! let mut buffer = InputOutput::new(
//!     Samples::new(512),
//!     Planes::new(&inputs).expect("two equal planes"),
//!     PlanesMut::new(&mut outputs).expect("two equal planes"),
//! )
//! .expect("equal lengths");
//!
//! let transport = TransportInfo::new(tutti_plugin_types::SampleRate::SR_48K).tempo(tutti_units::Bpm::of(120.0)).playing(true);
//! let midi = [(Offset::ZERO, midi::note_on(MidiChannel::FIRST, 60, 16384))];
//! processor.process(&mut buffer, &ClapProcessContext {
//!     midi: &midi,
//!     transport: Some(&transport),
//!     ..Default::default()
//! })?;
//!
//! // The halves rejoin into the plugin, deactivated.
//! let _loaded = ClapLoaded::deactivate(main, processor, controller)
//!     .map_err(|stuck| tutti_clap_host::ClapError::ProcessError(stuck.reason.to_string()))?;
//! # Ok::<(), tutti_clap_host::ClapError>(())
//! ```
//!
//! ### Parameters and state
//!
//! [`ClapController`] derefs to [`ClapLoaded`], so all of this is reachable while
//! audio is running as well as before activation. `no_run` for the same reason as
//! above.
//!
//! ```rust,no_run
//! # use tutti_clap_host::{ClapLoaded, LoadOptions, StateContext};
//! use tutti_plugin_types::{Control, StateUse};
//! use tutti_units::Normalized;
//! use tutti_units::ParamId;
//! # fn ex() -> Result<(), Box<dyn std::error::Error>> {
//! # let main = tutti_plugin_types::MainThreadMarker::new().expect("on the UI thread");
//! # let (_collector, work) = tutti_plugin_types::MainWork::new();
//! let mut plugin = ClapLoaded::load(main, &work, "/usr/lib/clap/MyPlugin.clap", LoadOptions::default())?;
//!
//! // CLAP addresses parameters by an opaque, plugin-chosen `clap_id`, never by
//! // position. `parameters` projects them into the shared `ParameterInfo`, so a
//! // consumer never learns which format it is reading.
//! for info in plugin.parameters(main) {
//!     println!("{} {:?}", info.qualified_name(), info.bounds());
//! }
//!
//! // Values are normalized `0..=1` at the trait. A loaded plugin takes the write
//! // now; a controller queues it to the Processor, which applies it at the head
//! // of its next block.
//! plugin.set_parameter(main, ParamId::new(0), Normalized::of(0.75))?;
//! plugin.set_parameter(main, ParamId::new(1), Normalized::of(0.5))?;
//!
//! // State save/restore is legal before any buffer exists. A project's state is
//! // saved for `CLAP_STATE_CONTEXT_FOR_PROJECT` when the plugin implements
//! // `clap.state-context`, and through plain `clap.state` when it does not.
//! let saved = plugin.save_state(main, StateUse::Project)?;
//! plugin.load_state(main, &saved, StateUse::Project)?;
//!
//! // A duplicate of the instance is saved and restored for its own context. A
//! // plugin that implements `clap.state-context` and refuses is not answered with
//! // a plain save: the two legitimately differ.
//! let copy = plugin.state_with_context(main, StateContext::ForDuplicate)?;
//! println!("{} bytes of duplicate state", copy.len());
//! # Ok(()) }
//! ```
//!
//! ### Editor
//!
//! ```rust,no_run
//! # use tutti_clap_host::{ClapLoaded, ContentScale, LoadOptions, WindowHandle};
//! # use tutti_plugin_types::{Control, EditorWindow};
//! # fn ex(window: &impl raw_window_handle::HasWindowHandle) -> Result<(), Box<dyn std::error::Error>> {
//! # let main = tutti_plugin_types::MainThreadMarker::new().expect("on the UI thread");
//! # let (_collector, work) = tutti_plugin_types::MainWork::new();
//! # let mut plugin = ClapLoaded::load(main, &work, "/usr/lib/clap/MyPlugin.clap", LoadOptions::default())?;
//! if plugin.has_editor(main) {
//!     // Any winit or other `raw-window-handle` window.
//!     let handle = WindowHandle::from_window(window)?;
//!     let size = plugin.open_editor(main, handle, ContentScale::IDENTITY)?;
//!     println!("editor is {size:?}");
//! }
//! plugin.close_editor(main);
//! # Ok(()) }
//! ```
//!
//! [`ClapLoaded`]: crate::ClapLoaded
//! [`ClapLoaded::parameter_list`]: crate::ClapLoaded::parameter_list
//! [`ClapController`]: crate::ClapController
//! [`ClapProcessor`]: crate::ClapProcessor
//! [`ClapProcessor::process`]: crate::ClapProcessor::process
//! [`ClapError`]: crate::ClapError
//! [`LoadStage`]: crate::LoadStage
//! [`ClapProcessContext`]: crate::ClapProcessContext
//! [`PluginInfo`]: crate::PluginInfo
//! [`ClapNoteExpression`]: crate::ClapNoteExpression
//! [`ParameterChanges`]: crate::ParameterChanges
//! [`TransportInfo`]: crate::TransportInfo
//! [`WindowHandle`]: crate::WindowHandle
//! [`WireMidi`]: crate::WireMidi
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` the CLAP host needs is in `tutti-clap-ffi`.
#![forbid(unsafe_code)]

mod error;
mod events;
pub mod host;
pub mod instance;
pub mod topology;
pub mod types;

pub use error::{ClapError, LoadStage, Result};
pub use events::{
    ClapEvent, EventList, FlushEvents, InputEventList, InputRooms, OutputEventList, OutputRooms,
    ParamBounds, TooManyNotePorts,
};
pub use host::{ClapHost, HostState, InputStream, OutputStream};
pub use instance::{
    ClapController, ClapEditor, ClapLoaded, ClapProcessContext, ClapProcessor, Flushed, LoadOptions,
};
// The CLAP-native, voice-addressed note expression is re-exported under its
// own distinct name (it does NOT shadow the shared
// `tutti_plugin_types::NoteExpressionValue`). The native parameter types
// (`ClapParamInfo` / `ClapParamFlags`) stay crate-private: the boundary speaks
// the shared `ParameterInfo` via `ClapLoaded::parameter_list`.
pub use types::{
    AudioPortFlags, AudioPortInfo, ChannelLayout, Color, ContentScale, EditorCapabilities,
    NoteDialect, NoteDialects, NoteExpressionType, NoteName, NotePortInfo, ParamAutomationState,
    ParameterChanges, ParameterPoint, ParameterQueue, PluginInfo, StateContext, SurroundChannel,
    TransportInfo, VoiceInfo, WindowHandle, WireMidi,
};
pub use types::{ClapNoteExpression, RT_NOTE_EXPR_CAPACITY, RtNoteExpressions};

// Test-only global allocator for RT-safety regression tests.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;
