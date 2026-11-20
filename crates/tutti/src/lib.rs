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
//!     .add(LfoNode::new(LfoShape::Sine).with_rate(Hz::new(440.0)));
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
//! | `tutti::measure` | the analyses `Session::analyze` runs: loudness, peaks, silence, onsets, spectrum, chroma, the vectorscope | `analyze` |
//! | `tutti::modulation` | modulation sources, curves and their errors | always |
//! | `tutti::polysynth` | the polyphonic synth node and its configs | `synth` |
//! | `tutti::soundfont` | the SoundFont playback node and its settings | `soundfont` |
//! | `tutti::plugin` | hosting settings, the main-thread proof and the plugin catalog | `plugin` + a format |
//!
//! `cargo xtask check-umbrella` holds the rule: no crate is re-exported whole,
//! and no item is exported twice.
//! Every type in the signature of an exported item is nameable here, bar the
//! internals of the engine, the graph compiler and the plugin host that a few
//! items still name; `cargo xtask umbrella-api` lists them for their owners.
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
//! - `decode-wav`, `decode-flac`, `decode-mp3`, `decode-ogg` — audio file decoders (each implies `codec` and
//!   turns on the same decoder in `tutti-codec`; a sampler decodes through it).
//! - `encode-wav`, `encode-flac`, `encode-aiff`, `encode-vorbis` — the encoders
//!   a `Format` names (each implies `codec`; none implies `export`, and
//!   `encode-vorbis` is the one path to a C library).
//! - `codec` — `tutti::codec`, `Wave`. Implied by every decode and encode feature.
//! - `record` — the session's `Recorder` and its pump loop, device-free. Implied by `capture`.
//! - `capture` — live audio input from the sound card: `Session::add_input`, takes, mic capture
//!   (implies `device` and `record`).
//! - `export` — `Bounce` and its results (implies `encode-wav`).
//! - `midi` — `tutti::midi`. `crates/tutti/examples/midi_all.rs` uses every piece together, offline.
//! - `midi-hardware` — OS MIDI I/O in `tutti::midi` (implies `midi`).
//! - `synth`, `soundfont`, `sampler` — the matching module.
//!   `synth` and `soundfont` imply `midi`; `sampler` implies no decoder.
//! - `analyze` — `Session::analyze` and `tutti::measure` (implies `session`).
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
    InletNode, InletRing, Level, Losses, MeterReader, NodeMode, Stamp, TapAlign, TapEvent, TapId,
    TapOptions, TapReader, TapWriter,
};
pub use tutti_rt::PushError;
// The engine's health: a `Gauge<Load>` and a `Counter<BlockCounts>`.
#[cfg(feature = "session")]
pub use tutti_engine::{BlockCounts, Load};
pub use tutti_timeline::{
    AfterLocate, FadeOut, LoopRegion, MotionCommand, MotionState, TransportCommand,
};

// Units, channels, buffers and the musical meter.
pub use tutti_buffers::{ChannelLayout, ChannelTopology, Interleaved, InterleavedMut, MaxBlock};
pub use tutti_music::{Meter, MeterMap, TimeSignature};
pub use tutti_units::{
    Amplitude, At, BarNumber, Beat, Bipolar, Bpm, Cents, Db, Depth, Frame, Hz, Normalized, ParamId,
    Q, SampleRate, Samples, Seconds, Semitones, TextError,
};

// The device handle and a device's identity.
#[cfg(feature = "device")]
pub use tutti_cpal::{AudioHost, DeviceId};

// The sampler's clip.
#[cfg(feature = "sampler")]
pub use tutti_sampler::Clip;

// A decoded wave, and what a bounce writes.
#[cfg(feature = "codec")]
pub use tutti_buffers::Wave;
#[cfg(feature = "codec")]
pub use tutti_codec::{Format, Pcm};

// The session, its wiring and its requests.
#[cfg(feature = "session")]
pub use tutti_graph::NodeKey;
#[cfg(feature = "plugin")]
pub use tutti_plugin_db::{Catalog, Hosting};
#[cfg(feature = "plugin")]
pub use tutti_plugin_types::{PluginFailure, PluginRef, PresetError, PresetId, StateUse};
#[cfg(all(feature = "session", feature = "midi-hardware"))]
pub use tutti_session::MidiPolicyTarget;
#[cfg(all(feature = "session", feature = "plugin"))]
pub use tutti_session::PluginParams;
#[cfg(all(feature = "session", feature = "analyze", feature = "sampler"))]
pub use tutti_session::windows_from_signal;
#[cfg(all(feature = "session", feature = "plugin"))]
pub use tutti_session::{
    AddPluginError, AnyPlugin, Diagnostics, ListedPreset, LoadPreset, Opened, Plugin, PluginBuses,
    PluginChange, PluginDeclaration, PluginEvent, PluginHealth, PluginInfo, PluginPresets,
    PooledPlugin, PortCounts, PresetJobError, PresetJobFailed, RestoreError, RestoreState,
    SaveState,
};
#[cfg(all(feature = "session", feature = "analyze"))]
pub use tutti_session::{
    Analyze, AnalyzeError, Analyzer, AnalyzerError, Audio, AudioContext, AudioInput, BindError,
};
#[cfg(feature = "session")]
pub use tutti_session::{
    AnyAssign, AnyTarget, Assign, AutomationTarget, ClickTargets, ClickVolumeTarget, CurveTarget,
    DepthTarget, Effective, GainTarget, GainTargetError, Info, IntoAssign, IntoRead, LoopTarget,
    MacroTarget, MeterTarget, MixTarget, ModeTarget, Modify, Modulatable, OutputPolicyTarget,
    ParamTarget, PolarityTarget, RangeTarget, RateTarget, ScalarTarget, SetRefusal, SettingTarget,
    Target, TargetInfo, TempoTarget, ValueError, ValueErrorKind, Values,
};
#[cfg(feature = "session")]
pub use tutti_session::{
    AppKey, BlockDates, Clock, ConnectError, CrossfadeControls, Edge, EventInPort, EventOutPort,
    InPort, Job, JobId, JobKind, JobOutput, JobStatus, Key, NoSuchNode, Offline, OfflineSession,
    OutPort, Playhead, RealTime, Root, Session, SessionBuilder, SessionEvent, SessionFloat,
    SystemClock, SystemSession, TransportHandle, WaitError, Workers,
};
#[cfg(all(feature = "session", feature = "export"))]
pub use tutti_session::{
    Bounce, BounceError, BounceLatency, BounceTail, Bounced, Range, Stem, StemGain, Written,
};
#[cfg(all(feature = "session", feature = "midi"))]
pub use tutti_session::{ClipEventsTarget, HarmonyTarget, MpeModeTarget, OutputLatencyTarget};
#[cfg(all(feature = "session", feature = "device"))]
pub use tutti_session::{Device, DeviceClock, DeviceEvent, LiveSession};
#[cfg(all(feature = "session", feature = "sampler"))]
pub use tutti_session::{DirectionTarget, LoopingTarget, SpeedTarget, WindowTarget};
#[cfg(all(feature = "session", feature = "capture"))]
pub use tutti_session::{
    Input, InputConfig, Punch, Recorded, Take, TakeError, TakeEvent, TakeTarget,
};
#[cfg(all(feature = "session", feature = "midi", feature = "analyze"))]
pub use tutti_session::{
    LevelVelocity, NoteCents, NoteMapping, clip_from_notes, harmony_from_chords,
};
#[cfg(all(feature = "session", feature = "plugin"))]
pub use tutti_session::{OtherPeer, PeerState, SavedPeer};
#[cfg(feature = "session")]
pub use tutti_session::{ParamRef, RouteSpec};
#[cfg(all(feature = "session", feature = "plugin"))]
pub use tutti_session::{
    PluginAutomationModeTarget, PluginParamRefusal, PluginParamTarget, PluginUnit, UnitError,
};
#[cfg(all(feature = "session", feature = "sampler"))]
pub use tutti_session::{SamplerEvent, SamplerLoad};

