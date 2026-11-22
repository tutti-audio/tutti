//! The runtime's control side: [`Editor`] holds the graph value, prepares
//! units, compiles, and sends commits to its paired [`Executor`] — and drains
//! the boxes it sends back.
//!
//! # Commits are sent, not handed out
//!
//! [`commit`](crate::Editor::commit) compiles and, on success, **sends the box
//! itself** to the paired executor over a preallocated queue. The caller
//! never holds a box, so one cannot be dropped unapplied, applied twice or
//! reordered: every commit is compiled against the plan sent just before it,
//! the executor applies them in that order, and the editor's idea of the
//! running plan can never run ahead of what the executor will install.
//!
//! # Back-pressure
//!
//! The audio thread sends every box back after applying it, and
//! that push must never fail — a failed push on the audio thread means either
//! freeing there or blocking. At most [`QUEUE_CAPACITY`](crate::QUEUE_CAPACITY) commits may be out
//! (sent, and not yet drained back by [`collect`](crate::Editor::collect)); with that
//! many out, `commit` returns [`CommitError::Backpressure`] **before**
//! compiling or advancing its plan. The return ring holds one more than that,
//! so the executor's push always has room. `commit` drains the return ring
//! first, so a caller that only ever commits never has to call `collect`.
//!
//! A crossfade ([`replace`](crate::Editor::replace)) does **not** hold its commit:
//! the box comes back as soon as it is applied. The fade's outgoing unit
//! comes back on its own, on the fade-return ring, when the fade ends (or
//! is cut), and `collect` drains that ring too. Each fade a commit starts
//! takes one of [`FADE_CAPACITY`](crate::FADE_CAPACITY) slots on it, reserved before the commit
//! is sent; a commit that would need more is `Backpressure` too — a cap no
//! session reaches in practice.
//!
//! # One `Prepare`
//!
//! [`Editor::new`] builds the editor *and* its [`Executor`], from one
//! [`Prepare`]. Units are prepared by the editor and run by the executor, so
//! the `MaxBlock` a node sized its scratch from is the one every block it is
//! handed obeys; the plan carries its `Prepare` too, and applying refuses a
//! plan prepared for anything else.
//!
//! # Changing the rate or the maximum block
//!
//! [`reprepare`](crate::Editor::reprepare): a full recompile with every unit
//! re-prepared on the control thread, in two commits — the first checks the
//! units out of the executor (which adopts the new `Prepare`), the second
//! sends them back re-prepared, with a plan compiled against their new
//! shapes. Its doc states what happens to delay and feedback state. An
//! offline render must be prepared with a `MaxBlock` no larger than the
//! graph's shortest feedback delay (see `crate::FeedbackFrom`);
//! `compile` enforces it, and `reprepare` refuses such a change before
//! sending anything.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::SampleRates;
use tutti_units::{At, SampleRate};

use crate::EdgeId;
use crate::Fade;
use crate::bus_edge::{BusError, BusFold, BusOutputPort, Lowering, NodeBus};
use crate::command::{CommandId, ScheduleError, Scheduled, command_channel};
use crate::compile::{CompileError, Shapes};
use crate::event_tap::EventTapWriter;
use crate::exec::{DEFAULT_EVENT_CAPACITY, Executor, Stager, channels};
use crate::fork::{ForkHealth, ForkSource};
use crate::gain::EdgeFade;
use crate::meter::MeterWriter;
use crate::node::Prepare;
use crate::plan::{Delta, Plan};
use crate::spec::{EventInputPort, GraphSpec, ValidGraph};
use crate::tap::TapWriter;
use crate::values::GraphValues;
use crate::{EventTapId, MeterId, NodeKey, TapId};
use tutti_buffers::Float;

mod cells;
mod commit;
mod controls;
mod errors;
mod lifecycle;
mod link;
mod nodes;
mod outlets;
mod placements;
mod taps;
mod unwired;
use cells::LiveCells;
pub use commit::Rewired;
pub use errors::{CloseError, CommitError, MissingSource};
use lifecycle::Lifecycle;
use link::Link;
use outlets::Outlets;
use placements::Placements;
use tutti_buffers::MAX_CHANNELS;
use tutti_units::Gain;
pub use unwired::{Inserted, Unwired};

