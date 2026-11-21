#![doc = include_str!("../README.md")]
//! The tutti engine's API layer (design 015): the cross-crate composition.
//!
//! A [`Session`] is a [`Graph`] (a `tutti-graph` editor, the nodes inserted
//! through it, their event feeds), its transport, and the engine side it
//! renders on, built by a [`SessionBuilder`]. Its type parameter is its
//! [`Clock`], what runs its blocks: [`Offline`] (the host pulls blocks with
//! [`Session::render`]), `DeviceClock` (`device`: a sound card's callback; the
//! `!Send` `Device` stays on the thread that opened it) or [`SystemClock`] (an
//! OS timer thread, for an app that plays MIDI); a call a clock cannot serve
//! is not a method of its session. One call, [`Session::update`], runs the
//! steps of a frame in [`PHASES`] order.
//!
//! ```
//! use tutti_buffers::MaxBlock;
//! use tutti_units::{Hz, SampleRate, Samples};
//! use tutti_buffers::ChannelLayout;
//! use tutti_buffers::InterleavedMut;
//! use tutti_graph::ForkByClone;
//! use tutti_nodes::testing::Osc;
//! use tutti_session::{Root, Session};
//!
//! let mut s = Session::builder()
//!     .outputs_floor(ChannelLayout::MONO)
//!     .outputs(ChannelLayout::MONO).max_block(MaxBlock::of(Samples::new(512))).build()
//!     .unwrap();
//! let (osc, _) = s.graph().add(ForkByClone(Osc::sine(Hz::new(1_000.0))));
//! s.graph().connect(osc, Root).unwrap();
//! s.update(); // commits: the graph was edited
//!
//! let mut block = vec![0.0f32; 512];
//! s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO).unwrap());
//! assert!(block.iter().any(|&x| x > 0.9));
//! ```
//!
//! # The two loops
//!
//! The control thread owns the [`Session`]. Each frame it calls
//! [`Session::update`], which runs the [`Phase`]s of [`PHASES`] in order and
//! returns the [`SessionEvent`]s they raised: [`Phase::Collect`] lands what
//! the session's jobs sent and frees what the audio thread retired,
//! [`Phase::Commit`] compiles and sends an edited graph, and the phases around
//! them serve plugins, modulation, MIDI and reclamation.
//!
//! The audio thread runs the other loop: every block it renders the newest
//! plan it has been handed, without waiting for the control thread.
//!
//! Between the two there are queues and no mutable state. A commit travels
//! to the audio thread over the editor's queue, retired values travel back
//! over its return queue, and a job on the session's executor reports over a
//! channel that [`Phase::Collect`] drains. A call on the [`Graph`] only stages
//! an edit, in the graph value the control thread holds
//! ([`Graph::is_edited`] says whether one waits); nothing reaches the audio
//! thread until an update's commit sends it.
//!
//! Like `tutti-graph`, every module is private: everything is
//! `tutti_session::Item`. The crate re-exports no crate whole (`cargo xtask check-session`).

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "analyze")]
mod analyze;
mod apply;
#[cfg(feature = "export")]
mod bounce;
mod builder;
#[cfg(feature = "plugin")]
mod bypass;
mod clock;
#[cfg(feature = "device")]
mod device;
mod drive;
#[cfg(feature = "plugin")]
mod editor;
mod error;
#[cfg(feature = "export")]
mod fork;
mod format;
#[cfg(feature = "soundfont")]
mod general_midi;
mod graph;
#[cfg(feature = "plugin")]
mod hang;
#[cfg(feature = "capture")]
mod input;
#[cfg(feature = "midi")]
mod instrument;
mod job;
mod jobs;
mod key;
#[cfg(any(feature = "capture", feature = "midi"))]
mod latency;
#[cfg(all(feature = "sampler", codec))]
mod load_audio;
#[cfg(feature = "soundfont")]
mod load_soundfont;
#[cfg(any(
    all(feature = "sampler", codec),
    feature = "soundfont",
    feature = "plugin"
))]
mod loaded;
#[cfg(feature = "midi")]
mod midi;
#[cfg(feature = "midi-hardware")]
mod midi_ci;
#[cfg(feature = "midi")]
mod midi_follow;
#[cfg(feature = "midi-hardware")]
mod midi_hardware;
#[cfg(feature = "midi")]
mod midi_load;
#[cfg(feature = "midi-hardware")]
mod midi_port;
#[cfg(all(feature = "midi", not(feature = "midi-hardware")))]
#[path = "midi_port_none.rs"]
mod midi_port;
#[cfg(feature = "midi")]
mod midi_report;
#[cfg(feature = "midi")]
mod midi_sink;
mod modulation;
mod node;
#[cfg(feature = "midi")]
mod os_clock;
mod params;
mod peer;
#[cfg(any(feature = "plugin", feature = "midi-hardware"))]
mod peer_state;
#[cfg(feature = "plugin")]
mod plugin;
#[cfg(feature = "plugin")]
mod plugin_diagnostics;
#[cfg(feature = "plugin")]
mod plugin_params;
#[cfg(feature = "plugin")]
mod plugin_presets;
mod policy;
mod pool;
#[cfg(feature = "record")]
mod recorder;
#[cfg(feature = "plugin")]
mod recovery;
mod rt_log;
#[cfg(feature = "sampler")]
mod sampler;
mod session;
#[cfg(all(feature = "analyze", feature = "sampler"))]
mod signal_windows;
#[cfg(feature = "device")]
mod switch;
#[cfg(feature = "capture")]
mod take;
#[cfg(all(feature = "midi", feature = "analyze"))]
mod transcribe;
mod transport_handle;
mod value;
#[cfg(feature = "device")]
mod watch;