// --- the modules ------------------------------------------------------------

/// The node catalogue (`tutti-nodes`): oscillators, LFOs, filters, delays,
/// dynamics, reverbs, modulation effects, mixing, panners, convolution and
/// automation lanes, each with its config, controls and enums. A node is
/// inserted with `Session::graph().add(node)`.
///
/// A linear-phase equalizer from bands, reachable from here alone:
///
/// ```
/// use tutti::nodes::{ConvolverNode, FirConvolverConfig, LinearPhaseDesign, MatchedBell, Tilt};
/// use tutti::types::BusTopology;
/// use tutti::{Db, Hz, Q, SampleRate};
///
/// let rate = SampleRate::SR_48K;
/// let bands = [
///     MatchedBell::new(Q::new(1.0), Db::new(6.0)).design(Hz::new(1_000.0), rate).unwrap(),
///     Tilt::new(Db::new(-3.0)).design(Hz::new(4_000.0), rate).unwrap(),
/// ];
/// let fir = LinearPhaseDesign::new(Hz::new(100.0)).design(rate, &bands).unwrap();
/// let node = ConvolverNode::<f32>::try_from(FirConvolverConfig::new(BusTopology::STEREO, &fir))
///     .unwrap();
/// assert_eq!(node.latency_samples().get(), 512 + (fir.len().get() - 1) / 2);
/// ```
pub mod nodes {
    pub use tutti_dsp::{
        Absorption, AllPass, AllPassForm, Band, BrickwallControls, ChunkSize, DecayTime, Fdn, Fir,
        FirSummary, Full, Gains, HrirMeasurement, HrirSphere, HrtfBinauralError, Inline, Lerp,
        Line, LineModulation, LookaheadError, Matrix, MatrixConvolver, MatrixConvolverConfig,
        MatrixConvolverError, MaxDecayError, Network, NetworkOutputMap, ResampleError, SimdLanes,
        SpeakerGeometryError, Stage, Tap, Taps, WaveshaperControls,
    };
    pub use tutti_dsp::{
        Biquad, Butterworth, ButterworthOrder, ButterworthOrderError, CutResponse, FirError,
        LinearPhaseDesign, LinkwitzRiley, Lti, MAX_GRID, MatchedBell, Sos, SosError, System, Tilt,
    };
    pub use tutti_dsp::{Lookahead, MaxDecay, MaxDelay, MaxDelayError, PeakDetection};
    pub use tutti_modulation::{
        ControlProgram, InputLayer, Layer, LfoShape, SignalSource, SourceId, Waveform,
    };
    pub use tutti_nodes::automation::Recorder as AutomationRecorder;
    pub use tutti_nodes::automation::{
        AutomationLaneNode, Breakpoint, ControlLaneNode, LaneControls, LiveAutomationLane,
        RecordMode, RecordingConfig, Trajectory,
    };
    pub use tutti_nodes::{
        Antialiasing, AutoPanConfig, AutoPanControls, AutoPanKernel, AutoPanNode, AutoPanSet,
        ChannelOutOfRange, MidSideConfig, MidSideNode, OscillatorWave, PanLaw, PolarityConfig,
        PolarityError, PolarityNode, RingModConfig, RingModControls, RingModKernel, RingModNode,
        RingModSet, StereoWidthConfig, StereoWidthKernel, StereoWidthNode, StereoWidthSet,
        TremoloConfig, TremoloControls, TremoloKernel, TremoloNode, TremoloSet, WidthControls,
    };
    pub use tutti_nodes::{
        BAND_PARAMS, BellFilterConfig, BellFilterControls, BellFilterNode, BitCrusherConfig,
        BitCrusherControls, BitCrusherNode, BrickwallLimiterConfig, BrickwallLimiterNode,
        BusStripConfig, BusStripNode, ChannelTimes, ChannelTimesError, ClickConfig, ClickLevel,
        ClickLevelError, ClickSettings, ClickState, CompressorConfig, CompressorControls,
        CompressorNode, ConvolverConfig, ConvolverError, ConvolverNode, CrossoverConfig,
        CrossoverControls, CrossoverError, CrossoverNode, CutAlignment, CutFilterConfig,
        CutFilterControls, CutFilterNode, DcBlockerConfig, DcBlockerControls, DcBlockerNode,
        DelayConfig, DelayControls, DelayLayout, DelayLayoutBuilder, DelayLayoutError, DelayNode,
        DistortionConfig, DistortionNode, DynamicEqConfig, DynamicEqControls, DynamicEqNode,
        DynamicMode, EnvelopeFilterConfig, EnvelopeFilterControls, EnvelopeFilterNode,
        EnvelopeFollowerNode, EqBandConfig, EqBandNode, FdnConfig, FdnControls, FdnLayout, FdnNode,
        FeedbackRouting, FeedbackRoutingBuilder, FeedbackRoutingError, FirConvolverConfig,
        FreeverbConfig, FreeverbControls, FreeverbNode, FreezeMode, GateConfig, GateControls,
        GateNode, HrtfBinauralConfig, HrtfBinauralNode, InterpolationMode, IrLayout, KeyFilter,
        KeySource, LadderControls, LadderFilterConfig, LadderFilterNode, LadderType,
        LevellerConfig, LevellerControls, LevellerNode, LfoClock, LfoNode, LimiterConfig,
        LimiterControls, LimiterNode, MetronomeMode, ModDelayConfig, ModDelayControls,
        ModDelayLayout, ModDelayLayoutBuilder, ModDelayNode, MultibandBands,
        MultibandCompressorConfig, MultibandCompressorNode, MultibandControls, NoteEnvelopeNode,
        OscillatorNode, OversamplingFactor, PhaserConfig, PhaserControls, PhaserNode,
        PlateControls, PlateReverbConfig, PlateReverbNode, SOURCE_EVENT_CAPACITY, ShapeKind,
        SourceNode, SourcePorts, Stages, StagesError, Stagger, StaggerError, SvfControls,
        SvfFilterConfig, SvfFilterNode, SvfType, Sweep, SweepBuilder, SweepError, SweepRange,
        SweepRangeError, TiltFilterConfig, TiltFilterControls, TiltFilterNode, TransientControls,
        TransientShaperConfig, TransientShaperNode, VbapError, VbapPannerConfig, VbapPannerNode,
    };
    pub use tutti_nodes::{
        BellFilterKernel, BitCrusherKernel, BrickwallKernel, CompressorKernel, Control,
        ControlRead, ControlSet, ControlSignals, ControlValid, CrossoverKernel, CutFilterKernel,
        DcBlockerKernel, DelayKernel, DistortionKernel, DynamicEqKernel, EnvelopeFilterKernel,
        EqControls, EqKernel, FdnKernel, Feed, FreeverbKernel, GateKernel, Kernel, KernelNode,
        LadderKernel, LevellerKernel, LimiterKernel, ModDelayKernel, PhaserKernel, ReconcileError,
        SvfKernel, TiltFilterKernel,
    };
    pub use tutti_nodes::{
        BellFilterSet, BitCrusherSet, BrickwallSet, CompressorSet, CrossoverSet, CutFilterSet,
        DcBlockerSet, DelaySet, DistortionSet, DynamicEqSet, EnvelopeFilterSet, FdnSet,
        FreeverbSet, GateSet, HasControl, LadderSet, LevellerSet, LimiterSet, ModDelaySet,
        PhaserSet, PlateKernel, PlateSet, SvfSet, TiltFilterSet, TransientShaperKernel,
        TransientShaperSet,
    };
}

