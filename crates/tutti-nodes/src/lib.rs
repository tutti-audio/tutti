#![doc = include_str!("../README.md")]
//! The Tutti audio engine's built-in DSP nodes: filters, delays, dynamics,
//! modulation effects, mixing and automation.
//!
//! Every node here is a graph node (`tutti_graph::Node`), and a node file holds
//! only the adapter: shape, prepare, process, params and fork. The algorithm each
//! one runs is a pure function or struct over slices in `tutti-dsp`: the filter
//! coefficients and steps (`SvfDesign`, `LadderDesign`, `CrossoverModel`), the dynamics
//! curves and recurrences, `Convolver`, `DelayLine` and the waveshaping curves. Build one, insert it
//! into a graph (`tutti_graph::{GraphBuilder, Editor}`), wire it, and the graph
//! prepares it at the device rate and renders it. Inserting a node hands back its
//! controls, usually a `tutti_graph::ParamSet` addressed by `UnitParam` (a
//! `UnitAddr` for a node with several params of a kind), which change the running
//! node without a lock.
//!
//! - **Filters** — `SvfFilterNode`, `LadderFilterNode` (both any width, one
//!   coefficient solve shared across the channels), `EqBandNode`, `CutFilterNode`, a
//!   Butterworth or Linkwitz-Riley high-pass or low-pass of 6 to 96 dB per octave,
//!   `BellFilterNode`, a peaking equalizer matched to the analog bell up to Nyquist, and
//!   `TiltFilterNode`, a first-order tilt about a pivot, and
//!   `EnvelopeFilterNode`, an SVF whose cutoff follows the input's level.
//! - **Crossover** — `CrossoverNode`, which makes `bands × channels` outputs from
//!   `tutti_dsp::CrossoverBank`, a 2 to 6 band Linkwitz-Riley 4th-order split (the
//!   bands sum to flat magnitude; IIR, so not linear-phase and no latency). Each
//!   crossover is an indexed `Cutoff` param (`UnitAddr::new(UnitParam::Cutoff, i)`).
//! - **Delay** — `DelayNode` (any width, with a checked cross-feedback `Routing`),
//!   a `KernelNode` over `DelayKernel` (`tutti_dsp::DelayBank`); the time and feedback are
//!   read per frame when driven.
//! - **Dynamics** — `CompressorNode`, `GateNode`, `BrickwallLimiterNode` and
//!   `LimiterNode` (each a `KernelNode` over a bank of `tutti_dsp`: the compressor and the
//!   gate read a typed `KeySource`, through an optional `KeyFilter`; the limiter reports its
//!   lookahead as latency), and
//!   `TransientShaperNode` (the attack and the sustain of a signal raised or lowered apart,
//!   one linked gain from a fast and a slow envelope), `LevellerNode` (a loudness leveller:
//!   one linked gain moving the BS.1770 short-term loudness to a target, within a boost and a
//!   cut and held under a gate), and
//!   `MultibandCompressorNode` (2 to 4 bands, a linked
//!   compressor per band of a Linkwitz-Riley crossover, a hand-written node over
//!   `tutti_dsp::MultibandBank`). Every setting is an indexed param: a
//!   threshold, knee, ratio, attack, release and make-up (`GainDb`) per band, a
//!   `Cutoff` per crossover (27 ports at 4 bands), and a `Bypass` and `Solo`
//!   toggle per band (`>= 0.5` is on, as every toggle; cells without a port). The
//!   per-band gain reduction meters are readings on `MultibandControls`.
//! - **Modulation effects** — `ModDelayNode` (chorus and flanger are
//!   its two `Sweep` presets, a `KernelNode` over `ModDelayKernel`; the General MIDI chorus a third,
//!   with no feedback and a finite tail) and `PhaserNode` (a `KernelNode` over `PhaserKernel`, its `Stages`
//!   and `SweepRange` checked; the rate is floored at 0.01 Hz, a control signal's value and a cell's alike), both any width.
//! - **Distortion** — `DistortionNode` and its `ShapeKind` waveshapers.
//! - **Bit crusher** — `BitCrusherNode`, a sample-and-hold and a quantizer, with no tail,
//!   from a `BitCrusherConfig`.
//! - **DC blocker** — `DcBlockerNode`, a first-order high-pass with a finite tail, from a
//!   `DcBlockerConfig`.
//! - **Modulation sources** — `LfoNode`, the per-sample LFO over
//!   `tutti_modulation::Waveform`, the wave a control lane evaluates: an audio generator, whose output feeds a node's
//!   audio input. Its `LfoClock` is free (a rate in `Hz`, a param) or synced to the
//!   transport beat (a span fixed at build); its depth and rate are read per frame.
//! - **Oscillator** — [`OscillatorNode`], a mono source: a sine, or a saw, square or triangle
//!   band-limited by `tutti_dsp::Oscillator`; its frequency (`Frequency`) and level (`Volume`) are read per frame.
//! - **Dry/wet** — the convolver, delay, chorus/flanger, phaser and the two reverbs output their wet signal only; blend with the input through the graph's `set_mix`.
//! - **Level and pan modulation** — `TremoloNode` and `AutoPanNode` (an LFO of a chosen
//!   `OscillatorWave` scales the level, or places a stereo pair by a `PanLaw`) and `RingModNode`
//!   (the input times a sine carrier), from a `TremoloConfig`, `AutoPanConfig` and `RingModConfig`.
//! - **Mixing** — `BusStripNode` (volume / balance; a mute or bypass is the graph's `NodeMode`).
//!   Summing is the graph's: a port or an output channel sums its sources; so is
//!   folding between layouts (a bus edge between declared buses).
//! - **Utilities** — `PolarityNode` (chosen channels inverted, from a `PolarityConfig`),
//!   `MidSideNode` (stereo L/R to M/S and back, from a `MidSideConfig`) and `StereoWidthNode`
//!   (the side scaled by a ramping `StereoWidth`, from a `StereoWidthConfig`).
//! - **Spatial** — `VbapPannerNode` (stereo in, one output per channel of a `Bed`)
//!   and `HrtfBinauralNode` (two outputs, by FFT convolution against a
//!   caller-supplied HRIR sphere), both steered by their params (bearing,
//!   height, spread and width, or the blend), ramped. A mix of several panned sources
//!   is plain fan-in: wire each panner's outputs to the same graph ports.
//! - **Param ports** — each modulatable node declares its params once, with its
//!   cells (`tutti_graph::declare_params!`), as param ports, filled per
//!   frame from one event source each (read through `Io::param`).
//! - **Modulation** — the `automation` module's `ControlLaneNode`, which
//!   evaluates a `tutti_modulation::ControlProgram` into a param port,
//!   the signal sources that feed a lane (`SourceNode<S>`, the one sender of a
//!   `tutti_modulation::ControlSource`: `EnvelopeFollowerNode`,
//!   `NoteEnvelopeNode`), and `Modulation`, the reconciler that builds them from
//!   `tutti_modulation` declarations.
//! - **Automation / convolution** — the `automation` module's `AutomationLaneNode`
//!   (a curve as audio) and recording units, and `ConvolverNode`.
//!
//! A `*Node` is the graph-facing node; the inner object some are built from,
//! `Waveform`, carries no suffix, because it is not a node and
//! can be used on its own. The algorithms are `tutti-dsp`'s (`tutti_dsp::DelayLine`,
//! `tutti_dsp::CrossoverBank`, …); this crate re-exports only the types in a node's
//! signature: `SvfType`, `LadderType`, `SimdLanes`, `CrossoverError`,
//! `InterpolationMode`, `OversamplingFactor` and `ShapeKind`.
//!
//! Nothing in this crate can fail: there is no `Error` type, and a node is ready
//! to insert the moment it is built.
//!
//! ## Scope
//!
//! - **No room simulation and no HRIR data.** The panners place a source; the
//!   HRIR dataset is supplied by the caller as bytes.
//! - **No sample playback and no synthesis.** Those are `tutti-sampler`,
//!   `tutti-polysynth` and `tutti-soundfont`.
//!
//! ## Quick start
//!
//! ```rust
//! # use tutti_buffers::Planes;
//! use tutti_buffers::MaxBlock;
//! use tutti_units::{Hz, Q, UnitParam};
//! use tutti_graph::{Prepare, Standalone};
//! use tutti_nodes::{SvfFilterNode, SvfType};
//! use tutti_units::{SampleRate, Samples};
//!
//! // A lowpass alone in a graph, fed by the graph's input. The filter is a
//! // graph node: inserted, it hands back its `ParamSet` — cutoff, Q and
//! // gain by `UnitParam` — and the graph prepares it at the graph's rate.
//! let filter = SvfFilterNode::new(SvfType::LowPass, Hz::new(800.0), Q::new(0.707));
//! let mut standalone = Standalone::new(filter, Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))));
//! let out = standalone.render_input(Planes::new(&[&[1.0; 64]]).unwrap());
//!
//! // Sweeping the cutoff on the *running* node: the set writes the `Param`
//! // cell the node reads, at the start of its next block.
//! assert!(standalone.controls().set(UnitParam::Cutoff, 2_000.0));
//! let out = standalone.render_input(Planes::new(&[&[1.0; 64]]).unwrap());
//! ```
//!
//! ## Changing a running node
//!
//! Once a node is in a graph it belongs to the executor, so the control thread
//! reaches it only through what it shares: its `Param<U>` cells and atomics,
//! reached through the controls it was inserted with. A setter that takes
//! `&self` writes such a cell; it is lock-free and lands on the node's next
//! block. A setter that takes `&mut self` restructures the node and is meant for
//! before insertion (or for a replacement node). Clones of a node share its
//! cells, so a control handle stays valid for the node it came from.
//!
//! `ParamSet::set` returns `false` for a param the node was not inserted with,
//! so a write to the wrong address is visible at the call site. A host pushing
//! one value to many nodes can ignore the `false` from the ones that do not own
//! it.
//!
//! ### Writing your own node's controls
//!
//! The same rule applies to a node you write: a value that changes while the
//! node renders lives behind an `Arc`, shared across clones.
//!
//! | the value | mechanism |
//! |---|---|
//! | one `f32` with a unit newtype | `Param<U>` + `&self` setter + its `UnitParam` address in the node's `ParamNode::param_set` |
//! | one `bool`, or a small `Copy` enum | `Arc<AtomicBool>` / `Arc<AtomicU8>` + `&self` setter. A bool rides `UnitParam`'s `>= 0.5` encoding, since `ParamSet::set` carries an `f32`. |
//! | a multi-field struct, or anything heap-backed | a command queue: flatten the struct into scalar fields, allocate sender-side, drain in the callback |
//!
//! A plain `AtomicBool` field is not enough: `Clone` copies the atomic rather
//! than sharing it, so writes to one clone never reach another. `Publisher` is
//! for moving a *deallocation* off the audio thread (routing tables, delay
//! vectors), not for sharing a small `Copy` value.
//!
//! ## Real-time behaviour
//!
//! The nodes' `process` paths allocate nothing and take no lock, at any block
//! length up to the graph's prepared maximum; buffers are sized at construction
//! and in `prepare`, on the control thread. The recursive nodes (filters, delays,
//! modulation effects, dynamics) read a non-finite control value (NaN, ±∞) as
//! unchanged, so it never reaches their state.
//!
//! ## Where it sits
//!
//! The `tutti` crate re-exports this one as `tutti::nodes`;
//! `tutti-polysynth`, `tutti-export` and `tutti-plugin` build on
//! it. It re-exports the unit types (`Hz`, `Db`, `Seconds`, …) from `tutti-units` and
//! `Param` from `tutti-rt`.
//!
//! ## Features
//!
//! `default = []`. The FFT convolution reverb is not a feature: `ConvolverNode` at any width, with
//! `IrLayout`, over `tutti_dsp::Convolver` and `MatrixConvolver`, whose FFT is
//! `tutti_dsp::RealFft` (scalar, so the output is CPU-independent). An IR's
//! spectra are stored once (`tutti_dsp::IrSpectra`, behind an `Arc`) and shared
//! by every channel and every fork of the graph.
//!
//! - `testing` — stimulus and plumbing nodes for tests, examples and benches
//!   (`Const`, `Osc`, `Through`, `Split`, `Sink`). Enable it from
//!   `[dev-dependencies]` only.
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
//!
//! ## Sample rate
//!
//! Every time constant here is derived from a sample rate: delay taps and ring
//! lengths from [`Seconds`], filter coefficients from a cutoff in [`Hz`]
//! against Nyquist, envelope attack and release coefficients, LFO phase
//! increments. A node built over a kernel ([`KernelNode`]: the filters, delays, dynamics,
//! modulation effects and reverbs) has no rate until `prepare` gives it one: it builds its
//! bank and every figure derived from the rate there, so an unprepared
//! node holds none. Before `prepare` it declares no latency and an unknown tail, which the
//! graph reads again after it, and renders silence. A setting that names a frequency is
//! checked for what a rate does not change (finite, above 0, in order), and a frequency
//! past the prepared rate's Nyquist is held under it, not refused. The nodes that are not
//! kernels ([`LfoNode`], [`ConvolverNode`]) take their rate from `prepare` too. To drive a node
//! outside a graph, prepare it first (`tutti_graph::Standalone`, or
//! `tutti_graph::contract::prepared` in tests).
//!
//! ## Items
//!
//! - Filters: [`SvfFilterNode`] ([`SvfType`]), [`LadderFilterNode`]
//!   ([`LadderType`]), [`EqBandNode`], [`CutFilterNode`] ([`CutResponse`]), [`BellFilterNode`],
//!   [`TiltFilterNode`], [`EnvelopeFilterNode`], [`FirConvolverConfig`] (a
//!   linear-phase equalizer: a [`tutti_dsp::Fir`] through the convolver); the coefficient solvers
//!   [`SvfDesign::new`](tutti_dsp::SvfDesign::new) and
//!   [`LadderDesign::new`](tutti_dsp::LadderDesign::new); a node runs in the float of the
//!   graph it is inserted in (`tutti_buffers::Float` with [`SimdLanes`]).
//! - Crossover: [`CrossoverNode`], over `tutti_dsp::CrossoverBank`, a 2 to 6 band
//!   Linkwitz-Riley 4th-order split (solved on the stream's 16-frame grid), each
//!   crossover frequency an indexed [`Cutoff`](tutti_units::UnitParam::Cutoff) param, with
//!   [`CrossoverError`].
//! - Delay: [`DelayNode`] over `tutti_dsp::DelayBank`, built to a [`DelayLayout`], with
//!   [`InterpolationMode`] and [`MaxDelay`].
//! - Dynamics: [`CompressorNode`], [`GateNode`], [`LimiterNode`],
//!   [`BrickwallLimiterNode`], [`TransientShaperNode`], [`LevellerNode`] (made from a
//!   [`LevellerConfig`], which can refuse a bus with no placement) and [`MultibandCompressorNode`] (2 to 4
//!   bands over a Linkwitz-Riley crossover, every setting an indexed param: see
//!   [`BAND_PARAMS`]) with its [`MultibandControls`].
//! - Modulation effects: [`ModDelayNode`] (over `tutti_dsp::ModDelayBank`, its [`Sweep`] and
//!   [`Stagger`] checked) and [`PhaserNode`] (over `tutti_dsp::PhaserChain`, its [`Stages`] and
//!   [`SweepRange`] checked); [`DistortionNode`] with [`ShapeKind`].
//! - Modulation sources: [`LfoNode`], over `tutti-modulation`'s [`Waveform`].
//! - Signal-driven modulation sources, feeding a
//!   control lane's event input: [`EnvelopeFollowerNode`] and
//!   [`NoteEnvelopeNode`].
//! - Spatial: [`VbapPannerNode`] ([`VbapError`]) and [`HrtfBinauralNode`],
//!   steered by their params (bearing, height, and the VBAP spread and width
//!   or the HRTF blend).
//! - Mixing: [`BusStripNode`]. Summing is the graph's (a port or an output
//!   channel sums its sources), not a node's, and so is folding between
//!   channel layouts (a bus edge between declared buses).
//! - Param ports: the params each node declares once, with its cells
//!   (`tutti_graph::declare_params!`), which a control lane drives
//!   ([`ControlLaneNode`](automation::ControlLaneNode)).
//! - Modules: [`automation`] (lanes and recording).
//!