/// The host's side of a main output generation: whether it has published the folds
/// for `main_output` at `outputs` channels ([`Editor::set_main_output_gate`]).
pub trait MainOutputGate: Send + Sync {
    /// Whether a plan of generation `main_output` with `outputs` main outputs has
    /// its folds. A graph with no outputs has nothing to fold.
    fn published(&self, main_output: crate::MainOutputId, outputs: usize) -> bool;
}

/// What the host that runs an [`Executor`] can take, enforced by its
/// [`Editor`] on every later commit and re-prepare, so a graph the host
/// cannot run is refused on the control thread and never reaches it.
///
/// The executor itself takes any graph; a host has buffers of its own. The
/// engine (`tutti_engine::Engine::new`) sets these to its fold
/// scratch's width and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Most main outputs a committed graph may have.
    pub max_main_outputs: usize,
    /// Largest `MaxBlock` a re-prepare may ask for, in frames.
    pub max_block: usize,
}

impl Limits {
    /// No limit: what an editor starts with.
    pub const NONE: Limits = Limits {
        max_main_outputs: usize::MAX,
        max_block: usize::MAX,
    };

    fn outputs(&self, outputs: usize) -> Result<(), CommitError> {
        if outputs > self.max_main_outputs {
            return Err(CommitError::TooManyOutputs {
                outputs,
                limit: self.max_main_outputs,
            });
        }
        Ok(())
    }

    fn block(&self, max_block: usize) -> Result<(), CommitError> {
        if max_block > self.max_block {
            return Err(CommitError::BlockTooLong {
                max_block,
                limit: self.max_block,
            });
        }
        Ok(())
    }
}