/// The shared vocabulary (units, music types, buffers and the param cells: `tutti-units`,
/// `tutti-music`, `tutti-buffers`, `tutti-rt`) a program names less
/// often than the root's units: moments and notes, the parameter cell and its
/// addresses, shape terms (`Tail`, `Latency`), the edge traits (`Source`,
/// `Sink`, `Pull`), channel folds and the range errors.
pub mod types {
    pub use tutti_buffers::{
        AmbisonicFormat, AmbisonicNormalization, AmbisonicOrder, AmbisonicOrderOutOfRange,
        AmbisonicOrdering, Bed, BusTooWide, BusTopology, ChannelIndex, ChannelIndexOutOfRange,
        InvalidMaxBlock, LayoutError, MAX_CHANNELS, Placement, Speaker, Speakers, TopologyError,
    };
    pub use tutti_buffers::{
        BlockMut, BlockSide, ByWidth, Frames, FramesMut, InputOutput, Mono, MonoMut, PlaneIter,
        PlaneIterMut, Planes, PlanesMut,
    };
    pub use tutti_buffers::{
        BufferError, FiniteSource, Float, FloatWidth, Fold, FoldEnd, FoldError, FoldLevel,
        FoldLevels, FoldSpec, NotAFloat, OverFloat, Pull, Sample, SampleFormat, Sink, SinkFailed,
        Source, StereoPlanes, Stop,
    };
    pub use tutti_curve::{Curvature, CurvatureError, Exponent, ExponentError, Law, Piece};
    pub use tutti_music::{
        Adsr, BarPosition, BeatsPerBar, FRAME_TOLERANCE, FrameClock, InvalidBeatsPerBar,
        InvalidNoteValue, LoopRange, LoopRangeError, MeterChange, NotOnMidiScale, Note,
        NoteNumberOutOfRange, NoteValue, Pass, PassEnd, PitchClass, SegmentOrigin, TimelineSegment,
        beats_per_sample, first_frame_at_or_after, frames_per_beat, snap_to_whole_frame,
    };
    pub use tutti_platform::{
        Asset, Destination, Executor, Identity, Open, Panicked, PriorityRequest, ReadSeek,
        Shutdown, Sleep, Staged, StagedCommitError, StagedFile, StagedKeep, Stamp, Threads, Timer,
    };
    pub use tutti_rt::{
        AtomicCell, AtomicWord, Bell, Bells, Count, Counter, CounterWriter, Drained, Gauge,
        GaugeWriter, LevelSpan, Listener, PosFrame, Reading, Reclaim, RingFloat, RingSample, Tally,
        TallyWriter, Words,
    };
    pub use tutti_rt::{AtomicSampleRate, Mint, Param, Param64, ParamCell};
    pub use tutti_units::{
        ArcDegrees, Azimuth, Balance, Ballistics, BarCount, BeatDuration, BeatsPerCycle,
        BeatsPerCycleOutOfRange, BipolarOutOfRange, BitDepth, BpmOutOfRange, CentsSpan,
        CentsSpanOutOfRange, CompressionRatio, Confidence, Correlation, DepthOutOfRange, Direction,
        Drive, Elevation, Feedback, Gain, HoldFactor, Latency, Mix, Moment, NormalizedOutOfRange,
        Pan, ParamAddr, ParamKey, PhaseIncrement, PhaseStep, PlayReading, PlaybackRate,
        PlaybackRateOutOfRange, Radians, ReadPosition, ReadRate, Resonance, RunningPhase,
        SamplePosition, SampleRateOutOfRange, SampleSpan, SampleSpanOutOfRange, Spread, SrcRatio,
        StereoBalance, StereoWidth, StretchFactor, Tail, Unit, UnitAddr, UnitParam,
        UnitParamOutOfRange, Velocity,
    };
    pub use tutti_units::{Phase, Quantity, Real, Resyncs};
}