// The fallible constructions are the crossover's (`CrossoverControls::new`,
// `CrossoverNode::new`, `MultibandCompressorNode::new`), all `CrossoverError`;
// the rest of this crate has no `Error` type.

// Per-block control reads and the ramps that keep them from stepping.
mod buses;
#[macro_use]
mod params;
mod ramp;
mod tail_clock;

// Tests of every kernel node in a graph of `f64`.
#[cfg(test)]
mod f64_graph;
// Rendering helpers for the width-generic nodes' unit tests.
#[cfg(test)]
mod test_support;

pub use tutti_rt::Param;
pub use tutti_units::{
    Amplitude, ArcDegrees, Azimuth, BitDepth, Bpm, Cents, CentsSpan, CompressionRatio, Db, Depth,
    Drive, Elevation, Feedback, HoldFactor, Hz, Mix, Q, Resonance, SampleRate, Seconds, Semitones,
    Spread, StereoWidth, Unit,
};

// The boundary this crate holds: pure DSP graph nodes plus the `ParamSet` /
// controls surface a host drives them through, and the modulation reconciler
// (`Modulation`) that builds this crate's lanes and signal nodes from
// `tutti_modulation` declarations. DAW-param policy is the host's, and
// deliberately outside this workspace.

mod click;
pub use click::{
    ClickConfig, ClickLevel, ClickLevelError, ClickNode, ClickSettings, ClickState, MetronomeMode,
};

