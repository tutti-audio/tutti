#![doc = include_str!("../README.md")]
//! The Tutti real-time audio engine behind one dependency.
//!
//! Tutti is a set of focused crates: a graph compiler and executor, a transport,
//! a device layer, DSP nodes, samplers and synths, MIDI, plugin hosting, offline
//! export. This crate names the part an application uses, and cargo features turn
//! subsystems on. A program that wires the pieces itself (a backend that drives
//! an engine per block, a node library, a codec tool) depends on the crate that
//! owns them.
//!
//! ```toml
//! [dependencies]
//! tutti = { version = "0.0.1", features = ["device", "decode-wav"] }
//! ```
//!
//! ## Quick start
//!
//! A [`Session`] holds a graph, its transport and the engine that renders it. An
//! offline session needs no device: [`Session::render`] does what the audio
//! callback does once per buffer.
//!
//! ```rust
//! use tutti::nodes::{LfoNode, LfoShape};
//! use tutti::{ChannelLayout, Hz, InterleavedMut, Root, SampleRate, Session};
//!
//! let mut session = Session::new(SampleRate::SR_48K).expect("a session builds");
//!
//! // A 440 Hz sine into the session's output. The node's own controls come back
//! // with its key; they set its parameters live.
//! let (tone, _controls) = session
//!     .graph()
//!     .add(LfoNode::new(LfoShape::Sine).with_rate(Hz(440.0)));
//! session.graph().connect(tone, Root).expect("both ends exist");
//!
//! // An update commits the edit; it lands at the start of the next block, and
//! // reports a fault or a dropout as an event.
//! assert!(session.update().is_empty(), "the commit raised no event");
//!
//! // One interleaved stereo block.
//! let mut out = vec![0.0f32; 256 * 2];
//! session.render(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap());
//! assert!(out.iter().any(|&s| s != 0.0), "the sine is audible");
//! ```
//!
//! To play through a sound card, turn on `device` and build a
//! `SessionBuilder::<DeviceClock>`: it opens the device and builds the graph at
//! the rate the device reports, and hands back a `LiveSession`, the `Session` on
//! the sound card's clock (dropping it stops the stream).
//! `examples/headless_engine.rs` plays a tone in under twenty lines. To write a
//! file instead, turn on `export` and bounce the session: a `Bounce` names a
//! range and a master, and optionally stems, and `Session::bounce` renders it
//! (`examples/headless_export.rs`). `examples/song_bounce.rs` is the whole common
//! path from one `use tutti::{..}`: a sampler `Clip` into a filter into a
//! reverb, an LFO on the cutoff, `Session::play`, and eight beats bounced to a
//! WAV, with `?` through the one `tutti::Error`.
//!
//! ## Where a name lives
//!
//! Every item is reachable at one path. What an application names is at the
//! crate's root, so one `use tutti::{Session, SessionEvent, Root, Samples};`
//! covers a program. The rest is in a module that groups it by what it is, each
//! item listed by name:
//!
//! | module | what it holds | feature |
//! |---|---|---|
//! | `tutti::nodes` | the node catalogue: oscillators, filters, delays, dynamics, reverbs, mixing, panners, automation lanes, with their configs, controls and enums | always |
//! | `tutti::types` | the unit vocabulary and edge traits not at the root: `Moment`, `Note`, `Param`, `Tail`, `Latency`, `Source`, `Sink`, `Pull`, layouts, channel folds | always |
//! | `tutti::tempo` | the project's tempo map and the pieces it is built from | always |
//! | `tutti::graph` | what a node author names: `Node`, `Shape`, `Io`, `Prepare`, `Env`, `ForkByClone`, `Unforkable`, `declare_params!` | always |
//! | `tutti::session` | the session's host plumbing: phases, recovery, main-thread plugin sessions, the pool, handles and errors a host reads | `session` |
//! | `tutti::midi` | the common MIDI path: files, messages, keys, routes, devices, profiles | `midi` |
//! | `tutti::codec` | audio file settings: `Spec`, `Quality`, `FlacLevel`, `IntDepth` and the decode errors | `codec` |
//! | `tutti::sampler` | sample playback voices and loops | `sampler` |
//! | `tutti::modulation` | modulation sources, curves and their errors | `modulation` |
//! | `tutti::polysynth` | the polyphonic synth node and its configs | `synth` |
//! | `tutti::soundfont` | the SoundFont playback node and its settings | `soundfont` |
//! | `tutti::plugin` | hosting settings, the main-thread proof and the plugin catalog | `plugin` + a format |
//!
//! `cargo xtask check-umbrella` holds the rule: no crate is re-exported whole,
//! and no name is exported twice.
//!
//! ## Features
//!
//! `session` is on by default.
//!
//! - `full` — everything below except `jack` and the plugin features.
//! - `device` — the sound card: `SessionBuilder::<DeviceClock>`, `AudioHost`,
//!   `DeviceId`. Off by default so an offline render, a CI job or a server builds
//!   without ALSA or CoreAudio headers.
//! - `jack` — JACK output through `tutti-cpal` (links libjack; implies `device`).
//! - `decode-wav`, `decode-flac`, `decode-mp3`, `decode-ogg` — audio file decoders (each implies `codec`, and
//!   enables the same decoder in the sampler when `sampler` is on).
//! - `encode-wav`, `encode-flac`, `encode-aiff`, `encode-vorbis` — the encoders
//!   a `Format` names (each implies `codec`; none implies `export`, and
//!   `encode-vorbis` is the one path to a C library).
//! - `codec` — `tutti::codec`, `Wave`. Implied by every decode and encode feature.
//! - `io` — the session's `Recorder` and its pump loop. Implied by `audio-io`.
//! - `audio-io` — mic capture and recording (implies `io`; the capture stream
//!   itself needs `device`).
//! - `export` — `Bounce` and its results (implies `encode-wav`).
//! - `midi` — `tutti::midi`. `crates/tutti/examples/midi_all.rs` uses every piece together, offline.
//! - `midi-hardware` — OS MIDI I/O in `tutti::midi` (implies `midi`).
//! - `synth`, `soundfont`, `sampler`, `modulation` — the matching module.
//!   `soundfont` implies `midi`; `sampler` implies `decode-wav`.
//! - `plugin` — `tutti::plugin` (implies `midi`), with one or more formats:
//!   `vst2`, `vst3`, `clap`, `au`. Each format links its SDK, which is why none
//!   is in `full`. The out-of-process plugin server is a separate binary,
//!   `tutti-plugin-server`, built on its own.
//!
//! ## This crate holds no code
//!
//! Every item is a `pub use` of another crate, and the crate's own test enforces
//! it. A convenience constructor or wrapper belongs in the crate that owns the
//! thing it wires, where it is available to every consumer.
#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// --- the application's names, at the root -----------------------------------
//
// What an application names, listed here by name, so a program imports from one
// place: `use tutti::{Session, SessionEvent, Root, Samples};`. There is no
// prelude and no glob: this file is the surface. Each group is behind the
// feature that gates it. A name here is in no module below. `Result`, `Sample`
// and `Unit` are not here: each would shadow a name a program already has.

