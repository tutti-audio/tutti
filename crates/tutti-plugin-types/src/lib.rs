#![doc = include_str!("../README.md")]
//! The shared value vocabulary underneath Tutti's four plugin-host crates.
//!
//! ## What this is
//!
//! Everything a VST2 / VST3 / CLAP / AU host needs to speak that is *not* specific
//! to one format: [`PluginDescriptor`] and [`PluginWiring`] (identity and the
//! load-time wiring snapshot), [`ParameterInfo`] with its [`ParamId`] /
//! [`PluginParamRange`] / [`ParamFlags`] parts, [`ProcessContext`] and [`ProcessOutput`],
//! [`TransportInfo`], [`ParameterChanges`] automation queues, note expression and
//! harmony changes, editor vocabulary
//! ([`WindowHandle`], [`EditorUnit`], [`EditorCapabilities`]), presets, and the
//! [`Features`] capability bitset.
//!
//! It also carries the [`bundle`] helpers that find the loadable module inside a
//! `.vst3` / `.vst` bundle.
//!
//! ## What it does not own
//!
//! **Any format's lifecycle.** The traits that carry a plugin's transitions and
//! control surface (`Loaded`, `Processor`, `Controller`, `Control`) are the
//! vocabulary; each format crate decides what its own lifecycle is as tightly as its
//! own contract allows, and the comparative account is in `tutti-plugin`'s crate
//! docs under *The plugin state machine*.
//!
//! **Any FFI.** Nothing here links a plugin SDK. The format host crates
//! (`tutti-vst2-host`, `tutti-vst3-host`, `tutti-clap-host`, `tutti-au-host`) do
//! that, and each re-exports these types from its own public API so a caller can
//! stay format-agnostic whenever only the shared surface is in play.
//!
//! ## Why it is its own crate
//!
//! **It sits under all four format hosts, and each re-exports it.** Without a common
//! home, the same `TransportInfo` would exist four times and a host would translate
//! between four dialects of it.
//!
//! The shape here is designed to keep formats' disagreements **representable rather
//! than averaged away**: [`PluginParamRange`] is a sum type because there is no safe number
//! to substitute when a format declares no range; [`ParamSteps`] separates
//! "continuous" from "nobody said", which a bare `step_count: u32` fuses at zero;
//! [`ParamFlags`] is paired with a `known` mask so a flag nobody reported reads as
//! `None` rather than `false`.
//!
//! Representable is not the same as unavoidable, though. A consumer should read a
//! parameter through [`ParameterInfo`]'s own accessors ([`bounds`], [`default_value`],
//! [`step_count`], [`flag`]) — the variants exist for the host that
//! must produce them, not as the reading path. A range is data for display: the
//! plain value of a normalized one is the format's answer
//! ([`AnyThread::parameter_plain`]: each format's own [`ParamMapping`], held to its
//! laws by `contract::mapping_contract!` under the `contract` feature), never a
//! formula over `bounds`.
//!
//! ## Example
//!
//! Two format hosts describe one parameter each. The shapes differ where the ABIs
//! differ, and the reading path is the same for both:
//!
//! ```rust
//! use tutti_plugin_types::{ParamFlags, ParamSteps, ParameterInfo};
//! use tutti_units::ParamId;
//!
//! // VST3/CLAP/AU: a plugin-chosen handle, often a hash of the name. Not an
//! // index, not dense, not ordered.
//! let cutoff = ParameterInfo::new(ParamId::new(0x8000_0001), "Cutoff")
//!     .with_plain_range(20.0, 20_000.0, 440.0)
//!     .with_steps(ParamSteps::Continuous)
//!     .with_flags(ParamFlags::AUTOMATABLE, ParamFlags::AUTOMATABLE);
//!
//! // VST2: the id is the parameter's position in `[0, numParams)`, and a
//! // plugin that declined `effGetParameterProperties` declared no range at all.
//! let mix = ParameterInfo::new(ParamId::new(3), "Mix")
//!     .with_normalized_default(0.5);
//!
//! // An absent declaration is `None`, never a substituted number.
//! assert_eq!(cutoff.bounds(), Some((20.0, 20_000.0)));
//! assert_eq!(mix.bounds(), None);
//! assert_eq!(mix.step_count(), None); // `Unknown`, not "continuous"
//! assert_eq!(mix.flag(ParamFlags::AUTOMATABLE), None); // unreported, not `false`
//!
//! // The id is the plugin's own number, and the type has no arithmetic: a host
//! // hands it back, it does not count with it.
//! assert_eq!(cutoff.id(), ParamId::new(0x8000_0001));
//! assert_eq!(mix.id().get(), 3);
//!
//! // The seam speaks normalized `0..=1`; what a value is in the plugin's own unit
//! // is asked of the format (`AnyThread::parameter_plain`), not computed here.
//! assert_eq!(cutoff.default_value(), 440.0);
//! ```
//!
//! ## `Normalized::clamped` is silent; `Normalized::new` refuses
//!
//! `Normalized::new` returns `None` for anything off `0..=1`, NaN included, and so
//! does deserializing one. `Normalized::clamped` saturates out-of-range input at the
//! nearest bound and maps NaN to `0.0`, returning no error and logging nothing.
//! Encode ordering data — or any plain value — through it and every entry above
//! `1.0` becomes `1.0`, which reads downstream as a legitimate sweep pinned at
//! maximum rather than as bad input. Normalize before constructing:
//!
//! ```rust
//! use tutti_units::Normalized;
//!
//! // A plain 20 kHz cutoff, handed to the normalized seam.
//! assert_eq!(Normalized::clamped(20_000.0).get(), 1.0);
//!
//! // Three distinct positions collapse onto one value, in silence.
//! let ranks: Vec<f64> = [7.0, 42.0, 79.0]
//!     .into_iter()
//!     .map(|r| Normalized::clamped(r).get())
//!     .collect();
//! assert_eq!(ranks, [1.0, 1.0, 1.0]);
//!
//! // Divide by the span first, and the positions survive.
//! let span = 79.0;
//! let ranks: Vec<f64> = [7.0, 42.0, 79.0]
//!     .into_iter()
//!     .map(|r| Normalized::clamped(r / span).get())
//!     .collect();
//! assert!(ranks[0] < ranks[1] && ranks[1] < ranks[2]);
//! ```
//!
//! ## Where it sits
//!
//! Depends on `tutti-units` ([`Samples`]), `tutti-buffers` ([`ChannelLayout`],
//! [`ChannelTopology`]), `tutti-music` (the meter vocabulary), `tutti-graph` (`Offset`), `tutti-rt` and `tutti-ump`,
//! re-exporting the parts format crates need so none of them takes its
//! own dependency for the type names.
//! All four format hosts and `tutti-plugin` depend on it.
//!
//! ## Features
//!
//! `default = []`.
//!
//! - `serde` — `Serialize`/`Deserialize` on the wire-eligible types
//!   ([`TransportInfo`], [`ParameterChanges`],
//!   [`EditorCapabilities`], [`WindowHandle`], and the metadata that rides the
//!   plugin-load reply). Required by the plugin IPC protocol (postcard); a host
//!   crate that does no IPC does not pay for the dependency.
//! - `contract` — the `contract` module: `mapping_contract!`, the laws a
//!   [`ParamMapping`] is held to. Test support, off by default; a format host turns
//!   it on in its dev-dependency.
//!
//! [`PluginDescriptor`]: crate::PluginDescriptor
//! [`PluginWiring`]: crate::PluginWiring
//! [`ParameterInfo`]: crate::ParameterInfo
//! [`ParamId`]: tutti_units::ParamId
//! [`PluginParamRange`]: crate::PluginParamRange
//! [`ParamFlags`]: crate::ParamFlags
//! [`ParamSteps`]: crate::ParamSteps
//! [`ProcessContext`]: crate::ProcessContext
//! [`ProcessOutput`]: crate::ProcessOutput
//! [`TransportInfo`]: crate::TransportInfo
//! [`ParameterChanges`]: crate::ParameterChanges
//! [`WindowHandle`]: crate::WindowHandle
//! [`EditorUnit`]: crate::EditorUnit
//! [`EditorCapabilities`]: crate::EditorCapabilities
//! [`Features`]: crate::Features
//! [`bundle`]: crate::bundle
//! [`Samples`]: crate::Samples
//! [`ChannelLayout`]: crate::ChannelLayout
//! [`ChannelTopology`]: crate::ChannelTopology
//! [`bounds`]: crate::ParameterInfo::bounds
//! [`default_value`]: crate::ParameterInfo::default_value
//! [`step_count`]: crate::ParameterInfo::step_count
//! [`flag`]: crate::ParameterInfo::flag
//! [`AnyThread::parameter_plain`]: crate::AnyThread::parameter_plain
//! [`ParamMapping`]: crate::ParamMapping
#![cfg_attr(docsrs, feature(doc_cfg))]
// One `unsafe` item: `WindowHandle::from_u64`, the constructor whose caller
// vouches for a window's bare platform number.
#![deny(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

