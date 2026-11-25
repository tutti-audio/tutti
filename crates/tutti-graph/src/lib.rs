#![doc = include_str!("../README.md")]
//! The audio graph of the Tutti engine: a graph described as a value, a pure
//! compiler from that value to an immutable [`Plan`], and a real-time
//! [`Executor`] that runs the plan block by block, on one thread or spread over
//! a [`Pool`]'s threads.
//!
//! Use it directly to host audio processors ([`Node`]s) in a graph you build
//! and edit at run time, or to write a node that runs inside the engine.
//! `tutti-engine`'s engine renders an [`Executor`] behind its device callback,
//! and the `tutti` facade re-exports this crate as `tutti::graph`.
//!
//! # Quick start
//!
//! Implement [`Node`] for a processor, add it to a [`GraphBuilder`], wire it
//! and render:
//!
//! ```
//! # use tutti_buffers::Planes;
//! use tutti_buffers::MaxBlock;
//! use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
//! use tutti_units::{SampleRate, Samples};
//! use tutti_buffers::ChannelLayout;
//!
//! /// A one-pole lowpass.
//! #[derive(Clone)]
//! struct Lowpass {
//!     a: f32,
//!     y: f32,
//! }
//!
//! impl Node for Lowpass {
//!     fn shape(&self) -> Shape {
//!         Shape::through_unassigned(ChannelLayout::MONO)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         let (ins, mut outs) = io.split();
//!         for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
//!             self.y += self.a * (i - self.y);
//!             *o = self.y;
//!         }
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {
//!         self.y = 0.0;
//!     }
//! }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let lp = g.add(ForkByClone(Lowpass { a: 0.1, y: 0.0 }));
//! g.pipe_input(lp).pipe_output(lp);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(256))))
//!     .expect("builds");
//! let out = r.render_input(Planes::new(&[&[1.0; 1_000]]).unwrap());
//! assert!(out[0][999] > 0.9); // a lowpass passes DC
//! ```
//!
//! [`GraphBuilder`] is a convenience for fixed graphs (tests, examples,
//! simple hosts). A host that edits its graph while it plays uses the
//! [`Editor`]/[`Executor`] pair directly; see [`Editor`] for an example.
//!
//! # How it fits together
//!
//! ```text
//!  GraphSpec          Topology + event edges + param sources        plain value, Eq + Hash
//!      │ validate()
//!  ValidGraph         checked without the nodes' shapes
//!      │ compile(&ValidGraph, &Shapes, &Prepare, prev) -> (Plan, Delta)
//!  Plan               ops, buffer slots, delay and feedback tables   immutable, Send + Sync
//!      │ Editor::commit ──queue──▶ Executor, which sends the replaced state back
//!  Executor           owns the units; runs the plan's ops, one block per call
//! ```
//!
//! - [`GraphSpec`] is the graph as data: a
//!   [`Topology`] (nodes, audio edges, main inputs
//!   and outputs) plus event edges and each param port's event source.
//! - [`Editor`] (control thread) holds the spec and the units not yet
//!   shipped. [`Editor::insert`] adds a node and returns its typed controls;
//!   [`Editor::commit`] validates, [`compile()`]s against the plan sent last
//!   and sends the result — the new [`Plan`] and only the units that changed
//!   — over a preallocated queue.
//! - [`Executor`] (audio thread) installs each commit at the start of a
//!   block and sends everything it replaced back, so nothing is freed on the
//!   audio thread. With no commit pending, [`Executor::process`] does not
//!   allocate, lock or block.
//!
//! [`Editor::new`] is the only way to build an executor, and builds the two
//! as a pair from one [`Prepare`] (sample rate and maximum block), so the
//! block size every node sized its buffers for is the one it is handed.
//!
//! # Writing a node
//!
//! A [`Node`] has four methods: [`shape`](Node::shape) declares its ports,
//! latency and tail as a [`Shape`]; [`prepare`](Node::prepare) sizes its
//! buffers on the control thread; [`process`](Node::process) renders one
//! block from an [`Io`] on the audio thread and returns a [`Status`];
//! [`reset`](Node::reset) clears its state. A node is inserted through
//! [`IntoNode`], which also says whether it can be forked: wrap a plain node
//! in [`ForkByClone`] or [`Unforkable`], or implement `IntoNode` to hand out
//! typed controls. [`ParamNode`] and [`param_parts`] cover a node whose
//! controls are `Param<U>` cells addressed by [`ParamSet`]. `impl Node for X`
//! is the `f32` node; a node that does not care about the sample format is
//! `impl<F: Float> Node<F> for X`, with an [`Io<F>`](Io); an
//! [`Executor<F>`](Executor) runs the nodes of its format. Every example below is a
//! doctest: it compiles and runs.
//!
//! ## The shortest node
//!
//! A gain: one mono channel in and out, `Io::channel(c).map(..)` applying a
//! function to each sample whether the executor gave the node separate
//! buffers or one it works in place.
//!
//! ```
//! # use tutti_buffers::Planes;
//! use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
//! use tutti_units::{SampleRate, Samples};
//! use tutti_buffers::{ChannelLayout, MaxBlock};
//!
//! #[derive(Clone)]
//! struct Gain(f32);
//!
//! impl Node for Gain {
//!     fn shape(&self) -> Shape {
//!         Shape::through_unassigned(ChannelLayout::MONO)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         let g = self.0;
//!         io.channel(0).map(|x| x * g);
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let gain = g.add(ForkByClone(Gain(0.5)));
//! g.pipe_input(gain).pipe_output(gain);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))))
//!     .expect("builds");
//! assert_eq!(r.render_input(Planes::new(&[&[1.0; 8]]).unwrap())[0], [0.5; 8]);
//! ```
//!
//! `ForkByClone` says the clone shares nothing with the original, so an
//! export can fork the node; a node that cannot be copied is `Unforkable`.
//!
//! The same gain for any sample format: `impl<F: Float> Node<F>`, with a
//! literal crossing into `F` through `F::lit`.
//!
//! ```
//! # use tutti_buffers::{Float, Planes};
//! use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
//! use tutti_units::{SampleRate, Samples};
//! use tutti_buffers::{ChannelLayout, MaxBlock};
//!
//! #[derive(Clone)]
//! struct Half;
//!
//! impl<F: Float> Node<F> for Half {
//!     fn shape(&self) -> Shape {
//!         Shape::through_unassigned(ChannelLayout::MONO)
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
//!         io.channel(0).map(|x| x * F::lit(0.5));
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! let mut g = GraphBuilder::<f64>::with_format(ChannelLayout::MONO, ChannelLayout::MONO);
//! let half = g.add(ForkByClone(Half));
//! g.pipe_input(half).pipe_output(half);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))))
//!     .expect("builds");
//! assert_eq!(r.render_input(Planes::new(&[&[1.0f64; 8]]).unwrap())[0], [0.5; 8]);
//! ```
//!
//! ## Shape
//!
//! [`Shape`] is everything the compiler needs to know about a node before it
//! runs: its audio ports and buses ([`Shape::through_unassigned`],
//! [`Shape::source_unassigned`], [`Shape::buses`] for a node with a
//! sidechain or several buses), its event ports ([`events`](Shape::events)),
//! its [`Cost`] ([`cost`](Shape::cost): a cheap node says so, one that
//! says nothing is `Heavy`, worth a thread), its **processing** latency
//! ([`latency`](Shape::latency), which feeds delay compensation: a
//! musical delay is not latency) and its **tail**, the ceiling on how long it
//! keeps sounding after its input stops ([`tail`](Shape::tail)).
//! A node that changes width declares its buses; the graph never guesses a
//! dry path.
//!
//! A ducker has a main input and a mono sidechain, so its shape is two
//! buses in and one out. The sidechain is read through the handle the shape
//! issued, not by index:
//!
//! ```
//! use tutti_graph::{Bus, BusRole, Buses, Cost, Cx, InBus, Io, Node, Prepare, Shape, Status};
//! use tutti_units::{Latency, Samples, Tail};
//! use tutti_buffers::{ChannelTopology, ChannelLayout};
//!
//! struct Ducker {
//!     key: InBus,
//! }
//!
//! impl Ducker {
//!     fn ports() -> Shape {
//!         let mono = ChannelTopology::unassigned(ChannelLayout::MONO).unwrap();
//!         let ins = Buses::main(mono)
//!             .with(Bus::new(mono, BusRole::Sidechain))
//!             .unwrap();
//!         Shape::buses(ins, Buses::unassigned(ChannelLayout::MONO))
//!             .cost(Cost::Light)
//!             .latency(Latency::new(Samples::new(0)))
//!             .tail(Tail::None)
//!     }
//!     fn new() -> Self {
//!         Self { key: Self::ports().input_bus(1).expect("the sidechain is the second bus") }
//!     }
//! }
//!
//! impl Node for Ducker {
//!     fn shape(&self) -> Shape {
//!         Self::ports()
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         // The main input in place or not, the sidechain by its handle.
//!         let (inputs, mut outputs) = io.split();
//!         let key = inputs.bus(self.key);
//!         for (o, k) in outputs.get(0).iter_mut().zip(key.get(0)) {
//!             *o = (1.0 - k.abs().min(1.0)) * *o;
//!         }
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! let shape = Ducker::new().shape();
//! assert_eq!(shape.audio_in().count(), 2);
//! assert_eq!(shape.audio_out().count(), 1);
//! assert_eq!(shape.cost, Cost::Light);
//! ```
//!
//! ## A node with params
//!
//! A param is a `Param<U>` cell in the node, in a unit (never a bare float).
//! [`declare_params!`] states the node's params once, next to the cells: what a
//! host reaches by address, what the graph may modulate (the shape's ports)
//! and the unit each is in. `process` reads a driven param **through the
//! declaration** (`declared_param`), never by a port index, so reordering the
//! rows cannot swap which param a node reads. A lane on the port overrides the
//! cell for the block (`last_frame` for a block-rate control, `frames` for
//! per-sample); undriven, the node reads its own cell.
//!
//! The node is inserted as [`Params`]`(node)`, never `ForkByClone`: its fork
//! must start from the values set through its [`ParamSet`], not share the
//! live cell. (`ForkByClone(param_node)` still compiles, Rust having no way
//! to refuse the type; see [`Params`].) In a session it is
//! `graph.add(Params(node))`, which returns the key and gives it its typed param
//! methods from the declaration.
//!
//! ```
//! # use tutti_buffers::Planes;
//! use tutti_graph::{
//!     Cx, GraphBuilder, Io, Node, ParamNode, ParamSet, Params, Prepare, Shape, Status,
//!     declare_params,
//! };
//! use tutti_units::{Amplitude, Real, SampleRate, Samples, UnitParam};
//! use tutti_rt::Param;
//! use tutti_buffers::{ChannelLayout, MaxBlock};
//!
//! #[derive(Clone)]
//! struct Volume {
//!     level: Param<Amplitude>,
//! }
//!
//! declare_params!([] Volume, n {
//!     Volume => Amplitude = &n.level,
//! });
//!
//! impl Node for Volume {
//!     fn shape(&self) -> Shape {
//!         Shape::through_unassigned(ChannelLayout::MONO)
//!             .param_addrs(self.declared_ports().as_slice())
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         let level = self
//!             .declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
//!             .last_frame()
//!             .unwrap_or_else(|| f32::from_unit(self.level.load()));
//!         io.channel(0).map(|x| x * level);
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//!     fn param_base(&self, port: usize) -> Option<f64> {
//!         self.declared_base(port)
//!     }
//! }
//!
//! impl ParamNode for Volume {
//!     fn param_set(&self) -> ParamSet {
//!         self.declared_set()
//!     }
//!     fn fork_fresh(&self) -> Self {
//!         let mut fork = self.clone();
//!         fork.level.detach();
//!         fork
//!     }
//! }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let (volume, controls) = g.add_with_controls(Params(Volume { level: Param::new(Amplitude::new(1.0)) }));
//! g.pipe_input(volume).pipe_output(volume);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))))
//!     .expect("builds");
//! assert_eq!(r.render_input(Planes::new(&[&[1.0; 4]]).unwrap())[0], [1.0; 4]);
//! // A host sets it by address; the next block reads the cell.
//! assert!(controls.set(UnitParam::Volume, 0.25));
//! assert_eq!(r.render_input(Planes::new(&[&[1.0; 4]]).unwrap())[0], [0.25; 4]);
//! ```
//!
//! ## Events in and out
//!
//! Events travel on ports the shape declares. A node reads an input with
//! [`Io::sub_blocks`], which splits the block at the events' offsets so it is
//! sample-accurate by construction, and writes an output with
//! [`Io::event_out`]. Here a source emits a note-on at frame 3 and a gate
//! opens on it:
//!
//! ```
//! use tutti_graph::{
//!     Cx, Event, EventKind, ForkByClone, GraphBuilder, InEvents, Io, Node, Offset, OutEvents,
//!     Prepare, Shape, Status,
//! };
//! use tutti_units::{SampleRate, Samples};
//! use tutti_buffers::{ChannelLayout, MaxBlock};
//! use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Ump, Voice1};
//!
//! // A declared capacity is never zero: the type says so.
//! const CAPACITY: std::num::NonZeroU32 = std::num::NonZeroU32::new(4).unwrap();
//!
//! #[derive(Clone)]
//! struct Pulse;
//!
//! impl Pulse {
//!     const SHAPE: Shape = Shape::EMPTY.events(0, 1).event_capacity(CAPACITY);
//!     const OUT: OutEvents = match Self::SHAPE.event_output(0) {
//!         Some(port) => port,
//!         None => panic!("declared"),
//!     };
//! }
//!
//! impl Node for Pulse {
//!     fn shape(&self) -> Shape {
//!         Self::SHAPE
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         let on = Message::midi1(MidiChannel::of(0), Voice1::note_on(NoteNumber::of(60), U7::of(100)));
//!         if let Some(at) = io.offset(3) {
//!             let _ = io.event_out(Self::OUT).push(Event::midi(at, Ump::from(on)));
//!         }
//!         Status::Silent
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! #[derive(Clone)]
//! struct Gate {
//!     open: bool,
//! }
//!
//! impl Gate {
//!     const SHAPE: Shape = Shape::source_unassigned(ChannelLayout::MONO).events(1, 0);
//!     const IN: InEvents = match Self::SHAPE.event_input(0) {
//!         Some(port) => port,
//!         None => panic!("declared"),
//!     };
//! }
//!
//! impl Node for Gate {
//!     fn shape(&self) -> Shape {
//!         Self::SHAPE
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         for (range, events) in io.sub_blocks(Self::IN) {
//!             for e in events {
//!                 if let EventKind::Midi(_) = e.kind {
//!                     self.open = true;
//!                 }
//!             }
//!             io.output(0)[range].fill(f32::from(u8::from(self.open)));
//!         }
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {
//!         self.open = false;
//!     }
//! }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let pulse = g.add(ForkByClone(Pulse));
//! let gate = g.add(ForkByClone(Gate { open: false }));
//! g.connect_events(pulse, 0, gate, 0).pipe_output(gate);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))))
//!     .expect("builds");
//! assert_eq!(r.render(6)[0], [0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
//! # let _ = Offset::ZERO;
//! ```
//!
//! ## Status and tail
//!
//! `process` returns a [`Status`] that tells the executor what it may skip:
//! `Modified` (outputs written, no claim), `Masked` (some channels silent or
//! constant, a claim the node must back), `Silent` (wrote nothing, output is
//! silence this block), `Idle` (silent with no pending activity: park me until
//! an input arrives), `Constant`, and `Bypass` (the output is the input, the
//! executor copies).
//!
//! [`Shape::tail`] is the node's ceiling. A node whose ring-out follows its
//! params narrows it each block with [`Io::report_tail`]: the report is
//! measured from the last non-quiet input, must cover the ring-out and err
//! long, and only ever moves the silence skip (the plan, delay compensation
//! and export read the ceiling).
//!
//! ```
//! # use tutti_buffers::Planes;
//! use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
//! use tutti_units::{SampleRate, Samples, Tail};
//! use tutti_buffers::{ChannelLayout, MaxBlock};
//!
//! /// A feedback echo whose ring-out is `taps` repeats of `period` frames.
//! #[derive(Clone)]
//! struct Echo {
//!     taps: usize,
//!     period: usize,
//!     on: bool,
//! }
//!
//! impl Node for Echo {
//!     fn shape(&self) -> Shape {
//!         // The ceiling: never longer than this, whatever the params.
//!         Shape::through_unassigned(ChannelLayout::MONO)
//!             .tail(Tail::Finite(Samples::new(8 * 4_800)))
//!     }
//!     fn prepare(&mut self, _: &Prepare) {}
//!     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
//!         if !self.on {
//!             // Switched off: the output is the input.
//!             return Status::Bypass;
//!         }
//!         // The ring-out follows the params, so say it; err long.
//!         io.report_tail(Tail::Finite(Samples::new(self.taps * self.period)));
//!         io.channel(0).map(|x| x);
//!         Status::Modified
//!     }
//!     fn reset(&mut self) {}
//! }
//!
//! let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
//! let echo = g.add(ForkByClone(Echo { taps: 3, period: 4_800, on: false }));
//! g.pipe_input(echo).pipe_output(echo);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))))
//!     .expect("builds");
//! assert_eq!(r.render_input(Planes::new(&[&[0.5; 4]]).unwrap())[0], [0.5; 4]);
//! ```
//!
//! ## What not to do
//!
//! - **Share state outside ports.** Nodes communicate only through ports: two
//!   nodes sharing an `Arc<Mutex<_>>` or a cursor are unordered when a block's
//!   tasks run on a worker pool, and the render stops being bit-identical at
//!   every participant count. Share through an edge, or make it one node.
//! - **Allocate, lock or block in `process`.** It runs on the audio thread:
//!   size every buffer in `prepare` (against [`Prepare::max_block`]), and
//!   publish non-scalar state to it through `tutti_rt`'s `Publisher`, never by
//!   swapping an `Arc` the audio thread then drops.
//! - **Report a musical delay as latency.** Latency is processing delay only.
//! - **Read a driven param by port index.** Use the declaration
//!   (`declared_param`), above.
//!
//! Several mistakes are ruled out by types:
//!
//! - **Latency is declared, never a delay time.** [`Shape::latency`] is a
//!   [`Latency`](tutti_units::Latency), which a musical delay in `Samples` or
//!   `Seconds` cannot be passed as, so the graph's delay compensation (PDC)
//!   never drags parallel paths behind an echo.
//! - **Blocks never exceed the [`MaxBlock`](tutti_buffers::MaxBlock) a node was
//!   prepared with** ([`Prepare::max_block`]); every [`Io`] is at most that long,
//!   so a node that sizes its scratch from it never clamps.
//! - **Block offsets are not frames.** An event carries an [`Offset`] inside
//!   its block, a different type from an absolute
//!   [`Frame`](tutti_units::Frame); the two convert only through the block's
//!   [`Env`] ([`Env::offset_of`], [`Env::frame_at`]).
//! - **Events arrive sorted and in range** ([`SortedEvents`]), and
//!   [`Io::sub_blocks`] splits the block at their offsets, so a node written
//!   against it is sample-accurate by construction.
//! - **Params are typed.** A [`ParamRamp`] or [`ParamSegment`] is built
//!   from a typed `ParamKey<U>` and read back as a `U`.
//!
//! # Whole blocks
//!
//! The executor hands every node the **whole block**. It never splits one,
//! not at event offsets (sub-chunking at an event is the node's job), not at
//! a transport loop wrap, and not at a transport change inside the block.
//! That keeps an out-of-process plugin's declared pipeline latency constant.
//! A loop wrap is visible through [`TransportPosition::looping`] and the block-start
//! beat in [`Env`]; a start, stop or seek landing inside a block is carried
//! by [`Env::changes`], and [`Env::transport_at`] answers "what was the
//! transport at this frame". A node behind a latent path reads
//! [`Cx::transport_at`] instead: the transport its *input* was rendered at,
//! [`Cx::arrival`] frames earlier, reaching into earlier blocks through the
//! executor's [`TransportHistory`].
//!
//! # Events and timing
//!
//! Events ([`Event`], [`EventKind`]: MIDI 2.0 [`Ump`](tutti_ump::Ump) packets,
//! [`Harmony`]) and param events ([`ParamEvent`]: [`ParamRamp`] and
//! [`ParamSegment`] automation) travel on graph ports ([`EventInputPort`],
//! [`EventOutputPort`]), so the same delay compensation that aligns audio aligns
//! them. A param event carries the unit's `f64` quantity and is larger than
//! the 32 bytes an [`Event`] is held to, so it travels in a buffer of its own
//! beside the port's events, merged and delayed with them
//! ([`SortedEvents::params`], [`EventWriter::push_param`]); a node that
//! writes them declares it ([`Shape::param_events`]), and only the
//! buffers its outputs reach have room for them. An event input may have several sources, merged by offset and then
//! by the source port's `(NodeKey, port)` — a property of the wiring, not of
//! the order edges were listed in. An audio input port, and a
//! main output channel, holds a set of sources, which the graph sums
//! (`Op::Sum`).
//!
//! - Each event output port declares how many events it writes per block
//!   ([`Shape::event_capacity`]), and every buffer downstream is sized from
//!   that at compile time. A writer past it is refused the newest event,
//!   except a note-off, which has room past the capacity
//!   ([`note_off_room`]); the executor counts each refusal
//!   ([`Executor::dropped_event_count`]), and per port
//!   ([`Editor::take_event_losses`]). Nothing downstream of a writer drops
//!   one, unless the receiving node bounds its inputs. A source whose rate is
//!   per second sizes its declaration from the block it is prepared for
//!   ([`EventDensity`]).
//! - A node that sizes a buffer for what it is sent declares the most events
//!   per block its event inputs accept together
//!   ([`Shape::event_input_capacity`]; `None` is unbounded, each input sized
//!   from its edges). Past it the node is sent the earliest events by frame
//!   (note-offs first) of what its edges, flushed events and scheduled
//!   commands bring, and the executor counts the rest as it counts a refused
//!   write ([`Executor::dropped_event_count`]). The bound holds however the
//!   edges change at a commit.
//! - [`Editor::schedule`] delivers an event, or a param ramp or segment to a
//!   node that declares [`Shape::param_events`], on an exact frame or beat
//!   ([`At`](tutti_units::At)). A late command lands at the start of the next
//!   block and is counted ([`Executor::late_command_count`]), never dropped.
//! - Each node declares how finely it honours event offsets
//!   ([`Shape::event_resolution`], a [`Resolution`]); an event edge marked
//!   with [`GraphSpec::require_resolution`] into a node that cannot honour it
//!   is [`CompileError::ResolutionTooCoarse`].
//!
//! # Editing a running graph
//!
//! - [`Editor::replace`] swaps a node's unit with a crossfade ([`Fade`],
//!   along a [`FadeLaw`]); the outgoing unit retires on the control
//!   thread.
//! - [`Editor::set_latency`] changes a node's declared latency (a hosted
//!   plugin's pipeline changing).
//! - [`Editor::reprepare`] changes the sample rate or maximum block,
//!   re-preparing every unit on the control thread.
//! - [`GraphSpec::connect_param`] drives one of a node's declared params
//!   ([`Shape::params`]) from another node's event output (a control
//!   lane's value and ramp segments); the node reads the result through
//!   [`Io::param`], and an unconnected param reads its own control, never 0.
//! - [`Editor::fork`] builds an independent copy of the graph, or of the part
//!   feeding one node, for an offline export or a live duplicate.
//! - A feedback loop is an explicit edge with its own delay
//!   (`crate::FeedbackFrom`), so an offline bounce at a larger
//!   block loops like playback; a delay shorter than the maximum block is
//!   [`CompileError::FeedbackTooShort`].
//!
//! # Precision
//!
//! A quantity has one width, and data is generic over the graph's float `F`.
//! Every buffer between nodes is planar, in the sample format of the
//! [`Executor<F>`](Executor) that runs them, and an executor sums fan-ins in
//! that format. [`Editor::new`] and [`GraphBuilder::new`] build the `f32`
//! graph; [`Editor::with_format`] and [`GraphBuilder::with_format`] build one
//! in another format.
//!
//! A node is `Node<F>` for any `F`: `impl<F: Float> Node<F>`. It reads a
//! param as a `&[F]` lane ([`ParamInput::frames`]) or as a `Param<U>` cell,
//! which holds the unit's own width and crosses
//! into the node's data with `F::from_unit(cell.load())` once per block. Its
//! state is `F`: there is no `f64` state inside an `f32` node. A cell, a
//! [`Node::param_base`] and a [`ParamSet`] value are all the unit's width, so
//! a param written through a [`ParamSet`] reaches the node at the width it was
//! written in; the crossing to `F` is the node's. An `f32` graph and an `f64`
//! graph never mix.
//!
//! A control value (a param's base, an edge weight, a mix, a fade's gain
//! law) is a quantity; the graph renders it into a per-sample lane in `F`
//! once per block or segment ([`ramp_in`], [`fade_gains`],
//! [`fade_in`]), so a param lane ([`ParamInput::frames`]), an edge weight's
//! ramp, a mix and a crossfade all run in the graph's float. A param event's
//! ramp or segment is rendered in `f64`, from the unit's quantity and `u32`
//! frame counts, and crosses to `F` once per frame ([`control_in`]): a
//! segment ten minutes long advances one frame per frame at its end in an
//! `f32` graph as in an `f64` one, and a circular param (`Azimuth`) turns the
//! short arc before the crossing. A meter
//! accumulates a block's figures in `F` over chunks of at most 256 frames, each
//! folded into `f64`, and reads them as `f64`. Quantities are `f64` too: time
//! ([`Env::frame`] is a `u64` [`Frame`](tutti_units::Frame) and the transport
//! position a `f64` [`Beat`](tutti_units::Beat)), and a param's value, as far
//! as its unit goes; a plugin that processes in another width converts at its
//! boundary. [`Io`] exposes buffers through methods rather than fields.
//!
//! # Testing
//!
//! - [`Plan::verify`] and [`Plan::verify_fades`] check a [`Plan`] against its
//!   op DAG; [`compile()`] runs `verify` in every debug build.
//! - [`GraphBuilder::renderer`] returns a [`Renderer`] that drives an
//!   executor and hands back planar or interleaved output.
//!
//! # Feature flags
//!
//! - `contract` (off by default): the `contract` module, a
//!   harness that checks a node responds on exactly the frame its declared
//!   latency and resolution promise, on every path the graph can put it on.
//!   Test support: enable it from a dev-dependency.
//! - `testing` (off by default): the `testing` module: the small test nodes
//!   (`TestNode`, `Kind`), the harness that drives an executor block by block
//!   (`Harness`) and a worker pool for tests (`TestPool`).
//!   Test support: this crate's own tests turn it on through a self
//!   dev-dependency, and a node crate's tests may do the same.
//!
//! # Import paths
//!
//! Every public item is re-exported at the crate root; import from there
//! (`tutti_graph::Plan`). The only public modules are `contract` and `testing`, both test support.

#![deny(missing_docs)]
// No `unsafe` anywhere in the crate: the arena's disjoint borrows are proven
// by `split_at_mut`, not by a comment. See `arena.rs`.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

mod compile;
#[cfg(feature = "contract")]
pub mod contract;
mod edges;
mod edit;
mod event;
mod exec;
mod model;
mod node;
#[cfg(feature = "testing")]
pub mod testing;

pub use compile::{CompileError, CycleEdge, Shapes, VerifyError, compile};
pub use edges::event_inlet::{
    EventCapacity, EventCapacityError, EventInletNode, EventInletWriter, InletCounts, Stamp,
};
pub use edges::event_tap::{EventTapOptions, EventTapReader, EventTapWriter, TapEvent};
pub use edit::builder::{GraphBuilder, Renderer};
pub use edit::editor::{
    CloseError, CommitError, Editor, Inserted, Limits, MainOutputGate, MissingSource, Rewired,
    Unwired, WireError,
};
pub use event::sorted::{EventOrderError, SortedEvents, SubBlocks};
pub use event::types::{Event, EventKind, Harmony, HarmonyKind, UmpAt};
pub use event::writer::{
    EventDensity, EventRejected, EventWriter, MAX_EVENT_PORT_CAPACITY, MIN_EVENT_PORT_CAPACITY,
    note_off_room,
};
pub use exec::command::{CANCEL_CAPACITY, COMMAND_CAPACITY, CommandId, ScheduleError, Scheduled};
pub use exec::lane::{control_in, fade_gains, fade_in, ramp_in};
pub use exec::{DEFAULT_EVENT_CAPACITY, Executor, FADE_CAPACITY, MainOutputFade, QUEUE_CAPACITY};
pub use model::fade::Fade;
pub use node::controls::{
    DeclaredUnit, ParamFork, ParamNode, ParamSet, ParamSetBuilder, Params, param_parts,
};
pub use node::cost::Cost;
pub use node::declare::{HasParam, HasParams, ParamValue, Scalar};
pub use node::fork::{
    ForkBuild, ForkByClone, ForkCause, ForkError, ForkFault, ForkFaultKind, ForkHealth,
    ForkRemains, ForkSource, ForkTarget, Forked, PendingFork, Unforkable,
};
pub use node::setting::{ErasedSetting, HasSetting, SettingCell};
pub use tutti_curve::{Curvature, DECLICK, Exponent, FadeLaw, Law, declick_frames};
// The audio graph as a value, and the two passes over it. `plan` is the
// latency pass and is exported as `latency_plan`.
pub use compile::ids::{
    AudioDelayId, AudioFeedbackId, AudioSlotId, EventDelayId, EventFeedbackId, EventSlotId, OpId,
    PlanEventTapId, PlanMeterId, PlanTapId, PlanUnitId, TaskId,
};
pub use compile::latency::{
    ChannelFeed, Compensation, Delays, Feed, InputDelay, LatencyGraph, MAX_NODE_LATENCY,
    OutputDelay, PortFeed, delays, plan as latency_plan,
};
pub use compile::main_output::{
    MainOutputBank, MainOutputError, MainOutputId, fold_to_channels, fold_to_topology,
    fold_to_topology_with, fold_to_width,
};
pub use compile::plan::{
    AudioDelay, AudioDelayKey, AudioFeedbackKey, Csr, Delta, EventDelay, EventDelayKey,
    EventFeedbackKey, EventSlotCapacity, FeedbackSpec, GainPortOp, InputBound, Op, ParamPortOp,
    Placement, Plan, PlanUnit, Span, UnitIdx, Value,
};
pub use compile::preroll::PrerollTable;
pub use compile::tail::{GraphTail, MAX_NODE_TAIL, TailGraph, graph_tail};
pub use edges::inlet::{InletNode, InletRing};
pub use edges::meter::{Level, MeterReader, MeterWriter};
pub use edges::rtlog::{
    LogFilter, MAX_FIELDS, RtLevel, RtLogReaders, RtRecord, RtTarget, RtValue, TEXT_CAPACITY,
};
pub use edges::tap::{TapOptions, TapReader, TapWriter};
pub use exec::history::TransportHistory;
pub use exec::loss::EventLoss;
pub use exec::par::Pool;
pub use exec::pool::{WorkerPool, WorkerPoolBuilder};
pub use exec::published::{ErasedPublished, HasPublished, Published, PublishedCell, PublishedKind};
pub use model::bus_edge::{BusError, BusFeed, BusFold, BusOutputPort, BusSide, Lowering, NodeBus};
pub use model::gain::{EdgeFade, EdgeGain, EdgeGainError};
pub use model::graph::{
    Edge, EdgeId, EventTapId, EventTapPort, EventTapSpec, FeedbackFrom, InputPort, Invalid,
    MeterId, MeterSpec, NodeKey, NodeSpec, OutputPort, PortSink, PortSource, TapAlign, TapId,
    TapSpec, Topology, Valid,
};
pub use model::mode::{MixError, ModeError, NodeMix, NodeMode};
pub use model::pass::{Pass, PassError};
pub use model::spec::{
    EventEdge, EventInputPort, EventOutputPort, GraphInvalid, GraphSpec, ValidGraph,
};
pub use node::bus::{Bus, BusRole, Buses, InBus, InEvents, MAX_BUSES, OutBus, OutEvents};
pub use node::instrument::InstrumentNode;
pub use node::io::{BusInput, BusOutput, Channel, Inputs, Io, Outputs, PortKind};
pub use node::param::{
    EffectiveCell, MAX_PARAM_PORTS, ParamIn, ParamInput, ParamPorts, ParamPortsError, ParamRange,
    ParamRangeCell, ParamRangeError, ParamRangeReader, RangeView,
};
pub use node::param_event::{ParamEvent, ParamKind, ParamRamp, ParamSegment};
pub use node::play::PlayClock;
pub use node::rates::SampleRates;
pub use node::render_mode::RenderMode;
pub use node::shape::Shape;
pub use node::time::{
    BeatCrossings, Continuity, Due, Env, MAX_TRANSPORT_CHANGES, Offset, TransportChange,
    TransportChangeRejected, TransportChanges, TransportPosition,
};
pub use node::{
    Chase, ConstantMask, Cx, InPlaceMask, IntoNode, MAX_PORTS, Node, NodeParts, Prepare,
    Resolution, Scratch, SilenceMask, Status, Wired, WiredMask,
};
pub use tutti_rt::BlockWaits;
// The reading is a value type, `tutti-units`' (the modulation compiler
// carries one without naming the graph); the clock that keeps it is here.
pub use node::wake::Wake;
pub use tutti_units::PlayReading;

/// What [`declare_params!`] expands to, and nothing a caller names.
#[doc(hidden)]
pub mod __private {
    pub use crate::node::declare::{
        Cells, OwnedCell, ParamEntry, Row, Visit, base, input, ports, row, set,
    };
    pub use std::sync::Arc;
    pub use tutti_buffers::Float;
    pub use tutti_rt::{AtomicCell, AtomicWord, Param};
    pub use tutti_units::{UnitAddr, UnitParam};
}

#[cfg(test)]
mod error_bounds {
    //! Each error type this crate returns is an `Error` a caller can box and move (`?` into
    //! `Box<dyn Error + Send + Sync>`); the bound is checked where the function
    //! is written, so a non-`Send` field fails the build.

    fn err<T: std::error::Error + Send + Sync + 'static>() {}

    #[allow(dead_code)]
    fn the_errors_are_boxable() {
        err::<crate::CompileError>();
        err::<crate::EventCapacityError>();
        err::<crate::EventOrderError>();
        err::<crate::ForkError>();
        err::<crate::ForkFault>();
        err::<crate::ParamPortsError>();
        err::<crate::ParamRangeError>();
        err::<crate::EdgeGainError>();
        err::<crate::TransportChangeRejected>();
        err::<crate::VerifyError>();
    }
}