// The one error a common-path call returns: decode, sampler placement, wiring,
// transport, bounce and the rest each convert into it, so
// `fn main() -> Result<(), tutti::Error>` takes `?` across a whole program.
#[cfg(feature = "session")]
pub use tutti_session::Error;

// The graph's vocabulary a session hands out.
pub use tutti_graph::{
    DECLICK, EventCapacity, EventInletNode, EventInletWriter, EventTapId, EventTapReader, FadeLaw,
    Level, Losses, MeterReader, NodeMode, Refused, Stamp, TapAlign, TapEvent, TapId, TapReader,
};
pub use tutti_metering::{LoadMeter, LoadReading};
pub use tutti_transport::{FadeOut, LoopRegion, MotionEvent, MotionState, Then, TransportCommand};

// Units, channels, buffers and the musical meter.
pub use tutti_buffers::{Interleaved, InterleavedMut};
pub use tutti_types::{
    Amplitude, At, BarNumber, Beat, Bpm, Cents, ChannelLayout, ChannelTopology, Db, Depth, Frame,
    Hz, MaxBlock, Meter, MeterMap, Q, SampleRate, Samples, Seconds, Semitones, TimeSignature,
};

// The device handle and a device's identity.
#[cfg(feature = "device")]
pub use tutti_cpal::{AudioHost, DeviceId};