mod lfo;
// `LfoShape` and `Waveform` are re-exported from `tutti-modulation` through
// `lfo`, so `use tutti_nodes::LfoShape` works.
pub use lfo::{LfoClock, LfoNode, LfoShape, Waveform};

mod oscillator;
pub use oscillator::OscillatorNode;

mod delay;
pub use delay::{DelayKernel, DelayNode, DelaySet};
// `InterpolationMode` and the delay's controls are in `DelayNode::new`'s signature;
// the ring itself (`DelayLine`) is `tutti-dsp`'s only.
pub use tutti_dsp::{
    ChannelTimes, ChannelTimesError, DelayControls, DelayLayout, InterpolationMode, MaxDelay,
    MaxDelayError, Routing, RoutingError,
};

mod distortion;
pub use distortion::{DistortionKernel, DistortionNode, DistortionSet};
// `ShapeKind` and `OversamplingFactor` are in `DistortionNode`'s signatures; what is behind
// them is `tutti-dsp`'s.
pub use tutti_dsp::{Antialiasing, OversamplingFactor, ShapeKind};

// A sample-and-hold and a quantizer.
mod bit_crusher;
pub use bit_crusher::{BitCrusherConfig, BitCrusherKernel, BitCrusherNode, BitCrusherSet};
pub use tutti_dsp::BitCrusherControls;