/// The control-thread half of a graph: holds the [`GraphSpec`], prepares
/// units, compiles, and sends each result to its paired [`Executor`].
///
/// [`Editor::new`] builds the editor and its executor together, from one
/// [`Prepare`]. Nodes are added with [`insert`](Self::insert) (which
/// prepares the unit and returns its typed controls), wired through
/// [`spec_mut`](Self::spec_mut), and shipped with [`commit`](Self::commit),
/// which validates and compiles the spec against the plan sent last and
/// sends a commit — the new [`Plan`] and only the units that changed — over a
/// preallocated queue. The executor installs it at the start of its next
/// block and sends back everything it replaced, which the editor frees on
/// the control thread in [`collect`](Self::collect) (`commit` collects
/// first, so a caller that only commits never needs to).
///
/// The caller never holds a commit, so one cannot be dropped unapplied,
/// applied twice or reordered. At most [`QUEUE_CAPACITY`](crate::QUEUE_CAPACITY) commits may be in
/// flight; past that `commit` returns [`CommitError::Backpressure`] before
/// compiling anything.
///
/// Every method runs on the control thread and may allocate. Other edits:
/// [`replace`](Self::replace) swaps a unit with a crossfade,
/// [`set_latency`](Self::set_latency) changes a node's declared latency,
/// [`reprepare`](Self::reprepare) changes the sample rate or maximum block,
/// [`schedule`](Self::schedule) delivers an event on an exact frame or beat,
/// and [`fork`](Self::fork) copies the graph for an offline render.
///
/// # Examples
///
/// ```
/// use tutti_buffers::MaxBlock;
/// use tutti_graph::{Cx, Editor, ForkByClone, Io, Node, Prepare, Shape, Status, TransportPosition};
/// use tutti_graph::{Edge, InputPort, OutputPort, PortSource};
/// use tutti_units::{Beat, Bpm, SampleRate, Samples};
/// use tutti_buffers::ChannelLayout;
/// use tutti_graph::NodeKey;
///
/// #[derive(Clone)]
/// struct Half;
/// impl Node for Half {
///     fn shape(&self) -> Shape {
///         Shape::through_unassigned(ChannelLayout::MONO)
///     }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
///         let (ins, mut outs) = io.split();
///         for (o, i) in outs.get(0).iter_mut().zip(ins.get(0)) {
///             *o = 0.5 * i;
///         }
///         Status::Modified
///     }
///     fn reset(&mut self) {}
/// }
///
/// let (mut editor, mut executor) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))));
/// let half = NodeKey(1);
/// editor.insert(half, "half", ForkByClone(Half));
/// let topology = &mut editor.spec_mut().topology;
/// topology.inputs = ChannelLayout::MONO;
/// topology.connect(InputPort::new(half, 0), Edge::Direct(PortSource::MainInput(0)));
/// topology.set_outputs([PortSource::Node(OutputPort::new(half, 0))]);
/// editor.commit().expect("compiles");
///
/// // On the audio thread:
/// let input = [1.0_f32; 64];
/// let mut output = [0.0_f32; 64];
/// let transport = TransportPosition::new(false, Bpm::of(120.0), Beat::new(0.0), None);
/// executor.process(64, &transport, &[&input], &mut [&mut output]);
/// assert_eq!(output[0], 0.5);
/// ```
pub struct Editor<F: Float = f32> {
    /// The rings to the paired executor.
    link: Link<F>,
    prepare: Prepare,
    spec: GraphSpec,
    shapes: Shapes,
    /// Each key's record: the unit waiting for the next commit, its wake
    /// and fork source, the crossfade or latency cut the commit carries,
    /// whether it is leaving, and its generation count.
    placements: Placements<F>,
    /// The open taps: each reader's width and end latch.
    taps: Outlets<TapId, TapWriter<F>, TapOpened>,
    /// The open event taps.
    event_taps: Outlets<EventTapId, EventTapWriter, ()>,
    /// The open meters: each one's lane capacity.
    meters: Outlets<MeterId, MeterWriter, usize>,
    /// Where the compile lifecycle stands: the plan sent last (what the
    /// executor will be running once the queue drains, and what the next
    /// commit is compiled against), a re-prepare between its two commits,
    /// or the cause a re-prepare left the editor poisoned by.
    life: Lifecycle<F>,
    /// What the executor's host can take.
    limits: Limits,
    /// The host that folds the main output into a device, once one is attached: a
    /// commit whose main output it has no folds for is refused.
    main_output_gate: Option<std::sync::Arc<dyn MainOutputGate>>,
    /// Events per event slot per block, as the executor was built with: a
    /// fork gets the same.
    event_capacity: usize,
    /// On a **fork** only: the health probe each forked unit's source handed
    /// over, by key (see `src/fork.rs`, "When a forked unit fails").
    fork_health: Vec<(NodeKey, Arc<dyn ForkHealth>)>,
    /// The cells shared with running units: param ranges, lane gains,
    /// modes. Written from the spec by every commit, and by the controls
    /// ([`set_param_range`](Self::set_param_range),
    /// [`set_edge_gain`](Self::set_edge_gain), [`set_mode`](Self::set_mode)
    /// ...) at once.
    cells: LiveCells,
    /// Each node's mode and mix, each port's range, and each gain lane's base
    /// and range: what a unit placed by a commit starts in, and what a fork
    /// starts every unit in. Never in the spec, whose commits they do not
    /// move.
    values: GraphValues,
    /// [`revision`](Self::revision): moved by every call that can change
    /// what a reader of the editor sees.
    revision: u64,
    /// What the executor will hold once the commits sent are applied: the
    /// bounds each commit's state is built from.
    stager: Stager,
    /// How a commit that changes the main output's generation trades the old main output
    /// for the new ([`set_main_output_fade`](Self::set_main_output_fade)); `None` cuts.
    main_output_fade: Option<Fade>,
    /// The commit sent last that carries an old main output's channels, until a
    /// commit without them follows.
    bank: Option<SentBank>,
    /// The old main output a crossfading plan renders, kept for the commits that
    /// continue the crossfade.
    fading: Option<Fading>,
    /// Where each tap this editor opens takes its bell
    /// ([`notify_with`](Self::notify_with)); `None` opens taps without one.
    bells: Option<tutti_rt::BellsHandle>,
}

/// The old main output of a crossfade in progress, as the commit that began it
/// found it: what each commit while the crossfade runs compiles the bank from.
/// It holds a clone of the committed spec and shapes until the crossfade ends.
#[derive(Clone)]
struct Fading {
    /// The old main output's spec and shapes.
    was: lifecycle::Committed,
    /// The base of each gain lane of `was`'s spec when the crossfade began.
    bases: BTreeMap<EdgeId, Gain>,
    /// The fade of each gain lane of `was`'s spec that ran one when the
    /// crossfade began.
    fades: BTreeMap<EdgeId, EdgeFade>,
    /// The main output generation that fades in.
    main_output: crate::MainOutputId,
    /// The bank the plans carry.
    bank: crate::MainOutputBank,
}

/// A commit sent with an old main output's channels beside the main output's own.
#[derive(Clone, Copy, Debug)]
struct SentBank {
    /// Its `seq`, or that of the commit that began the crossfade it
    /// continues.
    seq: u64,
    /// The main output that fades out.
    old: crate::MainOutputId,
    /// The main output that fades in.
    main_output: crate::MainOutputId,
    /// Whether the executor has rendered its crossfade through.
    ended: bool,
}