// The sampler's clip.
#[cfg(feature = "sampler")]
pub use tutti_sampler::Clip;

// A decoded wave, and what a bounce writes.
#[cfg(feature = "codec")]
pub use tutti_codec::Wave;
#[cfg(feature = "export")]
pub use tutti_codec::{Format, Pcm};

// The session, its wiring and its requests.
#[cfg(feature = "session")]
pub use tutti_graph::NodeKey;
#[cfg(feature = "modulation")]
pub use tutti_modulation::{Follower, Lfo};
#[cfg(feature = "plugin")]
pub use tutti_plugin::catalog::{Catalog, Hosting, PluginRef, PluginState};
#[cfg(feature = "plugin")]
pub use tutti_plugin::{Normalized, ParamId, PluginFailure, PresetError, PresetId};
#[cfg(all(feature = "session", feature = "plugin"))]
pub use tutti_session::{
    AddPluginError, Diagnostics, LoadError, Opened, Params, Plugin, PluginBuses, PluginChange,
    PluginDeclaration, PluginEvent, PluginHealth, PluginInfo, PluginNode, PluginParamError,
    PoolEvent, PoolState, PooledPlugin, PortCounts, PresetEntry, PresetKind, Presets, TypedParam,
};
#[cfg(all(feature = "session", feature = "export"))]
pub use tutti_session::{
    Bounce, BounceError, BounceLatency, BounceTail, Bounced, Range, Stem, StemGain, Written,
};
#[cfg(feature = "session")]
pub use tutti_session::{
    Clock, ConnectError, Edge, EventInPort, EventOutPort, InPort, Job, JobFailed, Key, NoSuchNode,
    Offline, OfflineSession, OutPort, Playhead, RealTime, Root, ScheduleError, Session,
    SessionBuilder, SessionEvent, SystemClock, SystemSession, TypedParamRef, Workers,
};
#[cfg(all(feature = "session", feature = "device"))]
pub use tutti_session::{Device, DeviceClock, DeviceEvent, LiveSession};
#[cfg(all(feature = "session", feature = "device", feature = "audio-io"))]
pub use tutti_session::{
    Input, InputConfig, Punch, Recorded, Take, TakeError, TakeEvent, TakeTarget, Trim,
};
#[cfg(all(feature = "session", feature = "modulation"))]
pub use tutti_session::{ParamRef, RouteSpec};
#[cfg(all(feature = "session", feature = "sampler"))]
pub use tutti_session::{SamplerEvent, SamplerLoad};

// --- the modules ------------------------------------------------------------