/// The project's tempo (`tutti-tempo`): `TempoMap`, the points and builder it is
/// made from, and the `TempoView` a reader walks it with. What
/// the tempo target takes.
pub mod tempo {
    pub use tutti_tempo::{Cell, CellGrid, CellGridError, FromBeatsError, TempoError};
    pub use tutti_tempo::{TempoMap, TempoMapBuilder, TempoPoint, TempoView, Transition};
    pub use tutti_timeline::{ProjectMeter, TempoReader};
}

/// What a node author names (`tutti-graph`): the `Node` contract and the
/// shape, io and prepare values it takes, the wrappers that insert a node
/// (`ForkByClone`, `Unforkable`) and the param declaration (`declare_params!`,
/// `ParamNode`). The graph's compiler, editor and executor are the session's;
/// a program that builds one directly depends on `tutti-graph`.
pub mod graph {
    pub use tutti_graph::{
        Bus, BusRole, Buses, Cost, Cx, Env, ForkByClone, HasParam, HasParams, InBus, Inputs,
        IntoNode, Io, Node, OutBus, Outputs, ParamFork, ParamNode, ParamSet, ParamSetBuilder,
        ParamUnit, Params, Prepare, RtLevel, RtRecord, RtTarget, RtValue, Scalar, Shape, Status,
        Unforkable, declare_params, param_parts,
    };
    pub use tutti_graph::{
        BusIn, BusOut, Channel, Chase, CompileError, ConstantMask, Continuity, CycleEdge, Due,
        Edge, Event, EventKind, EventOrderError, EventRejected, EventWriter, ForkBuild, ForkCause,
        ForkFaultKind, ForkHealth, ForkMode, ForkSource, Forked, GraphInvalid, Harmony,
        HarmonyKind, InPlaceMask, NodeParts, Offset, ParamEvent, ParamInput, ParamKind, ParamMod,
        ParamPorts, ParamPortsError, ParamRamp, ParamRange, ParamRangeCell, ParamRangeReader,
        ParamSegment, Pass, PlayClock, RenderMode, Resolution, SilenceMask, Sink, SortedEvents,
        Source, SubBlocks, TransportChange, TransportChangeRejected, TransportChanges,
        TransportHistory, TransportPosition, UmpAt, VerifyError, Wake, WiredMask,
    };
    pub use tutti_graph::{EdgeGain, FeedbackFrom, Invalid, NodeMix, RangeView};
}

/// The session's host plumbing (`tutti-session`): what a host that runs the
/// phases itself, a pool or a plugin editor names, rather than an application
/// that calls `Session::update`. The application's own names are at the root.
#[cfg(feature = "session")]
pub mod session {
    #[cfg(feature = "device")]
    pub use tutti_cpal::DeviceError;
    #[cfg(feature = "capture")]
    pub use tutti_cpal::MicConfig;
    #[cfg(feature = "device")]
    pub use tutti_cpal::{
        DeviceChange, DeviceDirection, DeviceHost, DeviceListing, DeviceRoster, DeviceSelector,
        Listed, OutputConfig, Snapshot, SnapshotSource, SupportedConfig,
    };
    pub use tutti_engine::{
        BackendErrorKind, GraphEngineError, LOAD_MEAN_TIME_CONSTANT, Lagging, OutputLevelError,
        OutputLevels, OutputMap, OutputMapError, OutputSpec, OutputStrip, Routing, RoutingError,
        StartError, StreamFault, StreamFaultKind, StreamFaults, StreamLatency, TimerError,
    };
    #[cfg(feature = "export")]
    pub use tutti_export::{
        ConfigError, JobError, RenderError, Rendered, RenderedError, TargetError, Unmeasured,
    };
    pub use tutti_graph::{
        BusError, BusRef, BusSource, CloseError, CommitError, Inserted, MissingSource, MixError,
        ModeError, PassError, Rewired, Unwired, Wired,
    };
    pub use tutti_graph::{
        BusFeed, BusFold, BusSide, EdgeFade, EdgeId, EventCapacityError, EventIn, EventOut,
        ForkError, InletCounts, Instrument, MeterId, ParamIn, PrerollTable, RootError, RootId,
    };
    pub use tutti_modulation::PeriodPoint;
    #[cfg(feature = "device")]
    pub use tutti_session::ClockError;
    pub use tutti_session::{
        BlockConfig, Bus, BusSink, Channel, Committed, Connection, Crossfade, CrossfadeId,
        CrossfadeRefused, DeclarationError, Event, EventEdge, EventTapPort, Fanin, Feeder, Graph,
        Inlet, InletError, LIVE_MAX_BLOCK, MainBus, MainSink, Meters, NodeSlots, NotReopened,
        Outlet, OutputPlacement, OutputPolicy, OutputPolicyError, OutputTapOpen, PHASES, Peer,
        PeerError, Phase as UpdatePhase, PhaseCx, Policy, Presence, Reclaimer, Reclaims,
        RemoveError, Reopened, ReplaceRefused, Resource, ResourceFact, ResourceId, RootDeclaration,
        RootReader, RootTopologyStatus, SessionNode, SessionOutputTapError, Taps, TempoError,
        Weighted, Wire, root_width,
    };
    pub use tutti_timeline::{
        AudioTaken, LoopEdit, OfflineTimeline, OfflineTimelineConfig, OfflineTransport, TimedError,
        Timeline,
    };
    // The pool.
    pub use tutti_session::PoolLoad;
    pub use tutti_session::WorkerError;
    pub use tutti_session::{
        Declared, HandleError, IntoPool, Life, LoadError, OpenCx, Owned, PoolConfig, PoolEntry,
        PoolError, PoolEvent, PoolJobError, PoolKey, PoolLoaded, PoolOutcome, PoolReport, PoolSet,
        PoolState, RefreshPool, RefreshReport, Retention, RetryError, Sharing, WhenLoadedError,
        stranded_loads,
    };