impl<F: Float> std::fmt::Debug for Editor<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Editor")
            .field("link", &self.link)
            .field("prepare", &self.prepare)
            .field("placements", &self.placements)
            .field("planned", &self.life.plan().is_some())
            .field("committed", &self.life.committed().is_some())
            .field("poisoned", &self.poison_cause())
            .field("limits", &self.limits)
            .field("event_capacity", &self.event_capacity)
            .field("param_ranges", &self.cells.ranges.len())
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

/// What an open tap's reader was opened with.
struct TapOpened {
    /// Its lanes: the writer the executor holds has as many rings.
    width: usize,
    /// Raised when every node it read was removed.
    end: Arc<crate::tap::TapEnd>,
}

/// A panic payload as text.
fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string panic".to_string())
}

impl Editor {
    /// An empty `f32` graph, and the executor that will run it, both for
    /// `prepare`. [`with_format`](Editor::with_format) builds the pair for
    /// another sample format.
    #[must_use]
    pub fn new(prepare: Prepare) -> (Self, Executor) {
        Self::with_format(prepare)
    }
}

impl<F: Float> Editor<F> {
    /// An empty graph in sample format `F`, and the executor that will run
    /// it, both for `prepare`, with the default event capacity
    /// ([`with_event_capacity`](Self::with_event_capacity)).
    #[must_use]
    pub fn with_format(prepare: Prepare) -> (Self, Executor<F>) {
        Self::with_event_capacity(prepare, DEFAULT_EVENT_CAPACITY)
    }

    /// As [`with_format`](Self::with_format), with `cap` events per block as
    /// the default capacity of an event output port whose node declares none
    /// ([`Shape::event_capacity`](crate::Shape::event_capacity)) — its
    /// writer's limit, and the rate the delay FIFOs fed by it are sized
    /// from. A declared capacity overrides it for that node's ports. The only
    /// way to build an [`Executor`].
    #[must_use]
    pub fn with_event_capacity(prepare: Prepare, cap: usize) -> (Self, Executor<F>) {
        let (channels, ends) = channels();
        let (commands, command_rx) = command_channel();
        let stager = Stager::new(
            cap,
            Arc::clone(&channels.pool),
            Arc::clone(&channels.rendered),
        );
        let editor = Self {
            link: Link::new(channels, commands),
            prepare,
            spec: GraphSpec::default(),
            shapes: Shapes::new(),
            placements: Placements::new(),
            taps: Outlets::new(),
            event_taps: Outlets::new(),
            meters: Outlets::new(),
            life: Lifecycle::Unplanned,
            limits: Limits::NONE,
            main_output_gate: None,
            event_capacity: cap,
            fork_health: Vec::new(),
            cells: LiveCells::new(),
            values: GraphValues::default(),
            revision: 0,
            stager,
            main_output_fade: None,
            bank: None,
            fading: None,
            bells: None,
        };
        (editor, Executor::new(prepare, cap, ends, command_rx))
    }

    /// What units are prepared for now — after a
    /// [`reprepare`](Self::reprepare), the new one.
    pub fn prepare(&self) -> &Prepare {
        &self.prepare
    }

    /// Bounds what this editor may send from now on, **tightening only**:
    /// each field becomes the smaller of the one in force and `limits`', so
    /// `Limits::NONE` changes nothing. Every later
    /// [`commit`](Self::commit), [`package`](Self::package) and
    /// [`reprepare`](Self::reprepare) is refused
    /// ([`CommitError::TooManyOutputs`], [`CommitError::BlockTooLong`]) past
    /// them. Refused itself, changing nothing, when what was already sent
    /// breaks them: the plan sent last, or the `Prepare` in force (or
    /// pending, mid re-prepare).
    ///
    /// Everything reaches the executor through this editor, so a host that
    /// holds the editor while it sets these knows no commit already sent,
    /// and none sent later, can exceed them.
    ///
    /// # Errors
    ///
    /// [`CommitError::TooManyOutputs`] or [`CommitError::BlockTooLong`] when
    /// the plan sent last or the `Prepare` in force already exceeds the
    /// tightened limits; nothing changes.
    pub fn set_limits(&mut self, limits: Limits) -> Result<(), CommitError> {
        // Limits only tighten: each field is the smaller of the one in force
        // and the one asked for, so no caller can undo a host's bound (a
        // looser one would let through a graph the host's buffers cannot
        // run).
        let limits = Limits {
            max_main_outputs: limits.max_main_outputs.min(self.limits.max_main_outputs),
            max_block: limits.max_block.min(self.limits.max_block),
        };
        self.collect();
        if let Some(plan) = self.life.plan() {
            limits.outputs(plan.main_outputs())?;
        }
        limits.block(self.prepare.max_block().get())?;
        self.limits = limits;
        Ok(())
    }