/// The node catalogue (`tutti-nodes`): oscillators, LFOs, filters, delays,
/// dynamics, reverbs, modulation effects, mixing, panners, convolution and
/// automation lanes, each with its config, controls and enums. A node is
/// inserted with `Session::graph().add(node)`.
pub mod nodes {
    pub use tutti_nodes::automation::{
        AutomationLaneNode, Breakpoint, ControlLaneNode, LaneControls, LiveAutomationLane,
        RecordMode, RecordingConfig, Trajectory,
    };
    pub use tutti_nodes::{
        BAND_PARAMS, BrickwallLimiterNode, BusStripNode, ChannelTimes, ChannelTimesError,
        ClickNode, ClickSettings, ClickState, CompressorControls, CompressorNode, ConvolverError,
        ConvolverNode, CrossoverControls, CrossoverError, CrossoverNode, DelayControls,
        DelayLayout, DelayNode, DistortionNode, EnvelopeFollowerNode, EqBandNode, FdnControls,
        FdnLayout, FdnNode, FreeverbControls, FreeverbNode, GateControls, GateNode,
        HrtfBinauralNode, InterpolationMode, IrLayout, KeySource, LadderControls, LadderFilterNode,
        LadderType, LfoClock, LfoNode, LfoShape, LimiterControls, LimiterNode, Lookahead, MaxDecay,
        MaxDelay, MaxDelayError, MetronomeMode, ModDelayControls, ModDelayNode, ModulatorNode,
        MultibandCompressorNode, MultibandControls, NoteEnvelopeNode, OscillatorNode,
        PhaserControls, PhaserNode, PortSide, Routing, RoutingError, SOURCE_EVENT_CAPACITY,
        ShapeKind, SourceNode, SourcePorts, Stages, StagesError, Stagger, StaggerError,
        SvfControls, SvfFilterNode, SvfType, Sweep, SweepError, SweepRange, SweepRangeError,
        VbapError, VbapPannerNode, VolumeOutOfRange, Waveform, generate_room_ir,
        generate_room_ir_into, generate_test_ir, generate_test_ir_into,
    };
}

/// The unit vocabulary (`tutti-types`, `tutti-buffers`) a program names less
/// often than the root's units: moments and notes, the parameter cell and its
/// addresses, shape terms (`Tail`, `Latency`), the edge traits (`Source`,
/// `Sink`, `Pull`), channel folds and the range errors.
pub mod types {
    pub use tutti_buffers::{
        BufferError, FiniteSource, Float, Fold, FoldEnd, FoldError, FoldLevel, FoldLevels,
        FoldSpec, Pull, Sample, SampleFormat, Sink, SinkFailed, Source, StereoPlanes, Stop,
    };
    pub use tutti_graph::{Curvature, Exponent, Law};
    pub use tutti_types::{
        Adsr, AmbisonicFormat, AmbisonicNormalization, AmbisonicOrder, AmbisonicOrderOutOfRange,
        AmbisonicOrdering, ArcDegrees, AtomicSampleRate, Azimuth, Balance, BarCount, BarPosition,
        BeatDuration, BeatsPerBar, BeatsPerCycle, BeatsPerCycleOutOfRange, Bed, BpmOutOfRange,
        BusTooWide, BusTopology, CentsSpan, CentsSpanOutOfRange, CompressionRatio, Confidence,
        Correlation, DepthOutOfRange, Direction, Drive, Elevation, FRAME_TOLERANCE, Feedback,
        FrameClock, Gain, InvalidBeatsPerBar, InvalidMaxBlock, InvalidNoteValue, Latency,
        LayoutError, LoopRange, LoopRangeError, MAX_CHANNELS, MeterChange, Mix, Moment,
        NotOnMidiScale, Note, NoteNumberOutOfRange, NoteValue, Pan, Param, Param64, ParamAddr,
        ParamCell, ParamKey, ParamRaw, Pass, PassEnd, PhaseIncrement, PitchClass, PlayReading,
        PlaybackRate, Radians, ReadRate, Resonance, SamplePosition, SampleRateOutOfRange,
        SampleSpan, SampleSpanOutOfRange, SegmentOrigin, Speaker, Speakers, Spread, SrcRatio,
        StereoBalance, StereoWidth, StretchFactor, Tail, TimelineSegment, TopologyError, Unit,
        UnitAddr, UnitParam, UnitParamOutOfRange, Velocity, beats_per_sample,
        first_frame_at_or_after, frames_per_beat, snap_to_whole_frame,
    };
}

/// The project's tempo (`tutti-tempo`): `TempoMap`, the points and builder it is
/// made from, and the `TempoView` a reader walks it with. What
/// `Session::set_tempo_map` takes.
pub mod tempo {
    pub use tutti_tempo::{TempoMap, TempoMapBuilder, TempoPoint, TempoView, Transition};
}