    #[cfg(feature = "export")]
    pub use tutti_dsp::FftFloat;
    #[cfg(any(feature = "record", feature = "export"))]
    pub use tutti_session::DitherShape;
    #[cfg(feature = "sampler")]
    pub use tutti_session::SamplerError;
    #[cfg(feature = "export")]
    pub use tutti_session::{
        AnyStem, BounceConfigError, BounceHandle, BounceJob, BounceRendered, BufferMaster,
        BufferStem, EventLossPolicy, ExportClock, FileMaster, FileStem, Master, NoMaster,
        Normalize, Outcome, PrepareEdit, PreparedGraph, Produced, Progress, Resample, Stage,
    };
    #[cfg(feature = "capture")]
    pub use tutti_session::{
        Armed, InFlight, InputOpener, OpenedInput, PunchError, Rolling, TakeEnd, TakeId,
        TakePlacement, Window,
    };
    #[cfg(any(feature = "capture", feature = "midi"))]
    pub use tutti_session::{Start, TakeLatency, Trim};
    // The `audio_file` entry decodes a file, so it needs a decoder.
    #[cfg(feature = "midi")]
    pub use tutti_session::MidiNodes;
    #[cfg(all(
        feature = "sampler",
        any(
            feature = "decode-wav",
            feature = "decode-flac",
            feature = "decode-mp3",
            feature = "decode-ogg"
        )
    ))]
    pub use tutti_session::{AudioEntry, AudioFile, StreamInfo, Tier, audio_file};
    pub use tutti_session::{
        AutomationTake, MixRef, ModInput, ModKeys, ModRefusal, ModRouteId, ModSourceId, ModState,
        ModTarget, Mods, ModsView, NoAutomation, NoSuchModRoute, NoSuchModSource, Problem, Ranged,
        RouteError, RouteInfo, SourceInfo, SourceName, TargetName, declare_targets,
    };
    #[cfg(feature = "device")]
    pub use tutti_session::{
        Backoff, DeviceLatency, DevicePolicy, DeviceRestart, DeviceRestarted, DeviceWatch, Follow,
        FollowEvent, RestartError, RestartOutcome,
    };
    #[cfg(feature = "record")]
    pub use tutti_session::{
        DitherSink, FinalizeError, Finite, IDLE_PARK, Live, NoiseShaping, PumpLoop, PumpPass,
        Recorder, RecorderError,
    };
    #[cfg(feature = "plugin")]
    pub use tutti_session::{
        Editor, EditorCallError, GiveUp, HangPolicy, LoadedPlugin, ParamGesture, PluginCause,
        PluginParam, PluginPorts, PoolPluginEntry, PortsRefusal, Recovery, RecoveryAction,
        RecoveryEvent, RecoveryFailure, RecoveryPolicy, ReloadCause, StateJobError, StateJobFailed,
        StateRestore, StateView,
    };
    #[cfg(feature = "soundfont")]
    pub use tutti_session::{GeneralMidi, SoundFontFile, soundfont};
}

/// Audio file settings (`tutti-codec`): the `Spec` a file is written with, the
/// quality, bit depth and FLAC level a `Format` takes, and the decode errors.
/// The decoders, encoders and `FileIn`/`FileOut` are `tutti-codec`'s. Feature
/// `codec`.
#[cfg(feature = "codec")]
pub mod codec {
    #[cfg(any(
        feature = "encode-wav",
        feature = "encode-flac",
        feature = "encode-aiff",
        feature = "encode-vorbis"
    ))]
    pub use tutti_codec::write_wave;
    pub use tutti_codec::{Codec, Error, Kept, Unfinished};
    pub use tutti_codec::{
        DecodeError, FlacLevel, IntDepth, Quality, QualityOutOfRange, Spec, SpecError, can_decode,
        decodable_extensions,
    };
    #[cfg(any(
        feature = "decode-wav",
        feature = "decode-flac",
        feature = "decode-mp3",
        feature = "decode-ogg"
    ))]
    pub use tutti_codec::{WaveMetadata, read_wave, read_wave_metadata};
}

/// Sample playback (`tutti-sampler`): voices and their loops, the placement
/// and send errors. The `Clip` an application loads is at the root. Feature
/// `sampler`.
#[cfg(feature = "sampler")]
pub mod sampler {
    pub use tutti_sampler::{
        Block, Direction, Error, GUARD_FRAMES, OpenError, RefusedStreamStart, SamplerFloat, Stream,
        StreamFault, StreamFile, StreamId, StreamPhase, StreamReader, StreamStart, StreamStatus,
        VoiceNodeHandle,
    };
    // A file's probe needs a decoder.
    pub use tutti_sampler::{
        ControlCell, Loop, LoopError, PlaceError, PlayError, Playback, SendError,
        StreamingAddError, VoiceWindow,
    };
    #[cfg(any(
        feature = "decode-wav",
        feature = "decode-flac",
        feature = "decode-mp3",
        feature = "decode-ogg"
    ))]
    pub use tutti_sampler::{ProbeError, SampleFacts};
}

/// Audio measurement (`tutti-measure`): the analyses `Session::analyze` runs.
/// Each is a config (`PeakConfig`, `OnsetConfig`, `LoudnessConfig`, ...) that is
/// a `Measure`, whose meter folds chunks into a result: loudness, peaks,
/// silence, onsets, spectrum, chroma and the vectorscope. Feature `analyze`.
#[cfg(feature = "analyze")]
pub mod measure {
    pub use tutti_measure::{Error, Measure, Meter};
    // Silence and onsets.
    pub use tutti_measure::{
        DetectionFunction, FiniteLevel, FiniteLevelError, Framing, Novelty, Onset, OnsetConfig,
        OnsetGap, OnsetGapError, OnsetMeter, OnsetSensitivity, OnsetSensitivityError,
        OnsetThreshold, OnsetThresholdError, Region, SilenceConfig, SilenceMeter,
    };
    // Spectrum and chroma.
    pub use tutti_measure::{
        BandCount, BandCountError, Banding, BandsPerOctave, BandsPerOctaveError, Chroma,
        ChromaConfig, ChromaFrame, ChromaMeter, ChromaSummary, FrequencyBand, HarmonicDecay,
        HarmonicDecayError, Harmonics, HarmonicsError, PeakDecay, PeakDecayError, PeakFloor,
        PeakFloorError, Smoothing, SpectrumConfig, SpectrumFloor, SpectrumFloorError,
        SpectrumMeter, SpectrumTime, SpectrumTimeError, Tuning, TuningOffset, TuningOffsetError,
    };
    // Loudness, waveform blocks and the vectorscope.
    pub use tutti_measure::{
        ChannelBlock, Loudness, LoudnessConfig, LoudnessMeter, PeakBlock, PeakBlocks, PeakConfig,
        PeakMeter, ScopePoint, TruePeakConfig, TruePeakMeter, VectorscopeConfig, VectorscopeMeter,
        WindowedLoudnessConfig, WindowedMeter, WindowedReading,
    };
    // The dsp types `Error`, the spectrum and `Chroma` name.
    pub use tutti_dsp::{
        BinCount, BinIndex, ChannelWeight, ChannelWeightError, ChannelWeights, CosineWindow,
        FrameCount, FrameIndex, Grid, StftError,
    };
}