    /// Attaches the host that folds the main output into a device: from now on a
    /// commit whose [`GraphSpec::main_output_id`] the gate has not published folds
    /// for, at the spec's width, is refused
    /// ([`CommitError::MainOutputNotPublished`]), so a main output id the host never heard
    /// of cannot reach the executor and render silence. The gate stays; a
    /// second call replaces it.
    pub fn set_main_output_gate(&mut self, gate: std::sync::Arc<dyn MainOutputGate>) {
        self.main_output_gate = Some(gate);
    }

    /// What this editor was bounded to with [`set_limits`](Self::set_limits).
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Whether `executor` is the one this editor sends to: the two were
    /// built together by [`Editor::new`].
    pub fn is_paired_with(&self, executor: &Executor<F>) -> bool {
        self.link.is_paired_with(executor)
    }

    /// The paired executor's play clock (`Env::play`), as it published it
    /// at the end of its last block: `(play_epoch, play)` at the next
    /// block's first frame, before a play start there. One wait-free load;
    /// before the first block it is the executor's starting reading (play
    /// 0). What the modulation compiler dates a rate edit's anchor from.
    pub fn play_reading(&self) -> crate::PlayReading {
        self.link.play_reading()
    }

    /// The output ports that lost events since the last call, one entry per
    /// port per block they lost some in, oldest first. Control thread; takes
    /// what the paired executor has noted, never blocks it.
    ///
    /// Each entry is exact for its block. The executor's
    /// [`dropped_event_count`](crate::Executor::dropped_event_count) is the
    /// total and is exact whether or not these were drained: a block that
    /// lost at more places than its log holds, or a control side that lets
    /// the ring (256 entries) fill, leaves entries out and the total stands.
    /// Drain it from the tick that collects (the session's `Collect` phase).
    pub fn take_event_losses(&mut self) -> impl Iterator<Item = crate::EventLoss> + '_ {
        std::iter::from_fn(|| self.link.pop_loss())
    }

    /// Refusals since the last call that [`take_event_losses`](Self::take_event_losses)
    /// did not name: a merge's, and those the per-block log (64 places) or the
    /// ring (256 entries) had no room for. Control thread.
    pub fn take_unreported_event_losses(&mut self) -> u64 {
        self.link.take_unreported()
    }

    /// Owned items the executor's apply had no room to hand back, since the
    /// editor was built: each was leaked rather than freed on the audio
    /// thread. Control thread; cumulative, read when [`collect`](Self::collect)
    /// has taken back what the executor returned.
    ///
    /// A nonzero count is a tutti bug worth reporting: the lists an apply
    /// fills are sized with the commit, so none should ever be too small.
    #[must_use]
    pub fn leaked_items(&self) -> u64 {
        self.link.leaked()
    }

    /// The graph value.
    pub fn spec(&self) -> &GraphSpec {
        &self.spec
    }

    /// The graph value, for wiring. Nodes are added with
    /// [`insert`](Self::insert), not here, so every node has a unit.
    /// Moves the [`revision`](Self::revision), whether or not the borrow
    /// writes.
    ///
    /// A gain entry is declared by the editor, which makes its cell
    /// ([`set_edge_gain`](Self::set_edge_gain),
    /// [`set_edge_gain_range`](Self::set_edge_gain_range),
    /// [`set_edge_fade`](Self::set_edge_fade)): wire a source
    /// ([`GraphSpec::connect_edge_gain`]) only to an edge declared that way,
    /// and never write [`GraphSpec::gains`] through this borrow. A commit
    /// that finds an entry without its cell makes one from the values.
    pub fn spec_mut(&mut self) -> &mut GraphSpec {
        self.touch();
        &mut self.spec
    }

    /// The values: each node's mode and mix, each port's range, and each gain
    /// lane's base and range.
    pub(crate) fn values(&self) -> &GraphValues {
        &self.values
    }

    /// The values, for a caller that builds the editor whole (a fork) or
    /// follows another graph's nodes (the test harness). Moves the
    /// [`revision`](Self::revision).
    pub(crate) fn values_mut(&mut self) -> &mut GraphValues {
        self.touch();
        &mut self.values
    }

    /// A count that moves whenever anything the editor shows a reader may
    /// have changed: the spec ([`spec_mut`](Self::spec_mut) included), the
    /// shapes and generations, the plan sent last ([`base`](Self::base)),
    /// a param range or its cells, the [`Prepare`]. Equal revisions mean
    /// none of those changed in between, so a control-side pass that is a
    /// function of them (the modulation reconcile) may skip its work. It
    /// may move with nothing changed, never the other way round. It does
    /// not cover [`play_reading`](Self::play_reading), which the audio
    /// thread moves.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// The shapes the units declared.
    pub fn shapes(&self) -> &Shapes {
        &self.shapes
    }

    /// The first of `nodes`, in the order given, whose declared
    /// [`rates`](crate::Shape::rates) leave out `rate`, with those rates:
    /// the one check a re-rate and a fork make.
    pub fn first_unsupported_rate(
        &self,
        nodes: impl IntoIterator<Item = NodeKey>,
        rate: SampleRate,
    ) -> Option<(NodeKey, SampleRates)> {
        nodes.into_iter().find_map(|node| {
            let supported = self.shapes.get(&node)?.rates();
            (!supported.contains(rate)).then_some((node, supported))
        })
    }

    /// Declares the bus edge `from -> to`: the output bus `from` feeds the input bus
    /// `to`, folded between the two declared topologies. The fold is checked
    /// **here**, against the shapes the units declared, so a failure is the
    /// author's call and nothing is stored. The commit lowers it to weighted
    /// lanes ([`GraphSpec::lowered`]); a source already declared is kept once.
    ///
    /// # Errors
    ///
    /// [`BusError::NoSuchNode`], [`BusError::NoSuchBus`] (a bus index the
    /// node does not declare on that side),
    /// [`BusError::Fold`] (the topologies cannot be folded: a channel with no
    /// place, a width that disagrees),
    /// [`BusError::MainInputTooWide`], and [`BusError::Overlaps`] /
    /// [`BusError::GainOnLane`] when a lane would land on an edge or a gain
    /// the spec already has.
    pub fn connect_bus(&mut self, from: BusOutputPort, to: NodeBus) -> Result<(), BusError> {
        self.spec
            .check_bus_edge(&self.shapes, from, to, BusFold::Speakers)?;
        self.touch();
        self.spec.connect_bus(from, to);
        Ok(())
    }

    /// Removes the bus edge `from -> to`: its lanes and their weights go with
    /// it at the next commit, and the author's own edges and gains stay.
    /// Returns whether it was declared.
    pub fn disconnect_bus(&mut self, from: BusOutputPort, to: NodeBus) -> bool {
        self.touch();
        self.spec.disconnect_bus(from, to)
    }

    /// Declares the bus edge `from -> main_output`: the output bus `from` feeds the
    /// graph's outputs, folded to the declared main output topology
    /// ([`GraphSpec::main_output`]). Checked here, as [`connect_bus`](Self::connect_bus)
    /// is, and nothing is stored on a failure. An undeclared main output refuses it.
    ///
    /// # Errors
    ///
    /// [`BusError::MainOutputUndeclared`], [`BusError::NoSuchNode`],
    /// [`BusError::NoSuchBus`], [`BusError::MainInputTooWide`],
    /// [`BusError::MainOutputFold`], and [`BusError::Overlaps`] /
    /// [`BusError::GainOnLane`] when a lane lands on an output source or a
    /// gain the spec already has.
    pub fn connect_bus_to_main_output(&mut self, from: BusOutputPort) -> Result<(), BusError> {
        self.spec
            .check_main_output_bus_edge(&self.shapes, from, BusFold::Speakers)?;
        self.touch();
        self.spec.connect_bus_to_main_output(from);
        Ok(())
    }

    /// Removes the bus edge `from -> main_output`, its lanes with it. Returns
    /// whether it was declared.
    pub fn disconnect_bus_from_main_output(&mut self, from: BusOutputPort) -> bool {
        self.touch();
        self.spec.disconnect_bus_from_main_output(from)
    }

    /// The spec with its bus edges lowered against the shapes as they stand:
    /// what the next commit compiles ([`GraphSpec::lowered`]).
    ///
    /// # Errors
    ///
    /// As [`GraphSpec::lowered`].
    pub fn lowered(&self) -> Result<Lowering, BusError> {
        self.spec.lowered(&self.shapes)
    }

    /// Lowers `spec` against `shapes` and validates the result, with the
    /// weights of the lanes the lowering made. The one door a commit, a
    /// re-prepare and its resume go through: a spec with no bus edge is
    /// validated as it stands.
    fn lower_and_validate(
        spec: &GraphSpec,
        shapes: &Shapes,
    ) -> Result<(ValidGraph, BusLanes), CommitError> {
        if !spec.has_bus_edges() {
            let valid = spec.validate().map_err(CommitError::Invalid)?;
            return Ok((valid, BTreeMap::new()));
        }
        let lowering = spec
            .lowered(shapes)
            .map_err(|e| CommitError::Compile(CompileError::Bus(e)))?;
        let valid = lowering.spec.validate().map_err(CommitError::Invalid)?;
        Ok((valid, lowering.weights()))
    }

    /// Crossfades sent and not yet drained back: running, waiting, or
    /// ended and waiting for [`collect`](Self::collect). At most
    /// [`FADE_CAPACITY`](crate::FADE_CAPACITY).
    pub fn fades_in_flight(&self) -> usize {
        self.link.fades_in_flight()
    }

    /// Commits sent and not yet drained back.
    pub fn in_flight(&self) -> usize {
        self.link.in_flight()
    }

    /// Whether a [`reprepare`](Self::reprepare) is between its two commits:
    /// until the next [`collect`](Self::collect) after the executor hands the
    /// units back, `commit`, `replace`, `set_latency` and `reprepare` refuse
    /// with [`CommitError::Repreparing`]. A host that must not lose what it
    /// would pass to one of them (a `replace` consumes its node) asks first.
    pub fn is_repreparing(&self) -> bool {
        matches!(self.life, Lifecycle::Repreparing { .. })
    }

    /// The cause the editor was poisoned by, if it is ([`CommitError::Poisoned`]).
    pub fn poison_cause(&self) -> Option<&str> {
        match &self.life {
            Lifecycle::Poisoned { cause, .. } => Some(cause),
            _ => None,
        }
    }

    /// The plan sent last — what the next commit is compiled against.
    pub fn base(&self) -> Option<&Arc<Plan>> {
        self.life.plan()
    }

    /// Where `key`'s forks come from, and the generation of the unit that
    /// handed it over, if it is forkable.
    pub(crate) fn fork_source(&self, key: NodeKey) -> Option<(u32, &dyn ForkSource<F>)> {
        self.placements.fork_source(key)
    }

    /// Watch `health` for the forked unit at `key` (`Editor::fork` only).
    pub(crate) fn watch_fork(&mut self, key: NodeKey, health: Arc<dyn ForkHealth>) {
        self.fork_health.push((key, health));
    }

    /// The forked units' probes, in key order.
    pub(crate) fn fork_probes(&self) -> &[(NodeKey, Arc<dyn ForkHealth>)] {
        &self.fork_health
    }

    /// Events per event slot per block, as the executor was built with.
    pub(crate) fn event_capacity(&self) -> usize {
        self.event_capacity
    }
    /// Delivers `payload` into event input `to` at time `at` on its exact
    /// frame: an [`EventKind`](crate::EventKind) (a note, a chord) into the port's events, or a
    /// [`ParamKind`](crate::ParamKind) (a ramp or a segment) into its param
    /// events ([`SortedEvents::params`](crate::SortedEvents::params)), both
    /// through [`Scheduled`]. Control thread; the command crosses to the
    /// executor over a preallocated ring of plain values, so nothing is freed
    /// on the audio thread. The rules:
    ///
    /// - **`at` is required.** [`At::NextBlock`] is the untimed case, and it
    ///   has to be spelled.
    /// - The port is checked against the plan sent last, and the command is
    ///   delivered to whichever unit holds that key when it falls due.
    /// - A frame already past when the executor sees it lands at offset 0 of
    ///   that block and is counted
    ///   ([`Executor::late_command_count`](crate::Executor::late_command_count)) —
    ///   never dropped. Beats follow a different rule; see below.
    /// - A param ramp or segment goes only to a node that declares
    ///   [`Shape::param_events`](crate::Shape::param_events); any other is
    ///   refused ([`ScheduleError::NoParamEvents`]). It is merged into the
    ///   port's param events by the order rules of the events: after the
    ///   port's own at a tie, by timeline position, then by scheduling
    ///   order. A bypassed node is sent none.
    /// - At most [`COMMAND_CAPACITY`](crate::COMMAND_CAPACITY) commands are
    ///   outstanding; past that this returns
    ///   [`ScheduleError::Backpressure`] and sends nothing. A command holds
    ///   its credit until it lands or is [`cancel`](Self::cancel)led — and a
    ///   beat-timed one can wait indefinitely (a stopped transport, a beat
    ///   past the loop end, one a seek jumped over), so cancel what you no
    ///   longer want.
    ///
    /// **Frames and beats fall due differently.** A frame is
    /// never dropped: already past, it lands at offset 0 of the next block
    /// and is counted late. A beat fires when the playhead reaches or crosses
    /// it by continuous playback — a loop wrap landing at or after it counts
    /// — and is late only if continuous playback crossed it before the
    /// command was processed; a beat a seek or loop jumps *over* stays
    /// pending until reached or cancelled. Pairing (a note-off for every
    /// note-on) is the caller's job: a note-on that fires and a note-off that
    /// waits is a stuck note, and `cancel` is how to take the other back.
    ///
    /// # Errors
    ///
    /// Nothing is sent on [`ScheduleError::NoPlan`] (nothing committed yet),
    /// [`ScheduleError::NoSuchPort`] (`to` is not an event input of the plan
    /// sent last), [`ScheduleError::NoParamEvents`] (a ramp or segment to a
    /// node that does not declare param events),
    /// [`ScheduleError::Backpressure`] or [`ScheduleError::Poisoned`].
    pub fn schedule(
        &mut self,
        at: At,
        to: EventInputPort,
        payload: impl Into<Scheduled>,
    ) -> Result<CommandId, ScheduleError> {
        let payload = payload.into();
        if self.poison_cause().is_some() {
            return Err(ScheduleError::Poisoned);
        }
        let plan = self.life.plan().ok_or(ScheduleError::NoPlan)?;
        let unit = plan.unit(to.node).filter(|u| to.port < u.shape.event_in);
        let Some(unit) = unit else {
            return Err(ScheduleError::NoSuchPort { to });
        };
        if matches!(payload, Scheduled::Param(_)) && !unit.shape.param_events {
            return Err(ScheduleError::NoParamEvents { to });
        }
        self.link.schedule(at, to, payload)
    }

    /// Takes back scheduled command `id`, if it has not landed — freeing its
    /// credit. A no-op for one that has. Travels on its own ring, so it works
    /// even with every credit held; refused with `Backpressure` only when
    /// [`CANCEL_CAPACITY`](crate::CANCEL_CAPACITY) cancels are waiting for
    /// the executor's next block.
    ///
    /// # Errors
    ///
    /// [`ScheduleError::Backpressure`] (above),
    /// [`ScheduleError::UnknownCommand`] for an id this editor never issued,
    /// or [`ScheduleError::Poisoned`].
    pub fn cancel(&mut self, id: CommandId) -> Result<(), ScheduleError> {
        if self.poison_cause().is_some() {
            return Err(ScheduleError::Poisoned);
        }
        self.link.cancel(id)
    }

    /// Takes back every command scheduled so far that has not landed.
    ///
    /// # Errors
    ///
    /// [`ScheduleError::Backpressure`] when the cancel ring is full, or
    /// [`ScheduleError::Poisoned`].
    pub fn cancel_all(&mut self) -> Result<(), ScheduleError> {
        if self.poison_cause().is_some() {
            return Err(ScheduleError::Poisoned);
        }
        self.link.cancel_all()
    }

    /// Scheduled commands sent and not yet delivered.
    pub fn commands_outstanding(&self) -> usize {
        self.link.commands_outstanding()
    }
}

/// The weighted lanes a lowering made, by edge: the fold's weight and the fade
/// the lane runs.
type BusLanes = BTreeMap<EdgeId, (Gain, EdgeFade)>;

/// The crossfades `delta` starts: one per fade that is not a plain swap.
/// Each is built into the commit and comes back once on the fade-return
/// ring (`Commit::build`).
fn starting(delta: &Delta) -> usize {
    delta.fades.iter().filter(|(_, f)| !f.is_cut()).count()
}