/// What a node author names (`tutti-graph`): the `Node` contract and the
/// shape, io and prepare values it takes, the wrappers that insert a node
/// (`ForkByClone`, `Unforkable`) and the param declaration (`declare_params!`,
/// `ParamNode`). The graph's compiler, editor and executor are the session's;
/// a program that builds one directly depends on `tutti-graph`.
pub mod graph {
    pub use tutti_graph::{
        BusRole, Buses, Cost, Cx, Env, ForkByClone, HasParam, HasParams, InBus, Inputs, IntoNode,
        Io, Node, OutBus, Outputs, ParamFork, ParamNode, ParamSet, ParamSetBuilder, ParamUnit,
        Prepare, Scalar, Shape, Status, Unforkable, declare_params, param_parts,
    };
}

/// The session's host plumbing (`tutti-session`): what a host that runs the
/// phases itself, a pool or a plugin editor names, rather than an application
/// that calls `Session::update`. The application's own names are at the root.
#[cfg(feature = "session")]
pub mod session {
    pub use tutti_graph::{
        BusError, BusRef, BusSource, CloseError, CommitError, MissingSource, MixError, ModeError,
        PassError, Placed, Unwired,
    };
    pub use tutti_session::{
        BlockConfig, Bus, BusSink, Channel, Committed, Connection, Crossfade, CrossfadeId,
        CrossfadeRefused, EdgeGainError, Event, EventEdge, EventTapPort, Fanin, Feeder, Graph,
        HeardOpen, HeardTapError, Inlet, InletError, LIVE_MAX_BLOCK, MainBus, MainSink, Meters,
        NotReopened, Outlet, OutputPlacement, OutputPolicy, OutputPolicyError, OutputPolicyHandle,
        PHASES, ParamError, Peer, PeerError, Phase, PhaseCx, Policy, Presence, RangeError,
        Reclaimer, RemoveError, RenderWorkerStart, Reopened, ReplaceRefused, Resource,
        ResourceFact, ResourceId, RootDeclaration, RootReader, RootTopologyStatus, SessionNode,
        Taps, TempoError, Weighted, Wire, root_width,
    };
    // The pool.
    pub use tutti_session::{
        Declared, HandleError, IntoPool, Life, Load, OpenCx, Parking, PoolConfig, PoolEntry,
        PoolError, PoolKey, PoolOutcome, PoolReport, PoolSet, PoolWait, RefreshPool, RetryError,
        WhenLoadedError, stranded_loads,
    };

    #[cfg(any(feature = "io", feature = "export"))]
    pub use tutti_session::DitherShape;
    #[cfg(feature = "export")]
    pub use tutti_session::{
        AnyStem, BOUNCE_STALL_WARNING, BounceConfigError, BounceHandle, BounceJob, BounceRendered,
        BufferMaster, BufferStem, EventLossPolicy, ExportClock, FileMaster, FileStem, Master,
        NoMaster, Normalize, Outcome, PrepareEdit, PreparedGraph, Produced, Progress, Rendered,
        Resample, Stage,
    };
    #[cfg(all(feature = "device", feature = "audio-io"))]
    pub use tutti_session::{
        Armed, InFlight, InputOpener, OpenedInput, Placement, PunchError, Rolling, Start, TakeEnd,
        TakeId, Terms, Window,
    };
    #[cfg(feature = "sampler")]
    pub use tutti_session::{AudioEntry, AudioFile, SamplerError, StreamInfo, Tier, audio_file};
    #[cfg(feature = "modulation")]
    pub use tutti_session::{
        AutomationTake, MixRef, ModInput, ModKeys, ModRefusal, ModRouteId, ModSourceId, ModState,
        ModTarget, Mods, ModsView, NoAutomation, NoSuchModRoute, NoSuchModSource, Problem, Ranged,
        RouteError, RouteInfo, RouteMut, SourceInfo, SourceName, TargetName, UNRESOLVED_WARNING,
        declare_targets,
    };
    #[cfg(feature = "device")]
    pub use tutti_session::{
        Backoff, DeviceLatency, DevicePolicy, DeviceRestart, DeviceRestarted, DeviceWatch, Follow,
        FollowEvent, LISTING_WARNING, RestartError, RestartOutcome,
    };
    #[cfg(feature = "io")]
    pub use tutti_session::{
        DitherSink, DitherSpec, Finite, IDLE_PARK, Live, PumpLoop, PumpPass, Recorder,
        RecorderError,
    };
    #[cfg(feature = "plugin")]
    pub use tutti_session::{
        EditorView, GiveUp, HangPolicy, HostPluginError, LoadedPlugin, MainPlugin, MainSession,
        ParamGesture, PluginCause, PluginEntry, PluginPorts, PortsRefusal, Recovery,
        RecoveryAction, RecoveryEvent, RecoveryFailure, RecoveryPolicy, ReloadCause,
        SNAPSHOT_INTERVAL, StateJobError, StateJobFailed, StateRestore, StateView,
    };
    #[cfg(feature = "midi")]
    pub use tutti_session::{
        FollowClock, FollowMachineControl, MIDI_INPUT_DROP_REPORT, MIDI_OUT_DROP_REPORT, MidiNodes,
        MidiRouteId, NoSuchMidiRoute, NotInstrument, SendMachineControl, SinkLoaded,
    };
    #[cfg(feature = "soundfont")]
    pub use tutti_session::{GeneralMidi, SoundFontFile, soundfont};
    #[cfg(feature = "midi-hardware")]
    pub use tutti_session::{InputLoaded, MidiInputEntry, MidiOutputEntry, OutputLoaded};
}