/// Modulation (`tutti-modulation`): the sources and curves an application
/// declares, and the `Lfo` and `Follower` that read them.
pub mod modulation {
    pub use tutti_modulation::{
        Amount, CompileError, ControlSource, Corners, Curve, CurveSegment, DepthBy, Follower,
        LevelFollower, Lfo, LfoRate, Macro, NoteAdsr, NoteEnvelope, Polarity, ProgramError,
        SignalKind, TargetDecl,
    };
    pub use tutti_modulation::{
        Clock, DepthSnapshot, LayerId, Leg, PlayGrid, ProgramTime, RateAnchor, RouteKey, SourceKey,
        TargetKey,
    };
}

/// The polyphonic subtractive synth node (`tutti-polysynth`): the node, its
/// config and the voice, unison and portamento settings. Feature `synth`.
#[cfg(feature = "synth")]
pub mod polysynth {
    pub use tutti_polysynth::{
        AllocationStrategy, FilterModConfig, FilterType, GlideTime, GlideTimeError, Lane,
        OscillatorType, PolySynth, PortamentoConfig, PortamentoMode, SvfMode, SynthConfig,
        SynthFloat, Tuning, UnisonConfig, UnisonVoices, UnisonVoicesError, VoiceMode,
    };
}

/// SoundFont (.sf2) playback (`tutti-soundfont`): the unit and its settings.
/// Feature `soundfont`.
#[cfg(feature = "soundfont")]
pub mod soundfont {
    pub use tutti_sf2::{
        Bank as SoundFontBank, BankError, Error, GeneratorType, Instrument, InstrumentGenerator,
        InstrumentRegion, LoopMode, Pcm24, Preset, PresetGenerator, PresetRegion, SampleHeader,
        SamplePool, Side, SoundFont, SoundFontInfo, SoundFontVersion, SummedGenerator,
        WrongLevelGenerator,
    };
    pub use tutti_soundfont::{Polyphony, PolyphonyError, Settings, SoundFontUnit};
}

/// Plugin hosting (`tutti-plugin`): the main-thread proof, the bridge and
/// automation settings, the plugin catalog, and the faults a load or an editor
/// reports. Feature `plugin`; a format feature (`clap`, `vst3`, `au`, `vst2`)
/// decides which plugins load.
///
/// The sandbox's server, `tutti-plugin-server`, is a separate binary the host
/// spawns; build it with `cargo build -p tutti-plugin-server`.
#[cfg(feature = "plugin")]
pub mod plugin {
    pub use tutti_plugin_db::ScanBusy;
    pub use tutti_plugin_db::{
        BeforeEpoch, BlacklistedFile, FileFacts, FileFailure, FileRow, FileState, FilesError,
        Mtime, PluginEntry, ProbeFailure, RowChange, ScanError, ScanOutcome, ScanProgress,
        ScannedFile,
    };
    pub use tutti_plugin_db::{BindError, BlacklistView, Loadable, PersistError, Resolved};
    pub use tutti_plugin_host::{BlockLosses, RebindError};
    pub use tutti_plugin_ipc::RandomSourceError;
    pub use tutti_plugin_ipc::{IpcConfig, PluginRenderFault};
    pub use tutti_plugin_types::PresetSupport;
    pub use tutti_plugin_types::bundle::ModuleKind;
    pub use tutti_plugin_types::features::{FeatureReport, Features};
    pub use tutti_plugin_types::{
        AuComponentType, AutomationModeError, Axis, BlockCounts, ClapFeature, ContentScale,
        ControlRole, EditorPresence, EmptyVersionText, Helper, InvalidContentScale, IpcError,
        IpcResult, KeyEvent, KeyUse, LatencyVerdict, LinearRange, Locator, Measured, NotChecked,
        ParamFlags, ParamRange, ParamSteps, ParamUnit, ParameterInfo, PlainMap, PlainUnit,
        PluginClass, PluginError, PluginFormat, PluginTooWide, PluginVersion, ProbeConfig,
        StateError, StateHash, TailVerdict, UnknownExtension, Verdict, VersionText, Vst2Category,
        Vst3PlugType, Vst3SubCategories,
    };
    pub use tutti_plugin_types::{
        AutomationMode, MainThreadMarker, MainWork, MainWorkHandle, Preset,
    };
    pub use tutti_plugin_types::{EditorError, LoadStage, TerminationReason};
    pub use tutti_plugin_types::{
        FrameTooLarge, ResultError, SlabAccessError, SlabError, SlabLayoutError,
    };
    // What a `ProbeFailure` holds.
    pub use tutti_plugin_types::{ProbeDeath, ProbeError, ProbeExit, ProbeRefusal};
    pub use tutti_plugin_types::{Timeouts, ZeroTimeout};

    /// The plugin catalog: scanning, the database and the descriptors it
    /// holds.
    pub mod catalog {
        pub use tutti_plugin_db::{
            CatalogConfig, FormatMismatch, PluginDb, Probed, Scan, ScanPoll, ScanResult,
        };
        pub use tutti_plugin_ipc::AudioConfig;
        pub use tutti_plugin_types::{
            Described, FourCc, PluginDescriptor, PluginId, PluginIdError, PluginRole, Uid, Widths,
        };
    }
}