// A first-order high-pass that removes a constant offset.
mod dc_blocker;
pub use dc_blocker::{DcBlockerConfig, DcBlockerKernel, DcBlockerNode, DcBlockerSet};
pub use tutti_dsp::DcBlockerControls;

mod filter;
pub use filter::{
    BellFilterConfig, BellFilterControls, BellFilterKernel, BellFilterNode, BellFilterSet,
    CrossoverKernel, CrossoverNode, CrossoverSet, CutAlignment, CutFilterConfig, CutFilterControls,
    CutFilterKernel, CutFilterNode, CutFilterSet, EqBandNode, EqControls, EqKernel,
    LadderFilterNode, LadderKernel, LadderSet, MIN_CUT_CUTOFF, SvfFilterNode, SvfKernel, SvfSet,
    TiltFilterConfig, TiltFilterControls, TiltFilterKernel, TiltFilterNode, TiltFilterSet,
};
pub use filter::{
    EnvelopeFilterConfig, EnvelopeFilterKernel, EnvelopeFilterNode, EnvelopeFilterSet,
};
pub use tutti_dsp::EnvelopeFilterControls;
pub use tutti_dsp::{ButterworthOrder, CutResponse};
// The types in the filter nodes' signatures stay reachable from here; the
// algorithms behind them (`SvfDesign`, `LadderDesign`, `CrossoverModel`) are
// `tutti-dsp`'s only.
pub use tutti_dsp::{
    CrossoverControls, CrossoverError, LadderControls, LadderType, SimdLanes, SvfControls, SvfType,
};