/// Audio file settings (`tutti-codec`): the `Spec` a file is written with, the
/// quality, bit depth and FLAC level a `Format` takes, and the decode errors.
/// The decoders, encoders and `FileIn`/`FileOut` are `tutti-codec`'s. Feature
/// `codec`.
#[cfg(feature = "codec")]
pub mod codec {
    pub use tutti_codec::{
        DecodeError, FlacLevel, IntDepth, Quality, QualityOutOfRange, Spec, SpecError, can_decode,
        decodable_extensions,
    };
}

/// Sample playback (`tutti-sampler`): voices and their loops, the placement
/// and send errors. The `Clip` an application loads is at the root. Feature
/// `sampler`.
#[cfg(feature = "sampler")]
pub mod sampler {
    pub use tutti_sampler::{
        Loop, LoopError, PlaceError, Playback, SendError, StreamerGone, StreamingAddError, Voice,
        VoiceWindow,
    };
}

/// Modulation (`tutti-modulation`): the sources and curves an application
/// declares. `Lfo` and `Follower` are at the root. Feature `modulation`.
#[cfg(feature = "modulation")]
pub mod modulation {
    pub use tutti_modulation::{
        Amount, CompileError, ControlSource, Corners, Curve, CurveSegment, DepthBy, LevelFollower,
        LfoRate, Macro, NoteAdsr, NoteEnvelope, Polarity, ProgramError, SignalKind,
    };
}

/// The polyphonic subtractive synth node (`tutti-polysynth`): the node, its
/// config and the voice, unison and portamento settings. Feature `synth`.
#[cfg(feature = "synth")]
pub mod polysynth {
    pub use tutti_polysynth::{
        AllocationStrategy, FilterModConfig, FilterType, GlideTime, GlideTimeError, OscillatorType,
        PolySynth, PortamentoConfig, PortamentoMode, SvfMode, SynthConfig, Tuning, UnisonConfig,
        UnisonVoices, UnisonVoicesError, VoiceMode,
    };
}

/// SoundFont (.sf2) playback (`tutti-soundfont`): the unit and its settings.
/// Feature `soundfont`.
#[cfg(feature = "soundfont")]
pub mod soundfont {
    pub use tutti_soundfont::{Polyphony, PolyphonyError, Settings, SoundFontUnit};
}