/// The names of the common MIDI path in one place:
/// a file (`MidiFile`, `Track`, `Tick`), a message (`Message`, `Ump`,
/// `MidiChannel`, `MidiGroup`, `NoteNumber`, `NoteVelocity`, `Voice1`, `SysexAssembler`, `Midi1Parser`), a timed event, the keys
/// that play an instrument or a live input (`MidiKeys`),
/// the session's MIDI (`Midi`, `InstrumentHandle`), its route rule, events and
/// errors (`MidiRouteRule`, `MidiRouteId`, `MidiReport`, `MidiError`), its
/// followers (`FollowTempo`, `FollowClock`, `FollowMachineControl`), the
/// sync node that sends the transport (`SyncOut`, `SyncMessages`), a host's own output (`midi_sink`, `MidiSink`,
/// `SinkLoaded`), and under `midi-hardware` a device (`DeviceId`,
/// `DeviceInfo`), the ports the pool opens for them (`midi_input`, `midi_output`,
/// `MidiInput`, `MidiOutput`, their entries and loaded reports) and what an open one said of itself (`Learned`, `SavedDevice`, `Match`), and the Profiles
/// its MIDI-CI peers support and how to switch one (`PeerProfiles`,
/// `ListedProfile`, `SwitchProfile`, `ProfilesChange`). The translation layer
/// (`Midi2Translator`, `Midi1Translator`), the runtime's clock sync and the MIDI-CI codec
/// belong to `tutti-midi-protocol`, `tutti-midi-runtime` and `tutti-midi-device`.
/// Feature `midi`.
#[cfg(feature = "midi")]
pub mod midi {
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_ci::{
        ChannelControllers, ChannelsDetail, CiCategories, CiDeviceId, CiPeer, DataControl,
        DetailsData, DiscoveryData, EnabledMpeZones, InquiryTarget, ManufacturerProfile,
        MaxSysexSize, MessageText, Nak, NakStatus, NotADataControl, NotADeviceId, NotASysexSize,
        NotProfileChannels, NoteData, ProcessError, ProcessFeatures, ProfileChange,
        ProfileDataError, ProfileEntry, ProfileOutcome, ProfileRequest, ProfileRequestError,
        ProfileState, ProfileTable, ReportRequest, ReportScope, SpecificData, StandardProfile,
        SystemMessages,
    };
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_ci::{Muid, NotAMuid};
    pub use tutti_midi_ci::{
        NotSetProfileOn, ProfileChannels, ProfileId, ProfileOnRequest, ProfileTarget,
    };
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_device::EndpointId;
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_device::{Identity, PortDirection, WinMmDevice};
    pub use tutti_midi_file::{
        Altered, Converted, DropReason, Dropped, Layout, Lead, LeadTrack, LyricChange,
        MalformedMeta, MetricalSequence, MetricalSequences, MetricalView, MicrosPerQuarter,
        MicrosPerQuarterOutOfRange, MidiFileKind, Moved, Normalized, OtherMetaKind,
        OtherMetaKindOutOfRange, OtherSmfFormat, OtherSmfFormatOutOfRange, OutOfOrder, PairedNote,
        Pattern, Placement, RefusedTimeSignature, SmfDivision, SmfPart, SmfTicksPerFrame,
        SmfTicksPerFrameOutOfRange, SmfTicksPerQuarter, SmfTicksPerQuarterOutOfRange, SmfTimeline,
        SmpteFractionalFrames, SmpteFractionalFramesOutOfRange, SmpteOffset, SmpteSequence,
        SmpteSequences, SmpteView, SysEx, TextKind, Timebase, TimelineDrop,
    };
    pub use tutti_midi_file::{
        ClipFileError, ReadError, Refusal, SmfParseError, ToClipError, TrackPart, TracksError,
        WriteError,
    };
    pub use tutti_midi_file::{FileEvent, Meta, MidiFile, SmfFormat, Tick, Track};
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_hardware::MidiHardwareError;
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_hardware::{
        InputTiming, NotAChannelCount, ProcessChange, ReportAsk, Reporter, WatchFailure,
    };
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_hardware::{MidiThread, NotFound, SeedError};
    pub use tutti_midi_protocol::{
        ChannelResponse, InvalidKeyTable, InvalidMpeZones, KeyTable, MmcSegmentError, MpeMode,
        MpeSpec, MpeZone, MpeZoneConfig, MpeZones, MtcRate, NotFrequencyData, NotSysexDeviceId,
        PitchBendSensitivity, Recipient, SmpteFrameRate, SmpteTimecode, SysexDeviceId,
        TuningFrequency,
    };
    pub use tutti_midi_runtime::{
        ClipEvents, ClockKind, ClockOffset, HarmonyChanges, HarmonyControls, HarmonyNode,
        MidiClipControls, MidiClipNode, MidiInputPorts, OffsetOutOfRange, OutputLatency,
        OutputOffset, OutputTerms, SegmentFault, SyncMessages, SyncOut, SyncOutControls,
        TimedHarmony,
    };
    pub use tutti_midi_runtime::{MidiKeys, MidiOut, Outgoing, SendAt, Taken, TimedMidiEvent};
    #[cfg(feature = "midi-hardware")]
    pub use tutti_ump::NoMidi1Form;
    pub use tutti_ump::{
        Accidental, Alteration, AlterationKind, ArticulationAttribute, Attribute, Bank, BarAccents,
        BendValue, BlockDirection, BlockFilter, BlockSelect, ChordBass, ChordName, ChordPitch,
        ChordType, ControlValue, Data128, DeltaTicks, DiscoveryFilter, EndpointInfoNotification,
        Field, FirstGroup, Flex, FlexAddress, FlexTextKind, FlexTimeSignature, Form, Framed,
        FunctionBlockCount, FunctionBlockId, FunctionBlockInfo, FunctionBlocks, GroupSpan,
        KeySignature, Lane, Letter, MdsChunk, MdsChunkSize, MdsChunks, MdsIdentity, Metronome,
        Midi1Bytes, Midi1Port, MixedDataHeader, MpeChannels, MpeOverlap, MpeZoneSet, MpeZoneSpan,
        NotChunkSize, NotManufacturerId, NoteKey, NoteKeyEvent, PerNoteManagement, Pitch,
        RegisteredData, RegisteredPerNoteController, Reserved, ReservedAccents, ReservedAttribute,
        Rpn, SharpsFlats, Stream, SyllableError, Sysex7Packet, Sysex7Packets, Sysex8Packet,
        Sysex8Packets, Sysex8Stream, Sysex8StreamIds, SysexError, SysexMessage, SysexPart,
        SysexRef, SysexStatus, SysexStep, System, U4, UiHint, UmpVersion, Unknown, Utility, Voice2,
    };
    pub use tutti_ump::{
        Address, CCNumber, DeviceIdentity, ExtremeValues, ManufacturerId, Message, Midi1Parser,
        MidiChannel, MidiGroup, NoteNumber, NoteVelocity, Protocol, StreamConfig, SysexAssembler,
        TicksPerQuarterNote, U7, U14, Ump, Voice1,
    };
    pub use tutti_ump::{
        CCNumberOutOfRange, ChordTypeOutOfRange, DeltaTicksOutOfRange, FirstGroupOutOfRange,
        FunctionBlockCountOutOfRange, FunctionBlockIdOutOfRange, GroupSpanOutOfRange,
        MidiChannelOutOfRange, MidiGroupOutOfRange, MpeChannelsOutOfRange, NoteNumberOutOfRange,
        SharpsFlatsOutOfRange, TicksPerQuarterNoteOutOfRange, U4OutOfRange, U7OutOfRange,
        U14OutOfRange,
    };
    pub use tutti_ump::{Join, PacketSpan, Spans};
    pub use tutti_ump::{
        LyricKind, NotASoftwareRevision, PlainTextKind, SoftwareRevision, TextError,
    };