#[cfg(feature = "analyze")]
pub use analyze::{
    Analyze, AnalyzeError, Analyzer, AnalyzerError, Audio, AudioContext, AudioInput, BindError,
};
pub use apply::Apply;
#[cfg(feature = "export")]
pub use bounce::{
    AnyStem, Bounce, BounceConfigError, BounceError, BounceHandle, BounceJob, BounceLatency,
    BounceRendered, BounceTail, Bounced, BufferMaster, BufferStem, ExportClock, FileMaster,
    FileStem, Master, NoMaster, PrepareEdit, PreparedGraph, Range, Stem, StemGain,
};
pub use builder::{BlockConfig, SessionBuilder, Workers, root_width};
#[cfg(feature = "device")]
pub use clock::DeviceClock;
pub use clock::{
    Clock, Offline, OfflineSession, RealTime, SessionFloat, SystemClock, SystemSession,
};
#[cfg(feature = "testing")]
pub use clock::{Detached, DetachedSession};
#[cfg(feature = "device")]
pub use device::{
    Device, DeviceEvent, DeviceLatency, DeviceRestart, DeviceRestarted, LiveSession, RestartError,
    RestartOutcome,
};
#[cfg(feature = "plugin")]
pub use editor::{Editor, EditorCallError, Opened};
pub use error::{
    ConnectError, DeclarationError, Error, InletError, NoSuchNode, RemoveError, Result,
    SessionOutputTapError, TempoError,
};
#[cfg(feature = "midi")]
pub use error::{MidiError, RouteDeclarer};
#[cfg(feature = "soundfont")]
pub use general_midi::GeneralMidi;
pub use graph::ModKeys;
#[cfg(feature = "testing")]
pub use graph::{AudioSide, Testing};
pub use graph::{
    Committed, Crossfade, CrossfadeId, CrossfadeRefused, Feeder, Graph, LIVE_MAX_BLOCK, Meters,
    NotReopened, Reopened, ReplaceRefused, RootReader, Taps,
};
#[cfg(feature = "capture")]
pub use input::{Input, InputConfig, InputOpener, OpenedInput};
#[cfg(feature = "midi")]
pub use instrument::{AddedInstrument, AnyNode, InstrumentHandle, NotInstrument};
pub use job::{Job, JobId, JobKind, JobOutput, JobStatus, WaitError, WorkerError};
pub use key::{
    Bus, BusSink, Channel, Connection, Edge, Event, EventEdge, EventInPort, EventOutPort,
    EventTapPort, Fanin, InPort, Inlet, Key, MainBus, MainSink, OutPort, Outlet, Root,
    RootDeclaration, Weighted, Wire,
};
#[cfg(any(feature = "capture", feature = "midi"))]
pub use latency::{Start, TakeLatency, Trim};
#[cfg(all(feature = "sampler", codec))]
pub use load_audio::{AudioData, LoadAudio, LoadedAudio, Tier};
#[cfg(feature = "soundfont")]
pub use load_soundfont::{LoadSoundFont, LoadedSoundFont};
#[cfg(feature = "midi")]
pub use midi::{Midi, MidiRouteId, MidiRouteRule, Stamping};
#[cfg(feature = "midi-hardware")]
pub use midi_ci::{
    ChannelProfile, GetProperty, ListedProfile, ListedResource, PeFailure, ProfilePeer, ReportPeer,
    RequestReport, ResourcePeer, SetProperty, Subscribe, Subscription, SubscriptionCause,
    SwitchProfile, WholeProfile,
};
#[cfg(feature = "midi")]
pub use midi_follow::{FollowClock, FollowMachineControl, FollowTempo};
#[cfg(feature = "midi")]
pub use midi_load::{
    DroppedEvent, LoadMidi, LoadReport, LoadedMidi, LoadedTrack, NoCommonTimelineError, TrackError,
};
#[cfg(feature = "midi-hardware")]
pub use midi_port::{
    MidiInput, MidiInputInfo, MidiOutput, MidiOutputInfo, MidiPortError, NoHardware,
    OpenMidiOutput, PortCause,
};
#[cfg(feature = "midi")]
pub use midi_report::MidiReport;
#[cfg(feature = "midi")]
pub use midi_sink::{MidiSink, OpenMidiSink};
pub use modulation::{
    AutomationTake, MixRef, ModRefusal, ModRouteId, ModSourceId, ModState, ModTarget, Mods,
    ModsView, NoAutomation, NoSuchModRoute, NoSuchModSource, ParamRef, Problem, Ranged, RouteError,
    RouteInfo, RouteSpec, SourceInfo, SourceName, TargetName, declare_targets,
};
pub use node::{Reclaimer, Reclaims, SessionNode};
pub use peer::{Peer, PeerError, Presence, Resource, ResourceFact, ResourceId};
#[cfg(any(feature = "plugin", feature = "midi-hardware"))]
pub use peer_state::{OtherPeer, PeerState, SavedPeer};
#[cfg(feature = "plugin")]
pub use plugin::{
    AddPluginError, AnyPlugin, LoadPlugin, LoadPreset, LoadedPlugin, LoadedPreset, Plugin,
    PluginApplyError, PluginCause, PluginChange, PluginEvent, PluginHealth, PluginInfo, PluginNode,
    PluginPorts, PortsRefusal, PresetStartError, RestoreState, RestoredState, SaveState,
    StateRestore, StateStartError, StateView, SwapPluginError,
};
pub(crate) use pool::Pool;
pub use pool::{
    Declared, HandleError, IntoPool, Life, LoadError, OpenCx, Owned, PoolConfig, PoolEntry,
    PoolError, PoolEvent, PoolJobError, PoolKey, PoolLoad, PoolLoaded, PoolOutcome, PoolReport,
    PoolSet, PoolState, Retention, RetryError, Sharing, WhenLoadedError, stranded_loads,
};