/// Plugin hosting (`tutti-plugin`): the main-thread proof, the bridge and
/// automation settings, the plugin catalog, and the faults a load or an editor
/// reports. Feature `plugin` plus a format feature.
///
/// The sandbox's server, `tutti-plugin-server`, is a separate binary the host
/// spawns; build it with `cargo build -p tutti-plugin-server`.
#[cfg(feature = "plugin")]
pub mod plugin {
    pub use tutti_plugin::{
        AutomationMode, BridgeConfig, EditorError, FeatureReport, Features, LoadStage,
        MainThreadMarker, MainWork, MainWorkHandle, PluginRenderFault, Preset, PresetSupport,
        TerminationReason,
    };

    /// The plugin catalog: scanning, the database and the descriptors it
    /// holds.
    pub mod catalog {
        pub use tutti_plugin::catalog::{
            AudioConfig, CatalogConfig, NO_SCAN_DIRS, PluginDb, PluginDescriptor, PluginId,
            PluginRole, Scan, ScanBusy, ScanPoll, ScanResult,
        };
    }
}

/// The names of the common MIDI path in one place:
/// a file (`MidiFile`, `Track`, `Tick`), a message (`Message`, `Ump`,
/// `MidiChannel`, `MidiGroup`, `NoteNumber`, `NoteVelocity`, `Voice1`, `SysexAssembler`, `Midi1Parser`), a timed event, the keys
/// that play an instrument or a live input (`MidiKeys`),
/// the session's route rule, events and errors (`MidiRouteRule`, `MidiReport`,
/// `midi::Error`), a host's own output (`midi_sink`, `MidiSink`), and under `midi-hardware` a device (`DeviceId`,
/// `DeviceInfo`), the ports the pool opens for them (`midi_input`, `midi_output`,
/// `MidiInput`, `MidiOutput`) and what an open one said of itself (`Learned`, `SavedDevice`, `Match`), and the Profiles
/// its MIDI-CI peers support and how to switch one (`PeerProfiles`,
/// `ListedProfile`, `ProfileSwitch`, `ProfilesChange`). The translation layer
/// (`Translator`, `Narrower`), the runtime's clock sync and the MIDI-CI codec
/// belong to `tutti-midi-protocol`, `tutti-midi-runtime` and `tutti-midi-device`.
/// Feature `midi`.
#[cfg(feature = "midi")]
pub mod midi {
    pub use tutti_midi_file::{FileEvent, Meta, MidiFile, SmfFormat, Tick, Track};
    pub use tutti_midi_runtime::{MidiKeys, TimedMidiEvent};
    pub use tutti_ump::{
        Address, CCNumber, DeviceIdentity, ManufacturerId, Message, Midi1Parser, MidiChannel,
        MidiGroup, NoteNumber, NoteVelocity, Protocol, StreamConfig, SysexAssembler,
        TicksPerQuarterNote, U7, U14, Ump, Voice1,
    };

    /// The session's one MIDI error (`tutti_session::MidiError`): each cause
    /// (hardware, file, send, route) is its source.
    #[cfg(feature = "session")]
    pub use tutti_session::MidiError as Error;
    #[cfg(feature = "session")]
    pub use tutti_session::RouteDeclarer;
    #[cfg(feature = "session")]
    pub use tutti_session::{
        AddedInstrument, AnyNode, FollowTempo, InstrumentHandle, Midi, MidiLoad, MidiReport,
        MidiRouteRule, MidiSink, MidiSinkEntry, MidiSinkInfo, SinkOptions, Stamping, TakePlacement,
        midi_sink,
    };

    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_device::{DeviceId, DeviceInfo, Learned, Match, SavedDevice};
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_hardware::{
        ChannelCount, ChannelProfile, CiChange, CiPeerInfo, DeviceQuery, ListedProfile,
        MidiHardwareOptions, PeerId, PeerProfiles, ProfileError, ProfileSwitch, ProfilesChange,
        Whole, WholeProfile,
    };
    #[cfg(all(feature = "session", feature = "midi-hardware"))]
    pub use tutti_session::{
        MidiInput, MidiInputInfo, MidiOutput, MidiOutputInfo, MidiPortError, OutputOptions,
        PortCause, midi_input, midi_output,
    };
}