mod dynamics;
pub use dynamics::{
    BAND_PARAMS, BrickwallKernel, BrickwallLimiterNode, BrickwallSet, CompressorKernel,
    CompressorNode, CompressorSet, DynamicEqConfig, DynamicEqKernel, DynamicEqNode, DynamicEqSet,
    GateKernel, GateNode, GateSet, LevellerConfig, LevellerKernel, LevellerNode, LevellerSet,
    LimiterKernel, LimiterNode, LimiterSet, MultibandCompressorNode, MultibandControls,
    TransientShaperConfig, TransientShaperKernel, TransientShaperNode, TransientShaperSet,
};
// The types in the dynamics nodes' constructors: the controls a node is built at, and the
// key a compressor or a gate listens to.
pub use tutti_dsp::{
    ChannelWeightError, CompressorControls, DynamicEqControls, DynamicMode, GateControls,
    KeyFilter, KeySource, LevellerControls, LimiterControls, Lookahead, PeakDetection,
    TransientControls,
};

// Utilities: polarity, mid/side and stereo width.
mod mid_side;
mod polarity;
mod stereo_width;
pub use mid_side::{MidSideConfig, MidSideNode};
pub use polarity::{PolarityConfig, PolarityError, PolarityNode};
pub use stereo_width::{StereoWidthConfig, StereoWidthKernel, StereoWidthNode, StereoWidthSet};
// The controls a width node starts at, and the channel a polarity config can get wrong.
pub use tutti_dsp::{ChannelOutOfRange, WidthControls};