#[cfg(feature = "plugin")]
use hang::HangDetector;
#[cfg(feature = "plugin")]
pub use hang::HangPolicy;
#[cfg(feature = "midi-hardware")]
pub use midi_hardware::{MidiPolicy, MidiPolicyError};
#[cfg(feature = "plugin")]
pub use plugin_diagnostics::{Diagnostics, PluginBuses};
#[cfg(feature = "plugin")]
pub use plugin_params::{ParamGesture, PluginParam, PluginParams};
#[cfg(feature = "plugin")]
pub use plugin_presets::{ListedPreset, PluginPresets};
pub use policy::{OutputPolicy, OutputPolicyError, Policy};
#[cfg(feature = "record")]
pub use recorder::{
    FinalizeError, Finite, IDLE_PARK, Live, PumpLoop, PumpPass, Recorder, RecorderError,
};
#[cfg(feature = "plugin")]
pub use recovery::{
    GiveUp, PortCounts, Recovery, RecoveryAction, RecoveryEvent, RecoveryFailure, RecoveryPolicy,
    ReloadCause,
};
#[cfg(feature = "sampler")]
pub use sampler::{OpenStream, OpenStreamError, OpenedStream, SamplerError};
#[cfg(feature = "midi")]
pub use session::MidiNodes;
pub use session::ModInput;
pub use session::{
    AppKey, CrossfadeControls, PHASES, Phase, PhaseCx, RootTopologyStatus, Session, SessionEvent,
};
#[cfg(all(feature = "analyze", feature = "sampler"))]
pub use signal_windows::windows_from_signal;
#[cfg(feature = "device")]
pub use switch::ClockError;
#[cfg(feature = "capture")]
pub use take::{
    Armed, InFlight, Punch, PunchError, Recorded, Rolling, Take, TakeEnd, TakeError, TakeEvent,
    TakeId, TakePlacement, TakeTarget, Window,
};
#[cfg(all(feature = "midi", feature = "analyze"))]
pub use transcribe::{LevelVelocity, NoteCents, NoteMapping, clip_from_notes, harmony_from_chords};
pub use transport_handle::TransportHandle;
#[cfg(any(feature = "record", feature = "export"))]
pub use tutti_dsp::DitherShape;
#[cfg(feature = "record")]
pub use tutti_dsp::{DitherSink, NoiseShaping};
pub use tutti_engine::{BlockDates, OutputPlacement, OutputTapOpen, Playhead};
#[cfg(feature = "export")]
pub use tutti_export::{
    EventLossPolicy, Normalize, Outcome, Produced, Progress, Rendered, Resample, Stage, Written,
};
pub use tutti_graph::{
    BusError, BusRef, BusSource, Buses, HasParam, HasParams, MissingSource, ParamUnit, Scalar,
};
pub use tutti_graph::{
    CloseError, CommitError, EventCapacity, Losses, NodeMode, PassError, Stamp, Wired, WiredMask,
};
#[cfg(feature = "midi")]
pub use tutti_ump::{MidiChannel, MidiGroup};
#[cfg(feature = "midi-hardware")]
pub use value::MidiPolicyTarget;
pub use value::OutputPolicyTarget;
pub use value::{
    AnyAssign, AnyTarget, Assign, AutomationTarget, ClickTargets, ClickVolumeTarget, CurveTarget,
    DepthTarget, Effective, GainTarget, GainTargetError, Info, IntoAssign, IntoRead, LoopTarget,
    MacroTarget, MeterTarget, MixTarget, ModeTarget, Modify, Modulatable, NodeSlots, ParamTarget,
    PolarityTarget, RangeTarget, RateTarget, ScalarTarget, SetRefusal, SettingTarget, Target,
    TargetInfo, TempoTarget, ValueError, ValueErrorKind, Values,
};
#[cfg(feature = "midi")]
pub use value::{ClipEventsTarget, HarmonyTarget, MpeModeTarget, OutputLatencyTarget};
#[cfg(feature = "sampler")]
pub use value::{DirectionTarget, LoopingTarget, SpeedTarget, WindowTarget};
#[cfg(feature = "plugin")]
pub use value::{
    PluginAutomationModeTarget, PluginParamRefusal, PluginParamTarget, PluginUnit, UnitError,
};
#[cfg(feature = "device")]
pub use watch::{Backoff, DevicePolicy, DeviceWatch, Follow, FollowEvent};
