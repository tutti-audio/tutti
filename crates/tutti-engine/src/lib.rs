#![doc = include_str!("../README.md")]
//! The per-block graph render of the Tutti audio engine.
//!
//! ## What this is
//!
//! `Engine` is what the real-time callback runs: each block it renders
//! `tutti-graph`'s `Executor` (whose `Editor` the control thread keeps), folds the
//! output to the device width, drives the transport's clock and applies timed
//! transport commands on their frame. A node reads musical time per frame from its
//! block's `Env` (`Env::for_each_beat`, `Env::transport_at`) rather than
//! consulting the transport.
//!
//! Use it directly when you drive the audio callback yourself (a custom device
//! layer, an offline renderer, a test harness); otherwise take the `tutti` facade,
//! whose `Session` owns an engine.
//!
//! ## What this crate does not own
//!
//! - **The transport.** `Transport`, its motion state machine and timed commands
//!   are `tutti-timeline`'s; the engine holds the transport's `TransportAudio` and
//!   applies what the transport schedules.
//! - **Level metering.** A meter is an op in the compiled plan, on the outputs or
//!   any node's port (`tutti-graph`'s `Editor::open_meter`, `MeterReader`), per
//!   channel and before the engine's fold and declick. The engine holds no main output
//!   meter and no stereo fold. The engine times each block and writes
//!   its [`Load`] and [`BlockCounts`], because they are not audio.
//! - **Taps.** Copying audio out of the graph is `tutti-graph`'s (`Editor::open_tap`,
//!   `TapReader`), an op in the compiled plan, before the declick fader. The engine
//!   writes the other kind itself: `Engine::take_heard_taps()` opens a tap on its
//!   device-width output after the fader (what is heard), read through the same
//!   `TapReader`.
//! - **MIDI.** `Engine` is MIDI-free; MIDI travels on the graph's event ports
//!   (`tutti-graph`), and the MIDI nodes are `tutti-midi-runtime`'s.
//! - **The device.** Opening a sound card's stream is `tutti-cpal`'s job; this
//!   crate owns the block interface under it ([`StreamDriver`], [`OutputBlock`]), so a
//!   driver of any other clock calls the same block.
//! - **The value vocabulary.** The unit newtypes are `tutti-units`'; the `Source`/`Sink` traits
//!   are `tutti-buffers`'. Delay compensation (`latency_plan`,
//!   `delays`) is `tutti-graph`'s pure pass, applied by the graph compiler.
//! - **The worker pool and the pre-roll table.** `tutti-graph`'s.
//! - **The metronome.** `ClickNode` is `tutti-nodes`'.
//! - **The main output width ceiling.** `tutti_buffers::MAX_CHANNELS` bounds the engine's
//!   editor, as it bounds a bus.
//!
//! ## Example — a graph, an engine, and a transport
//!
//! The engine's centre in one block: build a graph, hand its executor to an
//! `Engine` over a `Transport`, keep the editor, and render a block. `tutti-cpal`
//! does exactly this around a real device; here the block is pulled by hand, so
//! the whole thing runs headless.
//!
//! ```rust
//! use tutti_buffers::MaxBlock;
//! use tutti_graph::{OutputPort, PortSource};
//! use tutti_units::{Beat, Bpm, SampleRate, Samples, Tail};
//! use tutti_buffers::ChannelLayout;
//! use tutti_buffers::InterleavedMut;
//! use tutti_graph::NodeKey;
//! use tutti_engine::Engine;
//! use tutti_timeline::{MotionCommand, Timeline, Transport};
//! use tutti_graph::{Cx, Editor, ForkByClone, Io, Node, Prepare, Shape, Status};
//!
//! // A node reads the transport from each block's `Env`. This one puts the beat
//! // on two ports (whole beats, then the fraction). Tone generators and filters
//! // are `tutti-nodes`'; its tests use them through a dev edge.
//! #[derive(Clone)]
//! struct Beats;
//!
//! impl Node for Beats {
//!     fn shape(&self) -> Shape {
//!         Shape::source_unassigned(ChannelLayout::STEREO).tail(Tail::Unbounded)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         cx.env.for_each_beat(|i, beat| {
//!             io.output(0)[i] = beat.floor().get() as f32;
//!             io.output(1)[i] = beat.fract().get() as f32;
//!         });
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! let transport = Transport::new(SampleRate::SR_48K);
//!
//! // The editor is the control thread's half of the graph; the executor goes to
//! // the engine. Every edit reaches it through a `commit`.
//! let (mut editor, executor) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(256))));
//!
//! // Every insert says whether the node forks (for an export): `Beats` reads
//! // only its block's `Env`, so a clone of it is a fork.
//! let beats = NodeKey(1);
//! editor.insert(beats, "beats", ForkByClone(Beats));
//! editor
//!     .wire_outputs((0..2).map(|port| PortSource::Node(OutputPort::new(beats, port))))
//!     .expect("the ports exist");
//! editor.commit().expect("the graph compiles");
//!
//! let engine = Engine::new(&transport, &mut editor, executor).expect("within the limits");
//!
//! // Transport is a `motion`/`settings` split rather than a `play()` method:
//! // settings anyone may store into, motion a state machine that may defer or
//! // reject. Queued events apply at the next block, which the audio callback
//! // renders.
//! transport.settings().set_tempo(Bpm::of(90.0));
//! transport
//!     .motion()
//!     .send(MotionCommand::play_from(Beat::new(8.0)))
//!     .expect("the motion queue has room at startup");
//!
//! let mut block = vec![0.0f32; 256 * 2];
//! engine.process(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
//!
//! // The first frame carries beat 8 on the node's ports; the block moved the
//! // playhead on by 256 frames at 90 BPM.
//! assert_eq!((block[0], block[1]), (8.0, 0.0));
//! assert!(transport.is_rolling());
//! assert!(transport.beat() > Beat::new(8.0));
//! ```
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

mod error;
pub use error::{Error, Result};

mod block_clock;
mod callback;
mod callback_frames;
mod engine;
mod faults;
mod hearing;
mod latency;
mod load;
mod manual;
mod output;
mod restart;
mod routing;
mod stage;
mod stream;
mod timer;
pub use block_clock::{BlockClockReader, BlockDates, BlockReading, Playhead};
pub use callback::{AudioCallbackState, MAX_FRAMES, OutputBlock, process_audio};
pub use callback_frames::{CallbackFrames, CallbackSpan};
pub use engine::{DEFAULT_BLOCK_CAPACITY, Engine, GraphEngineError};
pub use faults::{
    BackendErrorKind, Lagging, RT_TEXT_BYTES, STALL_GRACE, StreamFault, StreamFaultKind,
    StreamFaults,
};
pub use hearing::HeardTaps;
pub use latency::StreamLatency;
pub use load::{BlockCounts, LOAD_MEAN_TIME_CONSTANT, Load};
pub use manual::{ManualRunning, ManualStream, ManualStreamDriver};
pub use output::{MAX_PENDING_MAIN_OUTPUTS, OutputControl, OutputPlacement};
pub use restart::{Stopped, StreamHost};
pub use routing::{
    OutputLevelError, OutputLevels, OutputMap, OutputMapError, OutputStrip, Routing, RoutingError,
};
pub use stream::{
    OpenedOutput, OutputQuantum, OutputSpec, OutputStream, QuantumOutOfRange, RunningStream,
    StartError, StreamDriver,
};
pub use timer::{DEFAULT_BLOCK, TimerDriver, TimerError, TimerRunning};