// Modulation effects on the level and the signal: tremolo, auto-pan and ring modulation.
mod auto_pan;
mod ring_mod;
mod tremolo;
pub use auto_pan::{AutoPanConfig, AutoPanKernel, AutoPanNode, AutoPanSet};
pub use ring_mod::{RingModConfig, RingModKernel, RingModNode, RingModSet};
pub use tremolo::{TremoloConfig, TremoloKernel, TremoloNode, TremoloSet};
// The controls those start at, the wave of their LFOs and the law an auto-pan places by.
pub use tutti_dsp::{AutoPanControls, OscillatorWave, PanLaw, RingModControls, TremoloControls};

// The mixer strip: volume, stereo balance, mute. Ungated: a fader is not a
// spatial concept.
mod strip;
pub use strip::BusStripNode;

// The one node over an effect's algorithm and state: the driver and the trait an
// algorithm implements to run in it.
mod kernel;
pub use kernel::{ControlSignals, Kernel, KernelNode, MAX_SIGNALS};

pub use params::{At, Control, ControlRead, ControlSet, ControlValid, HasControl, Params};

// Freeverb as a `KernelNode`, and the send (mono in, stereo wet out) the SoundFont
// player's General MIDI setup uses.
mod reverb_freeverb;
pub use reverb_freeverb::{FreeverbKernel, FreeverbNode, FreeverbSet};

// The feedback delay network as a `KernelNode`.
mod reverb_fdn;
pub use reverb_fdn::{FdnKernel, FdnNode, FdnSet};

// Dattorro's plate as a `KernelNode`.
mod reverb_plate;
pub use reverb_plate::{PlateKernel, PlateReverbConfig, PlateReverbNode, PlateSet};

// The banks' controls that sit in a node's signature.
pub use tutti_dsp::{
    FdnControls, FdnLayout, FreeverbControls, FreezeMode, MaxDecay, PlateControls,
};

// The spatial panners: VBAP speaker panning and binaural HRTF rendering.
mod spatial;
pub use spatial::{HrtfBinauralNode, VbapError, VbapPannerNode};

mod modulation;
pub use modulation::{
    ModDelayKernel, ModDelayNode, ModDelaySet, PhaserKernel, PhaserNode, PhaserSet,
};
// The sweep, its stagger and the controls are in `ModDelayNode::new`'s and `PhaserNode::new`'s
// signatures.
pub use tutti_dsp::{
    ModDelayControls, PhaserControls, Stages, StagesError, Stagger, StaggerError, Sweep,
    SweepError, SweepRange, SweepRangeError,
};

// The control sources' node (`SourceNode<S: ControlSource>`) and the
// signal-driven sources a `tutti_modulation::compile` declaration describes
// (`SignalSource`): nodes that send a raw value into control lanes.
mod signal;
pub use signal::{
    EnvelopeFollowerNode, NoteEnvelopeNode, SOURCE_EVENT_CAPACITY, SourceNode, SourcePorts,
};

// Declarations compiled and applied to an `Editor` as lanes, signal-source
// nodes and edges.
mod reconcile;
pub use reconcile::{Applied, Feed, ModHost, Modulation, ReconcileError, Target};
mod convolution;
pub use convolution::{ConvolverError, ConvolverNode, FirConvolverConfig, IrLayout, PortSide};

// Test and stimulus nodes (`Const`, `Osc`, `Through`, `Split`, `Sink`): what a
// test, example or bench wires a graph out of.
//
// Behind the `testing` feature, not `cfg(test)`: the consumers are *other*
// crates' tests, which a `cfg(test)` module is invisible to.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

// No `///` here on purpose: a doc comment on a `pub mod` line shadows the
// module's own `//!` and re-resolves its intra-doc links in this scope, which
// breaks every link the module makes to its own items. See `automation/mod.rs`.
pub mod automation;

// ECS bindings for automation and panning are host-side: they bind DAW
// `Volume`/`Pan`/`PluginParam` components, which are not this engine's
// vocabulary.