mod answer;
mod any_thread;
mod automation;
mod automation_mode;
mod bridge_error;
pub mod bundle;
mod bypass;
mod c_string;
mod channels;
mod classification;
#[cfg(feature = "contract")]
pub mod contract;
mod control_role;
mod deadline;
mod descriptor;
mod driver;
mod edit_slots;
mod editor;
mod error;
mod failure;
#[cfg(any(test, feature = "testing"))]
pub mod fake;
pub mod features;
mod format;
mod guard;
mod harmony;
mod host;
mod hosting;
mod invalidation;
mod lifecycle;
mod load_stage;
mod locate;
mod main_thread;
mod measured;
mod metadata;
mod midi;
mod midi_intake;
mod note_expression;
mod note_id;
mod note_name;
mod order;
mod param_edit;
mod param_mapping;
mod param_unit;
mod parameters;
mod pipeline;
mod plugin_ref;
mod points;
mod presets;
#[cfg(feature = "probe-wire")]
mod probe_exit;
#[cfg(feature = "probe-wire")]
mod probe_failure;
#[cfg(feature = "probe-wire")]
mod probe_wire;
mod process;
mod process_wake;
mod rate_refusal;
mod render_mode;
mod reports;
mod requests;
mod server_error;
mod state_frame;
mod state_use;
mod sysex;
mod timeouts;
mod track_info;
mod transport;
mod transport_request;
mod version;
mod wait_switch;
mod wakeups;
mod widths;
mod wire_error;
mod written_values;