    /// The session's one MIDI error: each cause (hardware, send, route) is its
    /// source.
    #[cfg(feature = "session")]
    pub use tutti_session::MidiError;
    #[cfg(feature = "session")]
    pub use tutti_session::RouteDeclarer;
    #[cfg(feature = "session")]
    pub use tutti_session::{
        AddedInstrument, AnyNode, DroppedEvent, FollowClock, FollowMachineControl, FollowTempo,
        InstrumentHandle, LoadMidi, LoadMidiError, LoadReport, LoadedMidi, LoadedTrack, Midi,
        MidiReport, MidiRouteId, MidiRouteRule, MidiSink, MidiSinkEntry, MidiSinkInfo,
        NoCommonTimelineError, NotInstrument, SinkLoaded, SinkOptions, Stamping, TrackError,
        midi_sink,
    };
    #[cfg(all(feature = "session", feature = "midi-hardware"))]
    pub use tutti_session::{
        InputLoaded, MidiInput, MidiInputEntry, MidiOutput, MidiOutputEntry, OutputLoaded,
    };

    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_ci::Whole;
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_device::{DeviceId, DeviceInfo, Learned, Match, MidiIdentity, SavedDevice};
    #[cfg(feature = "midi-hardware")]
    pub use tutti_midi_hardware::{
        ChannelCount, ChannelSwitcher, CiChange, CiPeerInfo, DeviceQuery, Errno, ErrnoOutOfRange,
        InputFault, MidiHardwareConfig, PeerError, PeerId, PeerProfiles, PeerRef, ProfileError,
        ProfileSwitch, ProfilesChange, Switcher, WholeSwitcher,
    };
    #[cfg(all(feature = "session", feature = "midi-hardware"))]
    pub use tutti_session::{
        ChannelProfile, ListedProfile, MidiInputInfo, MidiOutputInfo, MidiPolicy, MidiPolicyError,
        MidiPortError, NoHardware, OutputOptions, PortCause, ProfilePeer, ReportPeer,
        RequestReport, SwitchProfile, WholeProfile, midi_input, midi_output,
    };
    #[cfg(feature = "midi-hardware")]
    pub use tutti_ump::{EndpointName, InstanceIdError, Name, NameError, ProductInstanceId};
    /// MIDI-CI Property Exchange (M2-103 and its resource specifications): a
    /// peer's resources, read, written and subscribed to through the session.
    /// Under `plugin` alone it holds only the `MediaType` a `PeerState` carries.
    #[cfg(any(feature = "midi-hardware", feature = "plugin"))]
    pub mod property {
        pub use tutti_midi_ci::MediaType;
        #[cfg(feature = "midi-hardware")]
        pub use tutti_midi_ci::mcoded7::Mcoded7Error;
        #[cfg(feature = "midi-hardware")]
        pub use tutti_midi_ci::{
            Ack, AckStatus, ActionError, BodyLimit, BufferLimit, CallError, CallId, CanSet,
            CcIndex, ChangeKind, ChunkNo, ClusterKind, ColumnSource, ConfirmIcon, ConfirmType,
            Confirmation, CtrlControl, CtrlKind, CtrlRange, CtrlValueKind, CustomResource,
            EntryError, InvalidValue, JsonPointer, LinkRole, ListedTwice, ModeId, NotABodyLimit,
            NotABufferLimit, NotAChunkNo, NotAPeChannel, NotAPeMidiMode, NotAPriority,
            NotARequestId, NotASimultaneousRequests, NotAStateSize, NotAStepCount, NotAStreamLimit,
            NotATimeoutWait, NotSignificantBits, NotUnixSeconds, NotVendorName, OpaqueBody,
            PaginationFault, ParamPath, PartialError, PartialUpdate, PartialValue, PeAllCtrlEntry,
            PeAllCtrlList, PeAnswer, PeBankProgram, PeBasicChannelRx, PeBasicChannelTx,
            PeBodyError, PeCacheTime, PeCapabilities, PeCapabilitiesFailure, PeChCtrlEntry,
            PeChCtrlList, PeChannel, PeChannelEntry, PeChannelList, PeChannelMode, PeChunkError,
            PeCluster, PeColumn, PeController, PeCtrl, PeCtrlMapEntry, PeCtrlMapList,
            PeCurrentMode, PeData, PeDataError, PeDataFault, PeDecimal, PeDeviceInfo, PeEncoding,
            PeEndedBy, PeError, PeExtensions, PeExternalSync, PeExtra, PeExtraError, PeFlowControl,
            PeHeaderError, PeHeaderTooLong, PeJson, PeJsonError, PeJsonSchema, PeLink, PeLocalOn,
            PeMessage, PeMidiMode, PeModeEntry, PeModeList, PeObject, PeOpaque, PeOutcome, PePage,
            PeProgramEntry, PeProgramList, PeReassemblyError, PeReply, PeRequestId, PeResource,
            PeResourceList, PeResponse, PeSignal, PeState, PeStateEntry, PeStateList, PeStatus,
            PeStatusClass, PeTerminator, PeText, PeTextError, PeTransaction, PeUnreadable,
            PeVersion, Place, Priority, PropertyName, PropertyNameError, RefusedAt, RequestRefused,
            ResIdUse, ResourceDefaults, ResourceEntry, ResourceEntryBuilder, ResourceEvent,
            ResourceFailure, ResourceGet, ResourceId, ResourceName, ResourceSet, ResourceTarget,
            ResponderEvent, RoleName, SelectorAction, SetBody, SignificantBits, SimpleAction,
            SimultaneousRequests, StateId, StateRev, StateSize, StateStamp, StepCount, StreamLimit,
            SubscribeId, TimeoutWait, TypeHint, UnixSeconds, UnlistedText, UpdateKind,
        };
        #[cfg(feature = "midi-hardware")]
        pub use tutti_midi_hardware::{PropertyAsk, PropertyCall, PropertyChange, PropertyPeer};
        #[cfg(all(feature = "session", feature = "midi-hardware"))]
        pub use tutti_session::{
            GetProperty, ListedResource, PeFailure, ResourcePeer, SetProperty, Subscribe,
            Subscription, SubscriptionCause,
        };
    }
}