// Test-only global allocator for the no-alloc gates: panics on any heap
// allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

pub use tutti_buffers::MaxBlock;
pub use tutti_buffers::{ChannelLayout, ChannelTopology, Speaker};
pub use tutti_rt::{AtomicRequestFlags, RequestFlag, RequestFlags};
/// Re-exported so every format crate spells a sample count the same way
/// without each taking its own `tutti-units` dependency — `ChannelLayout`
/// above is here for the same reason.
pub use tutti_units::{SampleRate, Samples};

mod layout_support;
pub use layout_support::LayoutSupport;
// Musical vocabulary carried on `TransportInfo`. Re-exported for the same reason
// as `ChannelLayout`: format hosts speak these at their ABI boundary and should
// not need a `tutti-music` dependency of their own to name them.
pub use tutti_music::{BeatsPerBar, NoteValue, TimeSignature};
pub use tutti_units::{BarNumber, Seconds};

pub use answer::{Answer, AnswerSender};
pub use any_thread::{AnyThread, Health};
pub use automation::{ParameterChanges, ParameterPoint, ParameterQueue};
pub use automation_mode::{AutomationMode, AutomationModeError};
pub use bridge_error::{IpcError, IpcResult, NoEditorHost, PluginForkError};
pub use c_string::{
    c_str_prefix, read_c_chars, read_c_str, read_utf16_str, write_c_chars, write_c_str,
    write_utf16_str, write_utf16_units,
};
pub use channels::{ChannelTable, MAX_BLOCK_CHANNELS, PluginTooWide, sanitize, silence};
pub use classification::{ClapFeature, PluginRole, Vst2Category, Vst3PlugType, Vst3SubCategories};
pub use descriptor::{AuComponentType, Described, EditorPresence, PluginClass, PluginDescriptor};
pub use driver::{Hand, Lifecycle, Seat, Successor};
pub use edit_slots::{EditReader, EditWriter, UnknownParam, edit_slots, epoch_before};
pub use editor::{
    AspectRatio, ContentScale, EditorCapabilities, EditorError, EditorMirrorError, EditorOpened,
    EditorUnit, InvalidContentScale, KeyEvent, KeyUse, ModifierConvention, ResizeHints,
    WindowHandle,
};
pub use failure::{PluginFailure, TerminationReason};
pub use format::{FourCc, PluginFormat, PluginId, PluginIdError, Uid, UnknownExtension};
pub use guard::{contained_panics, ffi_guard};
pub use host::{
    Control, Controller, Loaded, PluginIo, Processor, Rebuilt, Refused, Stuck, Wraps, process_block,
};
pub use hosting::{Describe, Editor, EditorWindow, InProcess, OutOfProcess, Reopen};
pub use invalidation::{PluginInvalidation, PluginRefresh, ResyncClass, ResyncKind};
pub use order::{StableOrder, insertion_sort_by_key};
pub use plugin_ref::{PluginRef, canonical_path};
pub use points::{PointBuf, PointPool};
pub use process_wake::ProcessWake;
pub use rate_refusal::{RateRefusal, RateRefusalCause};
pub use reports::{ReportDrain, ReportSink, reports};
pub use requests::taken;
pub use server_error::ServerError;
pub use state_frame::{
    BareFrame, FrameReader, MAX_STATE_BYTES, Opened, StateFrame, StateFrameError,
};
pub use state_use::StateUse;
pub use track_info::{TrackInfo, TrackKind};
pub use transport_request::TransportRequest;
pub use version::{EmptyVersionText, PluginVersion, VersionText};
// Exported as `PluginResult` only: the bare name `Result` shadows std's under a
// glob import.
pub use deadline::Deadline;
pub use error::{
    Delivered, PluginError, PluginLearnError, PresetError, Result as PluginResult, StateError,
};
pub use features::{FeatureReport, Features};
pub use harmony::{
    ChordChanges, ChordValue, HARMONY_CAPACITY, NoteExpressionIntChanges, NoteExpressionIntValue,
    NoteExpressionTextChanges, NoteExpressionTextValue, ScaleChanges, ScaleValue,
};
pub use lifecycle::{
    EFFECT_CAPACITY, Effect, Event, FDS_PER_TICK, HostOp, MAX_STEP_EFFECTS, Notice, Prepare, Reply,
    Request, RescanScope, RestartRules, Stage, State, TIMERS_PER_TICK, TimerId, standing, step,
};
pub use load_stage::LoadStage;
pub use main_thread::{MainThreadMarker, MainWork, MainWorkHandle};
pub use measured::{
    LatencyVerdict, Measured, NotChecked, PROBE_FLOOR, ProbeConfig, StateHash, TailVerdict, Verdict,
};
pub use metadata::{
    BusChannels, Layout, MAX_LAYOUT_BUSES, PluginBus, PluginBuses, PluginTail, PluginWiring,
    Processing,
};
pub use midi::{
    MIDI_STACK_BOUND, MIDI_STACK_CAPACITY, MidiEventVec, RT_MIDI_CAPACITY, RtMidiEvents, WireMidi,
    group_of_port,
};
pub use midi_intake::MidiIntake;
pub use timeouts::{Timeouts, ZeroTimeout};
pub use tutti_graph::{BusRole, Offset};
// In the public `IpcError::Slab`, so a host matches it without depending on ipc-ffi.
pub use tutti_plugin_ipc_ffi::SlabError;
pub use wakeups::Wakeups;
pub use wire_error::{FrameTooLarge, ResultError, SlabAccessError, SlabLayoutError};
// `NoteExpressionVec` is the alias the change list is built from, so a caller
// assembling one has to name it.
pub use bypass::{BypassWrite, OwnBypass, bypass_parameter, bypass_value, is_bypassed};
pub use control_role::{Axis, ControlRole, MidiAssignment, MidiMapApplied, MidiSource};
pub use locate::{Helper, Locator};
pub use note_expression::{
    NoteExpressionChanges, NoteExpressionType, NoteExpressionValue,
    expression_of_per_note_controller, per_note_message, per_note_tuning,
};
pub use note_id::{note_id_to_channel_note, plugin_note_id};
pub use note_name::{NoteName, ParamGroup};
pub use param_edit::ParamEdit;
pub use param_mapping::{LinearRange, ParamMapping, PlainMap};
pub use param_unit::{ParamUnit, PlainUnit};
pub use parameters::{
    ParamAccess, ParamFlags, ParamSteps, ParamWriteError, ParameterInfo, PluginParamRange,
};
pub use pipeline::{
    BridgeLosses, ChunkCounters, ChunkOutcomes, MAX_CHUNK, PipelineCounts, ServerLosses,
};
pub use presets::{PluginPreset, PresetId, PresetSupport};
#[cfg(feature = "probe-wire")]
pub use probe_exit::{ProbeDeath, ProbeExit};
#[cfg(feature = "probe-wire")]
pub use probe_failure::{MeasureFailure, ProbeError, ProbeRefusal, Unmeasured};
#[cfg(feature = "probe-wire")]
pub use probe_wire::{ProbeOutcome, read_result, write_result};
pub use process::{BlockStatus, ExpressiveContext, ProcessContext, ProcessOutput};
pub use render_mode::RenderModeSupport;
pub use sysex::{SYSEX_MESSAGE_CAPACITY, push_sysex, sysex_assembler, sysex_packets};
pub use transport::{
    BarInfo, LoopRegion, MusicalTiming, PlayheadWatch, TransportChange, TransportChanges,
    TransportFlags, TransportInfo, TransportPosition,
};
pub use wait_switch::{WaitArmed, WaitSwitch};
pub use widths::Widths;
pub use written_values::WrittenValues;
// The session's render mode, which a plugin's `Prepare` carries and `Control::set_render_mode` takes.
pub use tutti_graph::RenderMode;
