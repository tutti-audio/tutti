//! [`Graph`]: a `tutti-graph` [`Editor`], the nodes inserted through it, the
//! event feeds into them, the replacements waiting on a re-prepare, and — until
//! an engine or a test takes it — the [`Executor`] it sends to.
//!
//! What each method does to the editor:
//!
//! | `Graph` | the editor |
//! |---|---|
//! | `add` | `Editor::insert` of the node's own [`IntoNode`] at a [`NodeKey`] minted by `NodeKey::fresh`, its controls, params and reclaimer kept ([`SessionNode`]) |
//! | `set_source`, `set_sources`, `add_source`, `remove_source`, the `output` forms, `widen_outputs` | written into `editor.spec_mut()` (a port holds a set of sources, which the graph sums) |
//! | `replace` | `Editor::replace_or_swap` with the [`Replacement`]'s [`Fade`]: a plain swap when there is nothing to fade from or the fade is zero, a refusal for a fade over another shape; parked while the graph re-prepares |
//! | `node_latency`, `node_tail`, `node_inputs`, `node_outputs` | the editor's [`Shapes`](tutti_graph::Shapes) |
//! | `set_feed`, `move_feeds` | the union of every feeder's sources, written as a sink's event edges beside the host's own |
//! | `refresh_node_latency` | `Editor::set_latency` |
//! | `add_plugin`, `replace_plugin` | the bound `PluginClient` itself, a node with its own fork source |
//!
//! The commit is the [`Session`](crate::Session)'s: it publishes the plan's
//! compensation with it.
//!
//! # Where the audio side lives
//!
//! `Editor::new` builds the pair. The executor stays here ("local") until
//! `take_audio_side` or the session builder hands it
//! to an engine. While it is local, a commit is applied at once on this
//! thread and its box collected, so a detached graph never meets
//! back-pressure, and `Session::render_frame`
//! runs it.
//!
//! # A param write lands on the next block, never at once
//!
//! There is no control-side copy of a node for a setting to land on at once.
//! A write (`Session::set` on a [`ParamTarget`](crate::ParamTarget)) goes into
//! the node's own `Param` cell, and the node reads it at the start of the
//! executor's next block: one block later, on every graph.

use std::any::Any;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use tutti_buffers::Float;
#[cfg(feature = "testing")]
use tutti_buffers::InterleavedMut;
#[cfg(feature = "testing")]
use tutti_graph::TransportPosition;

#[cfg(feature = "export")]
use crate::fork::PendingExport;

use tutti_buffers::ChannelLayout;
use tutti_buffers::MaxBlock;
use tutti_graph::Compensation;
use tutti_graph::NodeParts;
#[cfg(feature = "testing")]
use tutti_graph::fade_gains;
use tutti_graph::{BusRef, BusSource, Buses, EdgeFade, EdgeGain};
use tutti_graph::{
    CommitError, Editor, EventEdge, EventIn, EventOut, Executor, Fade, GraphInvalid, InletNode,
    InletRing, IntoNode, MeterReader, NodeMode, ParamIn, ParamMod, ParamRange, Prepare, RenderMode,
};
use tutti_graph::{
    Edge as RawEdge, EdgeId, InPort as RawIn, NodeKey, OutPort as RawOut, PortSink, PortSource,
};
use tutti_graph::{
    EventCapacity, EventInletNode, EventInletWriter, EventTapId, MeterId, MeterSpec, MissingSource,
    TapId,
};
use tutti_rt::Mint;
#[cfg(feature = "plugin")]
use tutti_units::Latency;
use tutti_units::{Amplitude, Mix, SampleRate, UnitAddr};
use tutti_units::{Samples, Tail};

mod observe;
mod phases;
#[cfg(feature = "testing")]
mod testing;

pub use observe::{Meters, NotReopened, Reopened, RootReader, Taps};
#[cfg(feature = "testing")]
pub use testing::{AudioSide, Testing};

use crate::key::{
    Bus, BusSink, Disconnection, Fanin, InEnd, Inlet, MainSink, OutEnd, Outlet, Removal,
    SetSources, Wire,
};
use crate::value::{GainTarget, GainTargetError, ValueTable, Write};
use crate::{
    ConnectError, Connection, DeclarationError, Edge, EventInPort, EventOutPort, InPort,
    InletError, Key, NoSuchNode, OutPort, RemoveError, Root, SessionNode,
};
use tutti_rt::LockExt;

/// The largest block a device's graph is prepared for.
///
/// A device block longer than this is rendered by `tutti_engine::Engine` as
/// consecutive graph blocks of at most this many frames, so it bounds the
/// arena, not the device. 1024 frames covers every buffer size a DAW offers
/// by default.
pub const LIVE_MAX_BLOCK: MaxBlock = MaxBlock::of(Samples::new(1024));

/// The spec `kind` of a hosted plugin.
#[cfg(feature = "plugin")]
const PLUGIN_KIND: &str = "tutti-session:plugin";

/// A node to swap in at a key, and the fade it swaps in over:
/// [`Graph::replace`]'s argument, and `Graph::replace_plugin`'s (feature
/// `plugin`) with a `Box<PluginClient>`.
///
/// The fade is [`DECLICK`](tutti_graph::DECLICK) along the default
/// [`FadeLaw`](tutti_graph::FadeLaw), at the graph's rate when the swap is made, unless
/// [`fade`](Self::fade) sets one.
#[derive(Clone, Debug)]
pub struct Replacement<N> {
    incoming: N,
    fade: Option<Fade>,
}

impl<N> Replacement<N> {
    /// `incoming` over the graph's declick fade.
    #[must_use]
    pub fn new(incoming: N) -> Self {
        Self {
            incoming,
            fade: None,
        }
    }

    /// Swaps over `fade` instead of the declick: its length in frames and its
    /// law. A zero length is a plain swap at the next block.
    #[must_use]
    pub fn fade(mut self, fade: Fade) -> Self {
        self.fade = Some(fade);
        self
    }
}

/// Why [`Graph::replace`] took nothing. The incoming node was dropped.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ReplaceRefused {
    /// The graph is poisoned (a re-prepare failed with its units out); the
    /// cause is the editor's.
    #[error("the graph is poisoned ({0}); build a new one")]
    Poisoned(String),
    /// The node is not in the graph.
    #[error("{0:?} is not in the graph")]
    NoSuchNode(NodeKey),
    /// The editor refused the swap.
    #[error("the graph refused the swap: {0}")]
    Editor(tutti_graph::CommitError),
    /// The incoming node's shape differs from the running node's, and the
    /// [`Replacement`] asked for a fade: a crossfade needs the same ports,
    /// buses and latency. Nothing changed and the incoming node was dropped.
    /// A caller who accepts the cut passes a zero-length [`Fade`]
    /// ([`Replacement::fade`]).
    #[error(
        "{node:?} cannot fade to a node of another shape; replace it with a zero-length fade to cut"
    )]
    ShapeChanged {
        /// The node.
        node: NodeKey,
        /// The running node's shape.
        running: Box<tutti_graph::Shape>,
        /// The incoming node's shape.
        incoming: Box<tutti_graph::Shape>,
    },
}

impl From<tutti_graph::CommitError> for ReplaceRefused {
    fn from(error: tutti_graph::CommitError) -> Self {
        match error {
            tutti_graph::CommitError::FadeShape {
                node,
                running,
                incoming,
            } => Self::ShapeChanged {
                node,
                running,
                incoming,
            },
            other => Self::Editor(other),
        }
    }
}

/// Why a swap made now took nothing: the graph is re-preparing and hands the
/// incoming value back, or the call is refused for good.
pub(crate) enum SwapRefused<U> {
    /// A sample-rate or block-size change is between its two commits.
    Busy(U),
    Failed(ReplaceRefused),
}

impl<U> From<ReplaceRefused> for SwapRefused<U> {
    fn from(refused: ReplaceRefused) -> Self {
        Self::Failed(refused)
    }
}

/// Who feeds a sink's event input 0 through [`Graph::set_feed`]. Each feeder
/// owns its own entry, so feeders never overwrite each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Feeder {
    /// A clip the session's sequencer plays.
    Sequencer,
    /// A live input the session made.
    LiveInput,
    /// The session's MIDI route rules.
    Routes,
    /// The timed queue of an instrument's handle
    /// (`Session::add_instrument`, feature `midi`).
    Instrument,
    /// A host's declarations.
    Declared,
    /// The session's MIDI clock, which feeds every output port
    /// (`open_midi_output`, feature `midi-hardware`).
    Clock,
}

/// One node's handle. Its params by address are the value table's.
struct Entry {
    /// What frees the values its published cells retired
    /// ([`SessionNode::reclaimer`]), run by [`Graph::reclaim`].
    reclaimer: Option<crate::Reclaimer>,
    /// The node's controls ([`IntoNode::Controls`]), kept for the node's
    /// life: a removal or a swap moves them to the graph's retiring list,
    /// dropped once the outgoing node has left the render, so a handle whose
    /// last drop releases something (a voice's stream) holds it while the node
    /// still renders. `None` for a node placed without [`SessionNode`]. Read
    /// by [`Graph::controls`].
    controls: Option<Held>,
}

/// How a node's controls are put under its key: its params, its slots and
/// its reclaimer, as [`Graph::address`] does for the type that made them.
type Addressing = fn(&ValueTable, NodeKey, &(dyn Any + Send)) -> Option<crate::Reclaimer>;

/// A node's controls with the means to address them again, which a swap that
/// is refused when it lands needs to give the outgoing node its values back.
struct Held {
    controls: Box<dyn Any + Send>,
    addressing: Addressing,
}

/// What a swap does with a fade over a node that does not fit the running
/// one's shape.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OnShape {
    /// Refuse it ([`ReplaceRefused::ShapeChanged`]): what a caller's
    /// [`replace`](Graph::replace) gets.
    Refuse,
    /// Fade where the shape fits and cut where it does not: what the
    /// session's own swaps (a plugin reloaded, rebuilt for new ports, one
    /// that came back) get.
    #[cfg(feature = "plugin")]
    Cut,
}

/// What the controls under a key were when a replacement parked: the
/// outgoing node's, or the incoming one an earlier parked replacement put
/// there (`None`: a node with no controls).
struct Stash(Option<Held>);

/// The executor, while this side still holds it, and what a local render
/// needs beside it.
struct Local<F: Float> {
    exec: Executor<F>,
    /// Planar scratch, one `Vec` per global output.
    #[cfg(feature = "testing")]
    scratch: Vec<Vec<F>>,
}

/// A node waiting to be swapped in, its type erased.
trait ErasedSwap<F: Float>: Send {
    /// Swap it in under `node`.
    fn swap(
        self: Box<Self>,
        graph: &mut Graph<F>,
        node: NodeKey,
        fade: Option<Fade>,
    ) -> Result<(), SwapRefused<Box<dyn ErasedSwap<F>>>>;
}

/// A node split into its unit and its controls, the unit as the
/// [`IntoNode`] a swap places: the controls were addressed under the key
/// when the swap parked, and the node still names its own type's kind.
struct Unit<N, F: Float> {
    parts: NodeParts<(), F>,
    node: std::marker::PhantomData<fn() -> N>,
}

impl<N: IntoNode<F>, F: Float> IntoNode<F> for Unit<N, F> {
    type Controls = ();

    fn into_parts(self) -> NodeParts<(), F> {
        self.parts
    }

    fn kind() -> &'static str {
        N::kind()
    }
}

/// A node's unit waiting to be swapped in; its controls are already under
/// the key.
struct Swap<N, F: Float>(Unit<N, F>);

impl<F: Float, N> ErasedSwap<F> for Swap<N, F>
where
    N: SessionNode<F>,
{
    fn swap(
        self: Box<Self>,
        graph: &mut Graph<F>,
        node: NodeKey,
        fade: Option<Fade>,
    ) -> Result<(), SwapRefused<Box<dyn ErasedSwap<F>>>> {
        match graph.place_unit(node, self.0, fade, OnShape::Refuse) {
            Ok(()) => {
                graph.drop_plugin_record(node);
                Ok(())
            }
            Err(SwapRefused::Busy(unit)) => Err(SwapRefused::Busy(Box::new(Swap(unit)))),
            Err(SwapRefused::Failed(refused)) => Err(SwapRefused::Failed(refused)),
        }
    }
}

#[cfg(feature = "plugin")]
struct PluginSwap(Box<tutti_plugin::PluginClient>);

#[cfg(feature = "plugin")]
impl ErasedSwap<f32> for PluginSwap {
    fn swap(
        self: Box<Self>,
        graph: &mut Graph,
        node: NodeKey,
        fade: Option<Fade>,
    ) -> Result<(), SwapRefused<Box<dyn ErasedSwap<f32>>>> {
        match graph.replace_plugin_now(node, self.0, fade) {
            Ok(()) => Ok(()),
            Err(SwapRefused::Busy(c)) => Err(SwapRefused::Busy(Box::new(PluginSwap(c)))),
            Err(SwapRefused::Failed(refused)) => Err(SwapRefused::Failed(refused)),
        }
    }
}

/// A replacement parked while the graph re-prepares.
struct Parked<F: Float> {
    node: NodeKey,
    swap: Box<dyn ErasedSwap<F>>,
    fade: Option<Fade>,
    /// What the key's controls were when it parked, `None` for a plugin
    /// swap, which leaves them alone until it lands.
    stash: Option<Stash>,
}

impl<F: Float> std::fmt::Debug for Parked<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parked")
            .field("node", &self.node)
            .field("fade", &self.fade)
            .finish_non_exhaustive()
    }
}

/// What a parked replacement came to on a retry.
pub(crate) enum Retried {
    Landed {
        node: NodeKey,
    },
    Failed {
        node: NodeKey,
        refused: ReplaceRefused,
    },
}

/// The engine's output folds and the device it renders into.
struct Placed<F: Float> {
    control: tutti_engine::OutputControl<F>,
    device: ChannelLayout,
}

/// The editable audio graph. See the module docs.
///
/// Every method runs on the control side, never on the audio thread; they
/// may allocate. Edits mark the graph edited, and the session's next
/// [`update`](crate::Session::update) commits them, all at once.
///
/// # Why opaque
///
/// Every method is named in graph terms (add a node, connect two ports,
/// replace a node under a fade) and takes a node as `impl Into<NodeKey>` and
/// ports as [`OutPort`] and [`InPort`], never a runtime type. There is no access to the
/// `Editor`: the edit/commit boundary stays visible.
pub struct Graph<F: Float = f32> {
    editor: Editor<F>,
    local: Option<Box<Local<F>>>,
    nodes: BTreeMap<NodeKey, Entry>,
    /// What each node's params are written through: the session's value
    /// table, which the session's `Values` share.
    values: Arc<ValueTable>,
    /// Edited since the last commit: what `update()` commits on, and what a
    /// local render commits first.
    edited: bool,
    /// The refusal reported last, until a commit is sent: a spec that keeps
    /// failing the same way is reported once.
    refused: Option<CommitError>,
    /// Plans sent, for a host (or a test) asking whether an update sent one.
    commits: u64,
    /// Threads per block, the caller included (`0` for one per core).
    render_workers: crate::Workers,
    /// Run by each live helper on its own thread, before it takes work.
    render_worker_start: Option<RenderWorkerStart>,
    /// Each sink's event input 0, by feeder: the sources each feeder feeds
    /// it. Written into the spec as their union.
    feeds: BTreeMap<NodeKey, BTreeMap<Feeder, Vec<EventOutPort>>>,
    /// Each fed sink's event input 0: the sources on it that the feeds put
    /// there and the host did not wire itself, which are all a change of the
    /// feeds may take away. Every other edge on the input is the host's.
    fed_edges: BTreeMap<NodeKey, Vec<EventOutPort>>,
    /// Names every tap, event tap and meter of this graph.
    ids: Mint,
    /// Each open event tap as it was opened (its port, alignment and
    /// capacity), so [`Taps::reopen_event`] can open it
    /// again.
    event_taps: BTreeMap<EventTapId, (tutti_graph::EventTapSource, tutti_graph::EventTapOptions)>,
    /// The master meter's reader until a host takes it.
    master_meter: Option<MeterReader>,
    /// The master meter's id: the one meter that reads the root.
    master_meter_id: MeterId,
    /// Each open tap on the root ([`Taps::open_root`]) with its capacity and
    /// the width its reader has, so a root of another width opens it again.
    output_taps: BTreeMap<TapId, (Samples, usize)>,
    /// The readers the root's width opened again, each held under its id until
    /// claimed ([`Graph::take_reopened`]).
    reopened: BTreeMap<RootReader, observe::Held<F>>,
    /// Crossfades refused `Busy`, in request order.
    parked: VecDeque<Parked<F>>,
    /// The nodes whose waiting replacements a removal dropped, until the
    /// session reports them ([`SessionEvent::ReplaceDropped`](crate::SessionEvent::ReplaceDropped)).
    dropped: Vec<NodeKey>,
    /// The wiring each re-insert dropped, by node, until the commit that
    /// carries the re-insert is sent and the session reports it
    /// ([`SessionEvent::Unwired`](crate::SessionEvent::Unwired)).
    unwired: Vec<Unwiring>,
    /// The nodes whose wiring the commits since the last call changed: the
    /// first commit's `was` and the last's `now`, until the session reports
    /// them ([`SessionEvent::Rewired`](crate::SessionEvent::Rewired)).
    rewired: Vec<tutti_graph::Rewired>,
    /// The nodes [`remove`](Self::remove) took out since the session last
    /// forgot what it declared on them ([`Graph::take_removed`]).
    removed: Vec<NodeKey>,
    /// What the root's channels are, when a host declared it
    /// ([`Session::declare_root_topology`](crate::Session::declare_root_topology));
    /// `None` is the engine's order for the root's width.
    root: Option<tutti_buffers::ChannelTopology>,
    /// The engine's output folds and the device it renders into, once an
    /// engine was built over this graph: a declaration is published through
    /// it, and a commit must fold into the device. One value, so a graph has
    /// folds to publish exactly when it has a device to place them on; a
    /// graph with no engine has neither, and nothing folds its root.
    output: Option<Placed<F>>,
    /// What the nodes logged on the audio thread, once an engine renders the
    /// graph: drained by the session's reclaim sweep ([`Graph::drain_rt_log`]).
    rt_log: Option<tutti_graph::RtLogReaders>,
    /// The last widening [`widen_outputs`](Self::widen_outputs) refused.
    widen_refusal: Option<tutti_graph::RootError>,
    /// Why the declaration that waited for a root crossfade, or a hard one
    /// made through [`harden_root`](Self::harden_root), could not be applied;
    /// the session takes it as its declaration refusal
    /// ([`take_apply_refusal`](Self::take_apply_refusal)). A slot of its own,
    /// so a widening neither hides nor erases it.
    apply_refusal: Option<tutti_graph::RootError>,
    /// A root declared while a root crossfade ran, applied when it ends:
    /// the newest declaration (`Some(None)` takes the declaration back).
    pending_root: Option<Option<tutti_buffers::ChannelTopology>>,
    /// The hosted plugins' records, by node (`plugin.rs`): kept here so a
    /// crossfade to another plugin and a removal update them where they
    /// happen.
    #[cfg(feature = "plugin")]
    pub(crate) plugins: BTreeMap<NodeKey, crate::plugin::HostedPlugin>,
    /// What a removal or a replacement took out of `plugins`, held until the
    /// main thread drains it (`Session::update` there).
    #[cfg(feature = "plugin")]
    pub(crate) graveyard: crate::plugin::Graveyard,
    /// The controls of a node removed or swapped out, held until the editor
    /// hands back its retired unit ([`Editor::collect`]): a node whose edges
    /// fade out keeps rendering until then, and its controls (a voice's
    /// stream) live as long.
    retiring: Vec<(NodeKey, Box<dyn Any + Send>)>,
    /// A reader of the session's tempo map, the template each plugin and
    /// each sampler node inserted through the graph is bound with (a clone
    /// of it: the node lends the map once per block and a fork clones its
    /// reader on the control side; [`SessionNode::bind_tempo`]). The
    /// publisher is the session's `TransportSettings`, whose `Reclaim` sweep
    /// already covers the cell.
    pub(crate) tempo: tutti_timeline::TempoReader,
    /// A re-prepare resumed in the update's collect: its plan's figures are
    /// published at the commit.
    resumed: bool,
    /// The per-channel pre-roll of the plan sent last, for sources outside
    /// the graph (a disk stream), republished with every plan.
    compensation: Arc<tutti_graph::PrerollTable>,
    /// The total latency of the plan sent last.
    latency: Samples,
}

/// The wiring a re-insert at one key dropped, in the session's port types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unwiring {
    pub(crate) node: NodeKey,
    pub(crate) edges: Vec<Edge>,
    pub(crate) event_edges: Vec<crate::EventEdge>,
    pub(crate) bus_edges: Vec<(BusSource, BusSink)>,
}

/// A hook each live render helper runs once on its own thread, with its
/// index (`1..`), before it takes work: see
/// [`SessionBuilder::render_worker_start`](crate::SessionBuilder::render_worker_start).
#[derive(Clone)]
pub(crate) struct RenderWorkerStart(Arc<dyn Fn(usize) + Send + Sync>);

impl RenderWorkerStart {
    /// Wrap `hook`.
    pub fn new(hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
        Self(Arc::new(hook))
    }

    /// Run the hook for helper `index`.
    pub fn call(&self, index: usize) {
        (self.0)(index);
    }
}

impl std::fmt::Debug for RenderWorkerStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderWorkerStart").finish_non_exhaustive()
    }
}

impl<F: Float> std::fmt::Debug for Graph<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Graph");
        d.field("editor", &self.editor)
            .field("edited", &self.edited)
            .field("commits", &self.commits)
            .field("render_workers", &self.render_workers)
            .field("feeds", &self.feeds)
            .field("fed_edges", &self.fed_edges)
            .field("parked", &self.parked);
        #[cfg(feature = "plugin")]
        d.field("plugins", &self.plugins);
        d.finish_non_exhaustive()
    }
}

/// What a commit came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Committed {
    /// Sent.
    Sent,
    /// Refused, with why. The plan sent last keeps playing and the edit flag
    /// stays set, so each update tries the spec again until an edit fixes it;
    /// [`Session::update`](crate::Session::update) reports a refusal once, as
    /// [`SessionEvent::CommitRefused`](crate::SessionEvent::CommitRefused).
    Refused(CommitError),
    /// Refused for now — the executor has not drained enough commits, or a
    /// re-prepare is between its halves. The edit flag stays set, and the
    /// next update retries.
    Retry,
}

fn compensation_of(plan: &tutti_graph::Plan) -> Compensation {
    Compensation::new(plan.compensation().to_vec(), plan.total_latency().samples())
}

/// The graph's `Prepare`: `rate`, `max_block`, and the device's callback
/// `quantum` when the host knows it (`tutti_engine::OutputSpec::quantum`). A
/// hosted out-of-process plugin ships one quantum per callback to its server,
/// so the quantum is what keeps its pipeline in step with the device. The
/// session's own graph is live: [`RenderMode::Realtime`]. An offline render or
/// a bounce prepares its fork with `RenderGraph::prepare`, which is offline.
pub(crate) fn prepare_for(
    rate: SampleRate,
    max_block: MaxBlock,
    quantum: Option<MaxBlock>,
) -> Prepare {
    let p = Prepare::new(rate, max_block).with_render_mode(RenderMode::Realtime);
    match quantum {
        Some(q) => p.with_quantum(q),
        None => p,
    }
}

/// `latency` clamped to what PDC compensates at `rate`, as the editor clamps a
/// latency it probes at insert.
#[cfg(feature = "plugin")]
fn clamp_latency(latency: Latency, rate: SampleRate) -> Latency {
    Latency::new(
        latency
            .samples()
            .min(tutti_graph::MAX_NODE_LATENCY.to_samples_ceil(rate)),
    )
}

impl<F: Float> Graph<F> {
    /// An empty graph with `inputs` global inputs and `outputs` global
    /// outputs, prepared for `rate`, `max_block` and the device's callback
    /// `quantum` (when known), its executor local.
    pub(crate) fn new(
        inputs: ChannelLayout,
        outputs: usize,
        rate: SampleRate,
        max_block: MaxBlock,
        quantum: Option<MaxBlock>,
        tempo: tutti_timeline::TempoReader,
    ) -> Self {
        let (mut editor, exec) = Editor::<F>::with_format(prepare_for(rate, max_block, quantum));
        let topology = &mut editor.spec_mut().topology;
        topology.inputs = inputs;
        topology.set_outputs(vec![PortSource::Zero; outputs]);
        // Declared and on from the start: a host reads the master level
        // without asking for it.
        // `Outputs` names no source, so the open cannot be refused.
        let ids = Mint::new();
        let master_meter_id = MeterId::mint(&ids);
        let master_meter = editor
            .open_meter(master_meter_id, MeterSpec::Outputs)
            .expect("a master meter names no source");
        master_meter.enable();
        Self {
            editor,
            local: Some(Box::new(Local {
                exec,
                #[cfg(feature = "testing")]
                scratch: Vec::new(),
            })),
            nodes: BTreeMap::new(),
            values: ValueTable::new(),
            edited: true,
            refused: None,
            commits: 0,
            render_workers: crate::Workers::SERIAL,
            render_worker_start: None,
            feeds: BTreeMap::new(),
            fed_edges: BTreeMap::new(),
            ids,
            event_taps: BTreeMap::new(),
            master_meter: Some(master_meter),
            master_meter_id,
            output_taps: BTreeMap::new(),
            reopened: BTreeMap::new(),
            parked: VecDeque::new(),
            dropped: Vec::new(),
            unwired: Vec::new(),
            rewired: Vec::new(),
            removed: Vec::new(),
            root: None,
            output: None,
            rt_log: None,
            widen_refusal: None,
            apply_refusal: None,
            pending_root: None,
            #[cfg(feature = "plugin")]
            plugins: BTreeMap::new(),
            #[cfg(feature = "plugin")]
            graveyard: crate::plugin::Graveyard::default(),
            retiring: Vec::new(),
            tempo,
            resumed: false,
            compensation: Arc::default(),
            latency: Samples::new(0),
        }
    }

    /// What every node is prepared for: the rate, the largest block, and the
    /// device's callback size when the session knows it.
    pub(crate) fn prepared(&self) -> Prepare {
        *self.editor.prepare()
    }

    pub(crate) fn set_render_workers(&mut self, workers: crate::Workers) {
        self.render_workers = workers;
    }

    /// Makes every tap the graph opens from now on carry a bell from `bells`.
    pub(crate) fn notify_with(&mut self, bells: tutti_rt::BellsHandle) {
        self.editor.notify_with(bells);
    }

    pub(crate) fn set_render_worker_start(&mut self, start: Option<RenderWorkerStart>) {
        self.render_worker_start = start;
    }

    /// A fresh pool of the configured size, its threads named `name-<i>`, or
    /// `None` for one participant or when the system refuses a thread
    /// (render on the caller alone). Each helper runs `start` first.
    pub(crate) fn render_pool(
        &self,
        name: &str,
        start: Option<&RenderWorkerStart>,
    ) -> Option<Arc<dyn tutti_graph::Pool>> {
        let n = self.render_workers.threads();
        if n < 2 {
            return None;
        }
        let name = name.to_owned();
        let mut builder = tutti_graph::WorkerPool::builder()
            .num_threads(n)
            .thread_name(move |i| format!("{name}-{i}"));
        if let Some(start) = start {
            let start = start.clone();
            builder = builder.start_handler(move |i| start.call(i));
        }
        let pool = builder
            .build()
            .inspect_err(|e| {
                tracing::warn!("graph: a {n}-thread worker pool was refused, rendering alone: {e}");
            })
            .ok()?;
        Some(Arc::new(pool))
    }

    /// Hand the executor over: to the engine, or to a test's audio side.
    fn take_executor(&mut self) -> Executor<F> {
        self.local
            .take()
            .expect("the graph's audio side was already taken")
            .exec
    }

    /// Build the engine over this graph's audio side, which it takes:
    /// `Engine::new` over the editor and its executor, which bounds the
    /// editor to what the engine can render.
    ///
    /// The engine renders into `device` channels, stated as `topology` when
    /// the caller knows its wiring and otherwise the engine's order for the
    /// width.
    ///
    /// # Errors
    ///
    /// The engine's refusal of the graph, or the
    /// [`FoldError`](tutti_buffers::FoldError) when the device has no channels
    /// or `topology` is not its width.
    pub(crate) fn engine(
        &mut self,
        transport: &tutti_timeline::Transport,
        device: ChannelLayout,
        topology: Option<tutti_buffers::ChannelTopology>,
        dates: tutti_engine::BlockDates,
    ) -> crate::Result<tutti_engine::Engine<F>> {
        let mut exec = self.take_executor();
        exec.set_pool(self.render_pool("tutti-worker", self.render_worker_start.as_ref()));
        // Built here, on the control side, after the pool sized the
        // executor: the engine moves to the audio thread and holds none.
        let rt_log = tutti_graph::RtLogReaders::new(&mut exec);
        let engine = tutti_engine::Engine::new(transport, &mut self.editor, exec)?.dates(dates);
        let control = engine.output_control();
        control.support(device)?;
        control.state_device(device, topology)?;
        self.output = Some(Placed { control, device });
        self.publish_output_facts();
        self.rt_log = Some(rt_log);
        Ok(engine)
    }

    /// Re-prepare every node for `rate`, keeping the block and the quantum.
    /// With the executor local, both halves of the re-prepare run here, now;
    /// with it taken, the second half lands on a later collect (and commits
    /// are `Retry` until it does). A refusal is logged.
    ///
    /// For a detached graph, which has no device to take a rate from. A
    /// device's session re-rates with [`Session::restart`](crate::Session).
    pub fn set_sample_rate(&mut self, rate: SampleRate) {
        let current = *self.editor.prepare();
        if let Err(e) =
            self.editor
                .reprepare(prepare_for(rate, current.max_block(), current.quantum()))
        {
            tracing::error!("graph: re-prepare at {} Hz refused: {e}", rate.get());
            return;
        }
        self.pump_local();
    }

    /// Whether a re-prepare is between its two commits.
    pub(crate) fn is_repreparing(&self) -> bool {
        self.editor.is_repreparing()
    }

    /// Refuse, before a device restart stops anything, a re-prepare to
    /// `max_block` this graph could not start: past the engine's block
    /// capacity (the editor's limits, set by `Engine::new`), with one
    /// already between its halves, or on a poisoned editor.
    #[cfg(feature = "device")]
    pub(crate) fn check_reprepare(&self, max_block: Option<MaxBlock>) -> Result<(), CommitError> {
        if let Some(cause) = self.editor.poison_cause() {
            return Err(CommitError::Poisoned {
                cause: cause.to_owned(),
            });
        }
        if self.editor.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        let limit = self.editor.limits().max_block;
        match max_block {
            Some(b) if b.get() > limit => Err(CommitError::BlockTooLong {
                max_block: b.get(),
                limit,
            }),
            _ => Ok(()),
        }
    }

    /// Re-prepare every node for `rate`, the device's callback `quantum` and,
    /// if given, `max_block` (else the block it has): the first half of
    /// `Editor::reprepare`, sent. A no-op when nothing moves.
    #[cfg(feature = "device")]
    pub(crate) fn reprepare(
        &mut self,
        rate: SampleRate,
        max_block: Option<MaxBlock>,
        quantum: Option<MaxBlock>,
    ) -> Result<(), CommitError> {
        let current = *self.editor.prepare();
        let prepare = prepare_for(rate, max_block.unwrap_or(current.max_block()), quantum);
        if prepare == current {
            return Ok(());
        }
        self.editor.reprepare(prepare)?;
        self.pump_local();
        Ok(())
    }

    /// Apply whatever the editor sent to a local executor, and collect what
    /// comes back, until nothing is in flight. A no-op once the executor is
    /// taken. Bounded: a re-prepare is two round trips, anything else one.
    fn pump_local(&mut self) {
        let Some(local) = &mut self.local else {
            return;
        };
        for _ in 0..4 {
            if self.editor.in_flight() == 0 {
                break;
            }
            local.exec.apply_pending();
            let retired = self.editor.collect();
            Self::bury(&mut self.retiring, &retired);
        }
    }

    // --- Nodes ---

    /// Adds `node` to the graph, unwired, and returns its key: **the one way a
    /// node enters**. Its fork source goes to the editor, so an export forks it
    /// as its type says; it is prepared at the graph's rate. What the node says of its controls is kept
    /// ([`SessionNode`]): its params are addressed
    /// ([`SessionNode::params`], so a [`ParamTarget`](crate::ParamTarget) write and control-rate
    /// modulation find them), its reclaimer is swept by the session's
    /// [`Reclaim`](crate::Phase::Reclaim) phase ([`SessionNode::reclaimer`]), and
    /// it reads the session's tempo map ([`SessionNode::bind_tempo`]). A node
    /// that is only an [`IntoNode`] does not compile here: a host's own is
    /// wrapped as [`ForkByClone`](tutti_graph::ForkByClone) or
    /// [`Unforkable`](tutti_graph::Unforkable), or is a
    /// [`Params`](tutti_graph::Params), or implements [`SessionNode`]. Writing
    /// the node is `tutti_graph`'s crate guide ("Writing a node").
    ///
    /// The graph keeps the node's controls for the node's life and an app never
    /// holds them: every value of the node is a target on its key.
    ///
    /// ```compile_fail
    /// # use tutti_session::{Graph, SessionNode};
    /// fn cells<N: SessionNode<f32>>(graph: &mut Graph, node: N) {
    ///     let (key, controls) = graph.add(node);
    /// }
    /// ```
    pub fn add<N: SessionNode<F>>(&mut self, node: N) -> Key<N> {
        let (key, controls) = self.place(node.bind_tempo(&self.tempo));
        self.address::<N>(key.erase(), controls);
        key
    }

    /// The controls [`add`](Self::add) kept for the node at `key`; `None` when
    /// the node is gone.
    pub(crate) fn controls<N: SessionNode<F>>(&self, key: &Key<N>) -> Option<&N::Controls> {
        self.nodes
            .get(&key.erase())?
            .controls
            .as_ref()?
            .controls
            .downcast_ref()
    }

    /// The sampler handle of the node at `key`, until its commands become
    /// targets ([`Handled`](crate::Handled) names the nodes). `None` when the
    /// node is gone.
    ///
    /// The graph owns the handle for the node's life, so a handle's last drop
    /// (a streamed voice's release) waits until the node has left the render:
    /// after its removal's fade-out, or a replacement's crossfade.
    #[cfg(feature = "sampler")]
    pub fn handle<N: crate::Handled<F>>(&self, key: &Key<N>) -> Option<&N::Controls> {
        self.controls(key)
    }

    /// Band `band`'s largest gain reduction in the last block, the meter of
    /// the multiband compressor at `key`: [`Db::UNITY`](tutti_units::Db::UNITY)
    /// for no reduction. `None` when the node is gone or `band` is past its
    /// last band.
    pub fn gain_reduction<N>(&self, key: &Key<N>, band: usize) -> Option<tutti_units::Db>
    where
        N: SessionNode<F, Controls = tutti_nodes::MultibandControls>,
    {
        let controls = self.controls(key)?;
        (band < controls.bands()).then(|| controls.gain_reduction_db(band))
    }

    /// Puts `node` in the editor under a fresh key, with nothing addressed and
    /// nothing to sweep: [`add`](Self::add)'s first half, for the nodes that
    /// are not a [`SessionNode`] (a bound plugin).
    pub(crate) fn place<N: IntoNode<F>>(&mut self, node: N) -> (Key<N>, N::Controls) {
        let id = NodeKey::fresh();
        // A fresh key has no wiring recorded, so nothing is unwired.
        let controls = self.editor.insert(id, N::kind(), node).controls;
        self.nodes.insert(
            id,
            Entry {
                reclaimer: None,
                controls: None,
            },
        );
        self.note_pass(id);
        self.edited = true;
        (Key::new(id), controls)
    }
}

impl<F: Float> Graph<F> {
    /// Adds an inlet: audio a thread of your own pushes into the graph. The
    /// ring comes from the producer's layer ([`InletRing::pair`], which
    /// states the width, the capacity and the rate its producer fills it at);
    /// an [`InletNode`] (unwired; declare what it feeds like any node) reads
    /// it, and the writer [`InletRing::pair`] returned is the producer's end:
    /// push whole frames with
    /// [`push_interleaved`](tutti_graph::TapWriter::push_interleaved) from any one thread.
    /// The mirror of [`Taps::open_ports`], which copies the other way.
    ///
    /// An inlet **does not resample**, and its clock is not the graph's: the
    /// producer must run at the graph's rate, checked here, once. After that
    /// two clocks drift however slowly, and
    /// nothing corrects it: a producer ahead of the graph finds the ring full
    /// and its writer refuses whole frames, counting them; one behind leaves
    /// the ring empty and the node writes silence for the frames it lacks.
    /// Neither side blocks. A ring of a few blocks keeps both rare, at that
    /// much latency. A re-prepare to another rate later is not checked.
    ///
    /// The node is unforkable: an export of a graph that holds it is refused,
    /// naming its key.
    ///
    /// # Errors
    ///
    /// [`InletError::RateMismatch`] when the ring's rate is not the graph's;
    /// nothing is opened.
    pub fn add_inlet(&mut self, ring: InletRing<F>) -> Result<Key<InletNode<F>>, InletError> {
        let node = self.inlet_node(ring)?;
        Ok(self.add(node))
    }

    /// The node for `ring`, once its rate is the graph's: what
    /// [`add_inlet`](Self::add_inlet) adds and an input's reopening swaps in.
    pub(crate) fn inlet_node(&self, ring: InletRing<F>) -> Result<InletNode<F>, InletError> {
        let graph = self.editor.prepare().sample_rate();
        if ring.rate() != graph {
            return Err(InletError::RateMismatch {
                producer: ring.rate(),
                graph,
            });
        }
        Ok(InletNode::new(ring.into_reader()))
    }
}

impl<F: Float> Graph<F> {
    /// Opens an event inlet: MIDI a thread of your own pushes into the graph,
    /// each event on its frame. A ring of [`capacity`](EventCapacity) events is read by an
    /// [`EventInletNode`] (unwired; its one event output feeds what you
    /// connect it to, like any node), and the writer is the producer's end:
    /// [`push`](EventInletWriter::push) at a [`Stamp`](tutti_graph::Stamp) from one
    /// thread. The inverse of [`Taps::open_event`]. `capacity`
    /// is the ring, the waiting list and the port's declared event capacity
    /// (a live input's is `tutti_midi_runtime::LIVE_INLET_CAPACITY`, 256).
    ///
    /// Nothing blocks and what does not fit is counted in the writer's
    /// [`Losses`](tutti_graph::Losses) (`EventInletWriter::counts`, then
    /// `losses()`: `lost`, `evicted`, `refused`, and `late` for what was found
    /// past). A producer
    /// has no rate to match: an event names a frame, or a
    /// [`Moment`](tutti_units::Moment) that carries its rate, so there is
    /// nothing to refuse and nothing resampled.
    ///
    /// The fork is **silent**: an export of a graph that holds the inlet
    /// renders it with no events, as an offline render has none.
    ///
    /// Several producers are several inlets, merged by the graph's event
    /// fan-in. A handle that must be `Clone` shares the writer behind a lock of
    /// its own, taken on control threads only.
    pub fn open_event_inlet(
        &mut self,
        capacity: EventCapacity,
    ) -> (Key<EventInletNode>, EventInletWriter) {
        let (node, writer) = EventInletNode::pair(capacity);
        (self.add(node), writer)
    }

    /// The table the node's params are addressed in: the session's value
    /// state shares it.
    pub(crate) fn value_table(&self) -> Arc<ValueTable> {
        self.values.clone()
    }

    /// Keeps what a [`SessionNode`] says of its controls under `node`: its
    /// params, its slots and its reclaimer.
    fn address<N: SessionNode<F>>(&mut self, node: NodeKey, controls: N::Controls) {
        let held = Held {
            controls: Box::new(controls),
            addressing: Self::addressing::<N>,
        };
        if let Some(entry) = self.nodes.get_mut(&node) {
            entry.reclaimer = (held.addressing)(&self.values, node, held.controls.as_ref());
            entry.controls = Some(held);
        }
    }

    /// [`address`](Self::address)'s step for the controls of an `N`.
    fn addressing<N: SessionNode<F>>(
        values: &ValueTable,
        node: NodeKey,
        controls: &(dyn Any + Send),
    ) -> Option<crate::Reclaimer> {
        let controls = controls
            .downcast_ref::<N::Controls>()
            .expect("controls are held with the addressing of their own type");
        if let Some(set) = N::params(controls) {
            values.address(node, std::any::TypeId::of::<N>(), set);
        }
        values.provide_node(N::slots(node, controls));
        N::reclaimer(controls).into_reclaimer()
    }
}

impl Graph {
    /// Inserts a hosted plugin: bound (`PluginClient::bind`, its typestate
    /// transition to a graph node, with a reader of the session's tempo map,
    /// so the plugin is told the project's tempo) and inserted as the `Node` it then is,
    /// which hands the editor its own fork source (a fork by state transfer)
    /// and declares its latency in its `Shape`. Capture its controls first:
    /// the plugin is out of reach once inserted.
    ///
    /// The session ticks the plugin's rebinder at each update, as a loaded
    /// plugin's: when the plugin changes its audio ports, an out-of-process one
    /// restarts into a new slab and the node at this key is replaced by one
    /// over the new ports.
    #[cfg(feature = "plugin")]
    pub fn add_plugin(&mut self, client: Box<tutti_plugin::PluginClient>) -> Key<crate::AnyPlugin> {
        let (key, rebinder) = self.add_plugin_with_rebinder(client);
        if let Some(hosted) = self.plugins.get_mut(&key.erase()) {
            hosted.set_rebinder(rebinder);
        }
        key
    }

    /// [`add_plugin`](Self::add_plugin), and the builder of the node
    /// that replaces this one when the plugin changes its audio ports.
    #[cfg(feature = "plugin")]
    pub(crate) fn add_plugin_with_rebinder(
        &mut self,
        client: Box<tutti_plugin::PluginClient>,
    ) -> (Key<crate::AnyPlugin>, tutti_plugin::catalog::PluginRebinder) {
        let node = NodeKey::fresh();
        let hosted = crate::plugin::HostedPlugin::served(
            client.handle(),
            client.plugin().clone(),
            client.controls(),
            client.answers(),
        );
        let tempo = self.tempo.clone();
        let bound = client.bind(tempo);
        let rebinder = tutti_plugin::catalog::PluginRebinder::out_of_process(&bound);
        // A fresh key has no wiring recorded, so nothing is unwired.
        let _controls = self.editor.insert(node, PLUGIN_KIND, bound).controls;
        self.nodes.insert(
            node,
            Entry {
                reclaimer: None,
                controls: None,
            },
        );
        self.note_pass(node);
        self.plugins.insert(node, hosted);
        self.provide_plugin(node);
        self.write_own_bypass(node);
        self.edited = true;
        (Key::new(node), rebinder)
    }

    /// Swaps the plugin at `node` for the replacement's, as
    /// [`replace`](Self::replace) swaps any node: crossfading when the running
    /// node's shape fits the bound plugin's, else a plain swap at the same
    /// key. The incoming plugin's rebinder replaces the old one, and the
    /// plugin's record is carried over to it. Capture the plugin's controls
    /// first: it is out of reach once handed over.
    ///
    /// It always lands, as [`replace`](Self::replace)'s does: now, or on the
    /// first update after a re-prepare resumes
    /// ([`SessionEvent::Replaced`](crate::SessionEvent::Replaced)).
    ///
    /// Unlike [`replace`](Self::replace), the key keeps naming the outgoing
    /// plugin while the swap waits: writes through it reach the outgoing
    /// plugin and `handle` answers for it, because the incoming client is
    /// bound only when the swap lands and a bound plugin cannot be unbound.
    ///
    /// # Errors
    ///
    /// [`ReplaceRefused`] on a poisoned graph or a `node` not in it.
    #[cfg(feature = "plugin")]
    pub fn replace_plugin(
        &mut self,
        node: impl Into<NodeKey>,
        replacement: Replacement<Box<tutti_plugin::PluginClient>>,
    ) -> Result<Key<crate::AnyPlugin>, ReplaceRefused> {
        let node = node.into();
        let Replacement {
            incoming: client,
            fade,
        } = replacement;
        if self.parked.iter().any(|p| p.node == node) {
            self.park(node, Box::new(PluginSwap(client)), fade, None);
            return Ok(Key::new(node));
        }
        match self.replace_plugin_now(node, client, fade) {
            Ok(()) => {}
            Err(SwapRefused::Busy(c)) => self.park(node, Box::new(PluginSwap(c)), fade, None),
            Err(SwapRefused::Failed(refused)) => return Err(refused),
        }
        Ok(Key::new(node))
    }

    /// [`replace_plugin`](Self::replace_plugin) while the graph is not
    /// re-preparing, handing `client` back unbound while it is.
    #[cfg(feature = "plugin")]
    fn replace_plugin_now(
        &mut self,
        node: NodeKey,
        client: Box<tutti_plugin::PluginClient>,
        fade: Option<Fade>,
    ) -> Result<(), SwapRefused<Box<tutti_plugin::PluginClient>>> {
        let rebinder = self.replace_plugin_with_rebinder(node, client, fade, OnShape::Refuse)?;
        if let Some(hosted) = self.plugins.get_mut(&node) {
            hosted.set_rebinder(rebinder);
        }
        Ok(())
    }

    /// [`replace_plugin`](Self::replace_plugin), and the builder of the node
    /// that replaces the incoming one when the plugin changes its audio ports.
    ///
    /// # Errors
    ///
    /// [`SwapRefused::Busy`] while the graph re-prepares, handing `client`
    /// back unbound; [`SwapRefused::Failed`] as [`replace_plugin`](Self::replace_plugin).
    #[cfg(feature = "plugin")]
    pub(crate) fn replace_plugin_with_rebinder(
        &mut self,
        node: impl Into<NodeKey>,
        client: Box<tutti_plugin::PluginClient>,
        fade: Option<Fade>,
        on_shape: OnShape,
    ) -> Result<tutti_plugin::catalog::PluginRebinder, SwapRefused<Box<tutti_plugin::PluginClient>>>
    {
        let node = node.into();
        // Asked here, before binding: `replace` hands back what it refuses,
        // and a bound plugin cannot be unbound.
        if let Some(cause) = self.editor.poison_cause() {
            return Err(SwapRefused::Failed(ReplaceRefused::Poisoned(
                cause.to_owned(),
            )));
        }
        if !self.nodes.contains_key(&node) {
            return Err(SwapRefused::Failed(ReplaceRefused::NoSuchNode(node)));
        }
        if self.editor.is_repreparing() {
            return Err(SwapRefused::Busy(client));
        }
        let handle = client.handle();
        let controls = client.controls();
        let plugin = client.plugin().clone();
        let answers = client.answers();
        let tempo = self.tempo.clone();
        let bound = client.bind(tempo);
        let rebinder = tutti_plugin::catalog::PluginRebinder::out_of_process(&bound);
        match self.swap_unit(node, bound, fade, on_shape) {
            Ok(_controls) => {
                // The record is carried over to the incoming plugin (unlike
                // `replace`, which takes it out): its handle is what the
                // health watch and the render mode talk to now, its controls
                // what the latency poll reads; nothing of the outgoing
                // plugin's state survives (`HostedPlugin::replace`).
                match self.plugins.get_mut(&node) {
                    Some(hosted) => {
                        let outgoing =
                            hosted.replace(handle, controls, plugin, Some(answers.clone()));
                        outgoing.retire().settle(node, &mut self.graveyard);
                    }
                    None => {
                        self.plugins.insert(
                            node,
                            crate::plugin::HostedPlugin::served(handle, plugin, controls, answers),
                        );
                    }
                }
                self.provide_plugin(node);
                self.write_own_bypass(node);
                Ok(rebinder)
            }
            Err(SwapRefused::Failed(refused)) => Err(SwapRefused::Failed(refused)),
            Err(SwapRefused::Busy(_)) => unreachable!("checked above"),
        }
    }

    /// [`replace_plugin`](Self::replace_plugin) for a plugin hosted in process:
    /// bound and swapped in at `node`, its record carried over to it.
    ///
    /// # Errors
    ///
    /// As [`replace_plugin_with_rebinder`](Self::replace_plugin_with_rebinder),
    /// handing `plugin` back while the graph re-prepares.
    #[cfg(feature = "plugin")]
    pub(crate) fn replace_hosted_plugin(
        &mut self,
        node: NodeKey,
        plugin: Box<tutti_plugin::catalog::Plugin>,
        fade: Option<Fade>,
        on_shape: OnShape,
    ) -> Result<
        tutti_plugin::catalog::PluginRebinder,
        SwapRefused<Box<tutti_plugin::catalog::Plugin>>,
    > {
        if let Some(cause) = self.editor.poison_cause() {
            return Err(SwapRefused::Failed(ReplaceRefused::Poisoned(
                cause.to_owned(),
            )));
        }
        if !self.nodes.contains_key(&node) {
            return Err(SwapRefused::Failed(ReplaceRefused::NoSuchNode(node)));
        }
        if self.editor.is_repreparing() {
            return Err(SwapRefused::Busy(plugin));
        }
        let handle = plugin.handle().clone();
        let controls = plugin.node_controls();
        let plugin_ref = plugin.plugin().clone();
        let (bound, rebinder) = (*plugin).bind_with_rebinder(self.tempo.clone());
        match self.swap_unit(node, bound, fade, on_shape) {
            Ok(_) => {
                match self.plugins.get_mut(&node) {
                    Some(hosted) => {
                        let outgoing = hosted.replace_hosted(handle, controls, plugin_ref);
                        outgoing.retire().settle(node, &mut self.graveyard);
                    }
                    None => {
                        self.plugins.insert(
                            node,
                            crate::plugin::HostedPlugin::new(handle, plugin_ref, controls),
                        );
                    }
                }
                self.provide_plugin(node);
                Ok(rebinder)
            }
            Err(SwapRefused::Failed(refused)) => Err(SwapRefused::Failed(refused)),
            Err(SwapRefused::Busy(_)) => unreachable!("checked above"),
        }
    }

    /// Swaps the node of the plugin at `node` for `bound`, the node
    /// [`PluginRebinder::prepare`](tutti_plugin::catalog::PluginRebinder::prepare)
    /// built over the same plugin with the ports it has now: the key, and the
    /// wiring to the ports the new node keeps, stay, and the wiring to the ports
    /// it lacks is dropped and reported
    /// ([`SessionEvent::Unwired`](crate::SessionEvent::Unwired)). The plugin's
    /// record stays.
    ///
    /// # Errors
    ///
    /// [`SwapRefused::Busy`] while the graph re-prepares, handing `bound`
    /// back unplaced; [`SwapRefused::Failed`] on a poisoned graph, a `node`
    /// not in it, or an editor that refuses the swap.
    #[cfg(feature = "plugin")]
    pub(crate) fn replace_rebound(
        &mut self,
        node: NodeKey,
        bound: tutti_plugin::catalog::BoundPlugin,
    ) -> Result<(), SwapRefused<tutti_plugin::catalog::BoundPlugin>> {
        self.swap_unit(node, bound, None, OnShape::Cut).map(|_| ())
    }

    /// Puts the silent stand-in of a plugin that is gone at `node`
    /// ([`Lost`](crate::plugin::Lost)), the plugin's record kept: the node keeps
    /// its key, its ports, its wiring and its latency, renders silence and has
    /// no tail, and the plugin's node is retired to the control thread. Returns
    /// whether the swap was made: not while the graph re-prepares. A swap parked
    /// for the same node (a replacement waiting for the re-prepare) lands after the
    /// stand-in and wins.
    ///
    /// # Errors
    ///
    /// A poisoned graph, or a `node` not in it.
    #[cfg(feature = "plugin")]
    pub(crate) fn silence_plugin(&mut self, node: NodeKey) -> Result<bool, ReplaceRefused> {
        if let Some(cause) = self.editor.poison_cause() {
            return Err(ReplaceRefused::Poisoned(cause.to_owned()));
        }
        let Some(shape) = self.shape(node).filter(|_| self.nodes.contains_key(&node)) else {
            return Err(ReplaceRefused::NoSuchNode(node));
        };
        if self.editor.is_repreparing() {
            return Ok(false);
        }
        let stand_in = tutti_graph::ForkByClone(crate::plugin::Lost::of(shape));
        match self.swap_unit(node, stand_in, None, OnShape::Cut) {
            Ok(_) => Ok(true),
            Err(SwapRefused::Failed(refused)) => Err(refused),
            // A re-prepare is `swap_unit`'s only cause for `Busy`, and it was
            // checked above on this thread.
            Err(SwapRefused::Busy(_)) => unreachable!("checked above"),
        }
    }
}

impl<F: Float> Graph<F> {
    /// The control-thread half of a copy of the master for an offline render
    /// at `rate`, sharing no state with this graph: `Editor::fork_gather`
    /// with `ForkMode::Offline`, prepared as a bounce is
    /// (`RenderGraph::prepare`). [`PendingExport::build`] makes the rest on a
    /// worker.
    ///
    /// The fork renders on the session's render pool unless `serial`, and
    /// places the master's channels by `root`: the session's declared root, the
    /// engine's order for the width, or the bounce's own topology.
    ///
    /// # Errors
    ///
    /// What the gather refuses (a graph with no outputs, a node that cannot be
    /// forked: [`tutti_export::RenderError::NotForkable`]).
    #[cfg(feature = "export")]
    pub(crate) fn gather_for_bounce(
        &self,
        rate: SampleRate,
        serial: bool,
        root: crate::bounce::RootChoice,
    ) -> tutti_export::Result<PendingExport<F>> {
        let root = match root {
            crate::bounce::RootChoice::Session => self.root.clone(),
            crate::bounce::RootChoice::Engine => None,
            crate::bounce::RootChoice::Placed(topology) => Some(topology),
        };
        // Its own pool, not the live engine's: the live callback would find a
        // shared one busy with the bounce's job and render alone. No start
        // hook: a bounce's threads are not the callback's, and promoting
        // them would take cores from it.
        let pool = if serial {
            None
        } else {
            self.render_pool("tutti-export", None)
        };
        PendingExport::gather(
            &self.editor,
            tutti_graph::ForkTarget::Master,
            rate,
            pool,
            root,
        )
    }
}

impl<F: Float> Graph<F> {
    /// Emits what the nodes logged on the audio thread as `tracing` events
    /// (`rt_log.rs`), naming each node by its kind in the current graph; a
    /// node removed since it logged is named by its id alone. Control
    /// thread: the session's [`Reclaim`](crate::Phase::Reclaim) phase.
    pub(crate) fn drain_rt_log(&self) {
        if let Some(readers) = &self.rt_log {
            let nodes = &self.editor.spec().topology.nodes;
            crate::rt_log::drain(readers, |key| nodes.get(&key).map(|n| n.kind.as_str()));
        }
    }

    /// Runs every node's reclaimer ([`SessionNode::reclaimer`], kept by
    /// [`add`](Self::add) and [`replace`](Self::replace), dropped with
    /// the node), the editor's param range cells ([`Editor::reclaim`]) and
    /// (`plugin`) every hosted plugin's meter cell,
    /// so a cell that stopped publishing still has what it retired freed.
    /// Returns how many retired values are still pinned. Control thread:
    /// the session's [`Reclaim`](crate::Phase::Reclaim) phase.
    pub(crate) fn reclaim(&self) -> usize {
        let pinned: usize = self
            .nodes
            .values()
            .filter_map(|entry| entry.reclaimer.as_ref())
            .map(crate::Reclaimer::reclaim)
            .sum();
        // The editor's range cells: a range a control moved or a commit
        // rewrote retires the one a block may still hold.
        let pinned = pinned + self.editor.reclaim();
        // The engine's output folds (offline or on a device): the declaration
        // retires a set, and a block may hold it when it does. Not observable
        // from outside the engine (a held loan is the audio thread's); the
        // cell's own behaviour is pinned in `tutti-engine`
        // (`a_replaced_fold_set_is_reclaimed_after_the_block`).
        let pinned = pinned
            + self
                .output
                .as_ref()
                .map_or(0, |placed| placed.control.reclaim());
        #[cfg(feature = "plugin")]
        let pinned = pinned
            + self
                .plugins
                .values()
                .map(|hosted| hosted.node.meter().reclaim())
                .sum::<usize>();
        pinned
    }

    /// Swaps the node behind `node` for the replacement's under its fade, and
    /// hands back the incoming node's key. The incoming node goes in as
    /// [`add`](Self::add) puts one in: its params are addressed and its
    /// reclaimer kept, and the outgoing node's go with it.
    ///
    /// The fade is the [`Replacement`]'s: the declick unless it set one. A
    /// crossfade needs the incoming node to have the running node's shape
    /// (its ports, buses and latency): over another shape the call is refused
    /// with [`ReplaceRefused::ShapeChanged`] and nothing changes. **The
    /// default declick is not zero, so a [`Replacement::new`] across a shape
    /// change is refused**: a caller who accepts the cut passes
    /// [`Replacement::fade`] with a zero-length [`Fade`], and the node lands
    /// as a plain swap on the next commit. Nothing to fade from (the node
    /// was added since the last commit) is a plain swap too.
    ///
    /// It always lands. While the graph re-prepares (a sample-rate or
    /// block-size change between its two commits) the node waits, and the
    /// first update that finds the graph resumed swaps it in and reports
    /// [`SessionEvent::Replaced`](crate::SessionEvent::Replaced), or
    /// [`SessionEvent::ReplaceFailed`](crate::SessionEvent::ReplaceFailed)
    /// when the shapes then turn out to differ under a fade. A replacement
    /// asked for while another of the same node waits waits behind it, so the
    /// later one still wins. [`remove`](Self::remove) drops a waiting
    /// replacement and reports it
    /// ([`SessionEvent::ReplaceDropped`](crate::SessionEvent::ReplaceDropped)).
    ///
    /// **The returned key names the incoming node from the call on**, also
    /// while it waits: its params are addressed at once, so a write through
    /// the key's targets reaches the incoming node and lands with it, and
    /// [`handle`](Self::handle) and the other readers of the node's controls
    /// answer for it. The outgoing node plays on, as it was, until the swap
    /// lands.
    ///
    /// (`plugin`) A hosted plugin replaced this way is no longer hosted: its
    /// record is dropped, so its health, snapshots and render mode stop
    /// (a replacement plugin goes in with
    /// `Graph::replace_plugin`, which carries the record).
    ///
    /// # Errors
    ///
    /// [`ReplaceRefused`] on a poisoned graph, a `node` not in it, or a fade
    /// over another shape, dropping the incoming node.
    pub fn replace<N>(
        &mut self,
        node: impl Into<NodeKey>,
        replacement: Replacement<N>,
    ) -> Result<Key<N>, ReplaceRefused>
    where
        N: SessionNode<F>,
        N::Controls: Send + 'static,
    {
        let node = node.into();
        let Replacement { incoming, fade } = replacement;
        if let Some(cause) = self.editor.poison_cause() {
            return Err(ReplaceRefused::Poisoned(cause.to_owned()));
        }
        if !self.nodes.contains_key(&node) {
            return Err(ReplaceRefused::NoSuchNode(node));
        }
        let NodeParts {
            node: unit,
            controls,
            fork,
        } = incoming.bind_tempo(&self.tempo).into_parts();
        let unit = Unit::<N, F> {
            parts: NodeParts {
                node: unit,
                controls: (),
                fork,
            },
            node: std::marker::PhantomData,
        };
        if self.parked.iter().any(|p| p.node == node) {
            self.park_typed::<N>(node, unit, controls, fade);
            return Ok(Key::new(node));
        }
        match self.place_unit(node, unit, fade, OnShape::Refuse) {
            Ok(()) => {
                self.retire_outgoing(node);
                self.drop_plugin_record(node);
                self.address::<N>(node, controls);
            }
            Err(SwapRefused::Busy(unit)) => self.park_typed::<N>(node, unit, controls, fade),
            Err(SwapRefused::Failed(refused)) => return Err(refused),
        }
        Ok(Key::new(node))
    }

    /// Parks `unit` behind a re-prepare, with its controls addressed under
    /// `node` now: the outgoing controls wait in the parked swap until it
    /// lands or is refused.
    fn park_typed<N: SessionNode<F>>(
        &mut self,
        node: NodeKey,
        unit: Unit<N, F>,
        controls: N::Controls,
        fade: Option<Fade>,
    ) {
        let outgoing = self.nodes.get_mut(&node).and_then(|e| e.controls.take());
        if let Some(entry) = self.nodes.get_mut(&node) {
            entry.reclaimer = None;
        }
        self.values.retire(node);
        self.address::<N>(node, controls);
        self.park(node, Box::new(Swap(unit)), fade, Some(Stash(outgoing)));
    }

    /// Registers the plugin hosted at `node` with the value table, as its
    /// record holds it now: the commit step every change of a plugin's handle
    /// or parameter read ends with.
    #[cfg(feature = "plugin")]
    pub(crate) fn provide_plugin(&self, node: NodeKey) {
        if let Some(hosted) = self.plugins.get(&node) {
            hosted.provide_values(node, &self.values);
        }
    }

    /// Takes the record of a plugin hosted at `node` out, as a swap that
    /// took leaves it: the node is no longer hosted.
    fn drop_plugin_record(&mut self, node: NodeKey) {
        #[cfg(feature = "plugin")]
        if let Some(record) = self.plugins.remove(&node) {
            record.retire().settle(node, &mut self.graveyard);
        }
        #[cfg(not(feature = "plugin"))]
        let _ = node;
    }

    /// [`replace`](Self::replace)'s swap of a plugin's node, leaving the
    /// plugin record alone: [`place_unit`](Self::place_unit) and the outgoing
    /// controls retired. A `fade` of `None` is the declick at the graph's
    /// rate.
    #[cfg(feature = "plugin")]
    fn swap_unit<N: IntoNode<F>>(
        &mut self,
        node: NodeKey,
        incoming: N,
        fade: Option<Fade>,
        on_shape: OnShape,
    ) -> Result<N::Controls, SwapRefused<N>> {
        let controls = self.place_unit(node, incoming, fade, on_shape)?;
        self.retire_outgoing(node);
        Ok(controls)
    }

    /// The editor's half of a swap: the unit placed at `node` over `fade`
    /// (`None` is the declick at the graph's rate), what it unwired noted and
    /// the next commit owed. The controls under the key are the caller's.
    fn place_unit<N: IntoNode<F>>(
        &mut self,
        node: NodeKey,
        incoming: N,
        fade: Option<Fade>,
        on_shape: OnShape,
    ) -> Result<N::Controls, SwapRefused<N>> {
        if let Some(cause) = self.editor.poison_cause() {
            return Err(SwapRefused::Failed(ReplaceRefused::Poisoned(
                cause.to_owned(),
            )));
        }
        // Before `Busy`: a node that is not there is refused at this call, not
        // parked for a retry that can only fail.
        if !self.nodes.contains_key(&node) {
            return Err(SwapRefused::Failed(ReplaceRefused::NoSuchNode(node)));
        }
        if self.editor.is_repreparing() {
            return Err(SwapRefused::Busy(incoming));
        }
        let rate = self.editor.prepare().sample_rate();
        let fade = fade.unwrap_or_else(|| Fade::seconds(tutti_graph::DECLICK, rate));
        let (controls, unwired) = match on_shape {
            OnShape::Refuse => {
                let placed = self
                    .editor
                    .replace_or_swap(node, incoming, fade)
                    .map_err(|e| SwapRefused::Failed(e.into()))?;
                (placed.controls, placed.unwired)
            }
            // The fade where the shape fits, else a cut: a recovered or rebuilt
            // plugin may report another latency, and no app asked for a fade
            // over a swap the session made itself.
            #[cfg(feature = "plugin")]
            OnShape::Cut => {
                let NodeParts {
                    node: unit,
                    controls,
                    fork,
                } = incoming.into_parts();
                let fits = self.editor.fade_fits(node, &unit.shape()) == Some(true);
                let unit = Unit::<N, F> {
                    parts: NodeParts {
                        node: unit,
                        controls: (),
                        fork,
                    },
                    node: std::marker::PhantomData,
                };
                let fade = if fits { fade } else { Fade::new(Samples::ZERO) };
                let placed = self
                    .editor
                    .replace_or_swap(node, unit, fade)
                    .map_err(|e| SwapRefused::Failed(e.into()))?;
                (controls, placed.unwired)
            }
        };
        self.note_unwired(node, &unwired);
        self.note_pass(node);
        self.sweep_gain_targets();
        self.edited = true;
        Ok(controls)
    }

    /// The outgoing node's controls at `node`, which a swap that took
    /// replaced: its values go, and the controls are held until the outgoing
    /// unit has left the render.
    fn retire_outgoing(&mut self, node: NodeKey) {
        if let Some(entry) = self.nodes.get_mut(&node) {
            self.values.retire(node);
            entry.reclaimer = None;
            if let Some(held) = entry.controls.take() {
                self.retiring.push((node, held.controls));
            }
        }
    }

    /// Drops the retiring controls of the nodes whose units the editor handed
    /// back: one, the oldest, per key in `retired` (a node of several units
    /// names its key once per unit).
    ///
    /// Two swaps at one key whose units both come back in one `retired` drop
    /// only the older box; the other is held until that key retires again or
    /// the graph drops.
    fn bury(retiring: &mut Vec<(NodeKey, Box<dyn Any + Send>)>, retired: &[NodeKey]) {
        let mut seen: Vec<NodeKey> = Vec::new();
        for &key in retired {
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            if let Some(at) = retiring.iter().position(|(k, _)| *k == key) {
                retiring.remove(at);
            }
        }
    }

    /// Records what a re-insert at `node` unwired, in the session's port
    /// types, for the update that commits it to report. A dropped edge no
    /// feed put there stops being one a feed owns.
    fn note_unwired(&mut self, node: NodeKey, unwired: &tutti_graph::Unwired) {
        if unwired.is_empty() {
            return;
        }
        let mut edges: Vec<Edge> = Vec::new();
        for id in &unwired.edges {
            // A feedback edge is reported as the pair of ports it joins.
            let source = match id.from {
                RawEdge::Direct(source) => source,
                RawEdge::Feedback(f) => PortSource::Node(f.from),
            };
            // Explicit silence is no wire an app made: nothing to report.
            let Some(from) = OutPort::of_source(source) else {
                continue;
            };
            let edge = Edge::new(from, InPort::of_sink(id.sink));
            if !edges.contains(&edge) {
                edges.push(edge);
            }
        }
        let mut event_edges: Vec<crate::EventEdge> = Vec::new();
        for (at, edge) in &unwired.event_edges {
            let (from, to) = (EventOutPort::of_raw(edge.from()), EventInPort::of_raw(*at));
            if at.port == 0
                && let Some(fed) = self.fed_edges.get_mut(&at.node)
            {
                fed.retain(|s| *s != from);
            }
            let edge = crate::EventEdge::new(from, to);
            if !event_edges.contains(&edge) {
                event_edges.push(edge);
            }
        }
        let mut bus_edges: Vec<(BusSource, BusSink)> = Vec::new();
        let named = unwired
            .bus_edges
            .iter()
            .map(|(to, feed)| (feed.source, BusSink::Node(*to)))
            .chain(
                unwired
                    .root_buses
                    .iter()
                    .map(|feed| (feed.source, BusSink::Root)),
            );
        for wire in named {
            if !bus_edges.contains(&wire) {
                bus_edges.push(wire);
            }
        }
        if edges.is_empty() && event_edges.is_empty() && bus_edges.is_empty() {
            return;
        }
        match self.unwired.iter_mut().find(|u| u.node == node) {
            Some(held) => {
                held.edges.extend(edges);
                held.event_edges.extend(event_edges);
                held.bus_edges.extend(bus_edges);
            }
            None => self.unwired.push(Unwiring {
                node,
                edges,
                event_edges,
                bus_edges,
            }),
        }
    }

    /// Folds a commit's rewired nodes into the pending report: a node keeps
    /// its first `was`, takes the last `now`, and drops out when the commits
    /// together left its wiring as it was.
    fn note_rewired(&mut self, rewired: Vec<tutti_graph::Rewired>) {
        // Only a node with a bus besides its main one has a bus to activate;
        // the rest read the answer per block and need no notice.
        let bused = |node| {
            self.shape(node)
                .is_some_and(|s| s.buses_in().len() > 1 || s.buses_out().len() > 1)
        };
        let rewired: Vec<_> = rewired.into_iter().filter(|r| bused(r.node)).collect();
        for r in rewired {
            match self.rewired.iter().position(|p| p.node == r.node) {
                Some(i) if self.rewired[i].was == r.now => {
                    self.rewired.remove(i);
                }
                Some(i) => self.rewired[i].now = r.now,
                None => self.rewired.push(r),
            }
        }
        // A node a commit removed has no wiring to report.
        let nodes = &self.nodes;
        self.rewired.retain(|r| nodes.contains_key(&r.node));
    }

    /// The nodes whose wiring the commits since the last call changed.
    pub(crate) fn take_rewired(&mut self) -> Vec<tutti_graph::Rewired> {
        std::mem::take(&mut self.rewired)
    }

    /// What the re-inserts since the last call unwired, once the commit that
    /// carries them is sent: nothing while an edit still waits.
    pub(crate) fn take_unwired(&mut self) -> Vec<Unwiring> {
        if self.edited {
            return Vec::new();
        }
        std::mem::take(&mut self.unwired)
    }

    fn park(
        &mut self,
        node: NodeKey,
        swap: Box<dyn ErasedSwap<F>>,
        fade: Option<Fade>,
        stash: Option<Stash>,
    ) {
        self.parked.push_back(Parked {
            node,
            swap,
            fade,
            stash,
        });
    }

    /// Try every waiting replacement again, in request order: one still
    /// `Busy` goes back on the queue, in order.
    pub(crate) fn retry_replacements(&mut self) -> Vec<Retried> {
        let mut out = Vec::new();
        let mut pending = std::mem::take(&mut self.parked);
        while let Some(parked) = pending.pop_front() {
            let (node, fade) = (parked.node, parked.fade);
            if self.parked.iter().any(|p| p.node == node) {
                // An earlier one of the same node is still waiting.
                self.parked.push_back(parked);
                continue;
            }
            let Parked { swap, stash, .. } = parked;
            match swap.swap(self, node, fade) {
                Ok(()) => {
                    if let Some(Stash(Some(outgoing))) = stash {
                        self.retiring.push((node, outgoing.controls));
                    }
                    out.push(Retried::Landed { node });
                }
                Err(SwapRefused::Busy(swap)) => self.parked.push_back(Parked {
                    node,
                    swap,
                    fade,
                    stash,
                }),
                Err(SwapRefused::Failed(refused)) => {
                    tracing::error!("graph: replacement of {node:?} refused: {refused}");
                    if let Some(stash) = stash {
                        self.give_back(node, stash, &mut pending);
                    }
                    out.push(Retried::Failed { node, refused });
                }
            }
        }
        out
    }

    /// A parked replacement of `node` that failed: its incoming node is
    /// dropped, and the controls it stashed go to the next waiting one of the
    /// node (in `pending`, the ones this pass has yet to try, or parked again)
    /// or, with none, back under the key.
    fn give_back(&mut self, node: NodeKey, stash: Stash, pending: &mut VecDeque<Parked<F>>) {
        if let Some(next) = pending
            .iter_mut()
            .chain(self.parked.iter_mut())
            .filter(|p| p.node == node)
            .find_map(|p| p.stash.as_mut())
        {
            *next = stash;
            return;
        }
        let Some(entry) = self.nodes.get_mut(&node) else {
            return;
        };
        self.values.retire(node);
        entry.controls = stash.0;
        entry.reclaimer = entry
            .controls
            .as_ref()
            .and_then(|held| (held.addressing)(&self.values, node, held.controls.as_ref()));
    }

    /// How many event inputs `node` declares.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn node_event_inputs(&self, node: impl Into<NodeKey>) -> Result<usize, NoSuchNode> {
        self.shape_of(node.into()).map(|s| usize::from(s.event_in))
    }

    /// Whether `port` names an event output the graph has.
    fn check_event_out(&self, port: EventOutPort) -> Result<(), ConnectError> {
        if !self.contains(port.node()) {
            return Err(ConnectError::NoSuchNode(port.node()));
        }
        let fits = port
            .raw()
            .is_some_and(|raw| self.shape(raw.node).is_some_and(|s| raw.port < s.event_out));
        fits.then_some(()).ok_or(ConnectError::NoSuchEventOut(port))
    }

    /// Whether `port` names an event input the graph has.
    pub(crate) fn check_event_in(&self, port: EventInPort) -> Result<(), ConnectError> {
        if !self.contains(port.node()) {
            return Err(ConnectError::NoSuchNode(port.node()));
        }
        let fits = port
            .raw()
            .is_some_and(|raw| self.shape(raw.node).is_some_and(|s| raw.port < s.event_in));
        fits.then_some(()).ok_or(ConnectError::NoSuchEventIn(port))
    }

    pub(crate) fn check_event_wire(
        &self,
        from: EventOutPort,
        to: EventInPort,
    ) -> Result<(), ConnectError> {
        self.check_event_out(from)?;
        self.check_event_in(to)?;
        if from.node() == to.node() {
            return Err(ConnectError::Itself(from.node()));
        }
        Ok(())
    }

    /// Adds the event edge `from -> to`: whether it was new. Events are
    /// never faded. An edge a feed ([`set_feed`](Self::set_feed)) put there
    /// becomes the caller's: it stays when the feed drops it.
    pub(crate) fn add_event_edge(
        &mut self,
        from: EventOutPort,
        to: EventInPort,
    ) -> Result<bool, ConnectError> {
        self.check_event_wire(from, to)?;
        let (raw_from, raw_to) = (
            from.raw().expect("checked: the port fits"),
            to.raw().expect("checked: the port fits"),
        );
        let held = self
            .editor
            .spec()
            .events
            .get(&raw_to)
            .is_some_and(|edges| edges.iter().any(|e| e.from() == raw_from));
        if raw_to.port == 0
            && let Some(ours) = self.fed_edges.get_mut(&to.node())
        {
            ours.retain(|s| *s != from);
            if ours.is_empty() {
                self.fed_edges.remove(&to.node());
            }
        }
        if held {
            return Ok(false);
        }
        self.editor
            .spec_mut()
            .connect_events(raw_to, EventEdge::Direct(raw_from));
        self.edited = true;
        Ok(true)
    }

    /// Takes the event edge `from -> to` out: whether it was there.
    pub(crate) fn remove_event_edge(&mut self, from: EventOutPort, to: EventInPort) -> bool {
        let (Some(raw_from), Some(raw_to)) = (from.raw(), to.raw()) else {
            return false;
        };
        let removed = self.editor.spec_mut().disconnect_events(raw_to, raw_from);
        if removed {
            if let Some(ours) = self.fed_edges.get_mut(&to.node()) {
                ours.retain(|s| *s != from);
            }
            self.edited = true;
        }
        removed
    }

    /// Feeds `to` from exactly `sources` (fan-in merges them by offset; the
    /// graph orders ties by source), beside what the feeds
    /// ([`set_feed`](Self::set_feed)) feed it on input 0: the feeds are
    /// partial, as audio's declarations are, and own only their own
    /// sources. Touches nothing when that is what it already holds. Event
    /// edges from a node the graph did not add through
    /// [`add`](Self::add) (a modulation lane on a plugin's event
    /// input) are not the caller's, and stay.
    pub(crate) fn set_event_sources_of(
        &mut self,
        to: EventInPort,
        sources: &[EventOutPort],
    ) -> Result<(), ConnectError> {
        self.check_event_in(to)?;
        for &from in sources {
            self.check_event_wire(from, to)?;
        }
        let sink = to.node();
        let port = u16::try_from(to.index()).expect("checked: the port fits");
        if port != 0 {
            self.write_event_sources(sink, port, sources);
            return Ok(());
        }
        let fed = self.fed_in_graph(sink);
        let theirs: Vec<EventOutPort> = fed.into_iter().filter(|s| !sources.contains(s)).collect();
        let mut all = sources.to_vec();
        all.extend_from_slice(&theirs);
        self.set_fed_edges(sink, theirs);
        self.write_event_sources(sink, 0, &all);
        Ok(())
    }

    /// Takes what the caller wired out of `to`, the feeds' and a lane's
    /// edges staying: whether anything went.
    pub(crate) fn disconnect_all_events(&mut self, to: EventInPort) -> bool {
        let ours = if to.index() == 0 {
            self.fed_edges.get(&to.node()).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        let hand = self
            .event_sources_of(to)
            .into_iter()
            .any(|s| self.nodes.contains_key(&s.node()) && !ours.contains(&s));
        hand && self.set_event_sources_of(to, &[]).is_ok()
    }

    /// Feeds `sink`'s event input `port` from exactly `sources`, but for
    /// edges from nodes the graph did not insert.
    fn write_event_sources(&mut self, sink: NodeKey, port: u16, sources: &[EventOutPort]) {
        let at = EventIn { node: sink, port };
        let mut want: Vec<EventOut> = sources.iter().filter_map(|s| s.raw()).collect();
        want.sort();
        want.dedup();
        let nodes = &self.nodes;
        let spec = self.editor.spec_mut();
        let (have, others): (Vec<EventEdge>, Vec<EventEdge>) = spec
            .events
            .get(&at)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .partition(|e| nodes.contains_key(&e.from().node));
        let have: Vec<EventOut> = have.iter().map(|e| e.from()).collect();
        if have == want {
            return;
        }
        spec.events.remove(&at);
        for edge in others {
            spec.connect_events(at, edge);
        }
        for from in want {
            spec.connect_events(at, EventEdge::Direct(from));
        }
        self.edited = true;
    }

    /// The event outputs feeding `to`, in the graph's order: every edge the
    /// spec holds, a modulation lane's included.
    pub(crate) fn event_sources_of(&self, to: EventInPort) -> Vec<EventOutPort> {
        let Some(at) = to.raw() else {
            return Vec::new();
        };
        self.editor
            .spec()
            .events
            .get(&at)
            .into_iter()
            .flatten()
            .map(|e| EventOutPort::of_raw(e.from()))
            .collect()
    }

    // --- Event feeds ---

    /// Sets what `feeder` feeds `sink`'s event input 0, replacing what it fed
    /// before (empty: nothing). The sink's input reads the union of every
    /// feeder's sources that are in the graph, written when it changes; a
    /// sink with no event input takes nothing, and keeps the feed.
    ///
    /// Each [`Feeder`] owns only its own entry, so feeders never overwrite
    /// each other.
    pub fn set_feed(
        &mut self,
        sink: impl Into<NodeKey>,
        feeder: Feeder,
        sources: Vec<EventOutPort>,
    ) {
        let sink = sink.into();
        if sources.is_empty() {
            // No entry is made for a feeder that feeds nothing.
            let Some(feeds) = self.feeds.get_mut(&sink) else {
                return;
            };
            if feeds.remove(&feeder).is_none() {
                return;
            }
            if feeds.is_empty() {
                self.feeds.remove(&sink);
            }
        } else {
            let feeds = self.feeds.entry(sink).or_default();
            if feeds.get(&feeder) == Some(&sources) {
                return;
            }
            feeds.insert(feeder, sources);
        }
        self.write_feeds(sink);
    }

    /// What `feeder` feeds `sink`: empty when nothing.
    pub fn feed(&self, sink: impl Into<NodeKey>, feeder: Feeder) -> &[EventOutPort] {
        let sink = sink.into();
        self.feeds
            .get(&sink)
            .and_then(|f| f.get(&feeder))
            .map_or(&[], Vec::as_slice)
    }

    /// Every source any feeder feeds `sink`, sorted and without repeats.
    pub fn feeds(&self, sink: impl Into<NodeKey>) -> Vec<EventOutPort> {
        let sink = sink.into();
        let mut all: Vec<EventOutPort> = self
            .feeds
            .get(&sink)
            .into_iter()
            .flat_map(|f| f.values().flatten().copied())
            .collect();
        all.sort();
        all.dedup();
        all
    }

    /// Moves every feeder's sources from sink `from` to sink `to`, replacing
    /// what each fed `to`: a host whose sink was re-bound to a new node keeps
    /// its feeds. `from` is left fed by nothing.
    pub fn move_feeds(&mut self, from: impl Into<NodeKey>, to: impl Into<NodeKey>) {
        let from = from.into();
        let to = to.into();
        if from == to {
            return;
        }
        let Some(moved) = self.feeds.remove(&from) else {
            return;
        };
        let into = self.feeds.entry(to).or_default();
        for (feeder, sources) in moved {
            into.insert(feeder, sources);
        }
        self.write_feeds(from);
        self.write_feeds(to);
    }

    /// Write `sink`'s feeds into its event input 0: the union of every
    /// feeder's sources still in the graph, beside the edges the host wired
    /// there itself (every edge the feeds did not put there), which stay.
    fn write_feeds(&mut self, sink: NodeKey) {
        if !self.nodes.contains_key(&sink) || self.node_event_inputs(sink) == Ok(0) {
            return;
        }
        let union = self.fed_in_graph(sink);
        let ours = self.fed_edges.get(&sink).cloned().unwrap_or_default();
        let mut all: Vec<EventOutPort> = self
            .event_sources_of(EventInPort::new(sink, 0))
            .into_iter()
            .filter(|s| self.nodes.contains_key(&s.node()) && !ours.contains(s))
            .collect();
        let hand = all.len();
        let theirs: Vec<EventOutPort> = union
            .into_iter()
            .filter(|s| !all[..hand].contains(s))
            .collect();
        all.extend_from_slice(&theirs);
        self.set_fed_edges(sink, theirs);
        self.write_event_sources(sink, 0, &all);
    }

    /// Every source any feeder feeds `sink` that is an event output the graph
    /// has: a source re-inserted without that port is not fed until it has it
    /// again.
    fn fed_in_graph(&self, sink: NodeKey) -> Vec<EventOutPort> {
        self.feeds(sink)
            .into_iter()
            .filter(|s| self.check_event_out(*s).is_ok())
            .collect()
    }

    /// Records `sources` as the edges the feeds own on `sink`'s input 0.
    fn set_fed_edges(&mut self, sink: NodeKey, sources: Vec<EventOutPort>) {
        if sources.is_empty() {
            self.fed_edges.remove(&sink);
        } else {
            self.fed_edges.insert(sink, sources);
        }
    }

    /// Takes `node` out of the graph. Every edge to and from it reads silence
    /// afterwards, so a sink still naming it is left silent rather than
    /// dangling, and the feeds into and from it go.
    ///
    /// The graph's one removal for a node, whatever it is (a plugin, an inlet,
    /// a clip). The edges fade out first (a declicked removal): the node stays
    /// in the editor, rendering, until they have. [`Removal::unfaded`] cuts it
    /// at the next block boundary instead. A replacement waiting on it is
    /// dropped and reported
    /// ([`SessionEvent::ReplaceDropped`](crate::SessionEvent::ReplaceDropped)),
    /// and the session forgets what it declared on the node (modulation, MIDI
    /// routes, an input's capture) at its next update, as
    /// [`Session::remove`](crate::Session::remove) does at once.
    ///
    /// # Errors
    ///
    /// [`RemoveError::NoSuchNode`] when `node` is not in the graph (removed
    /// already, or never added); nothing changes.
    pub fn remove(&mut self, node: impl Into<NodeKey>) -> Result<Removal<'_, F>, RemoveError> {
        let node = node.into();
        self.values.retire(node);
        self.values.retire_node_controls(node);
        self.values.retire_declared_on(node);
        let Some(entry) = self.nodes.remove(&node) else {
            return Err(RemoveError::NoSuchNode(node));
        };
        self.drop_parked(node, entry.controls);
        #[cfg(feature = "plugin")]
        if let Some(record) = self.plugins.remove(&node) {
            record.retire().settle(node, &mut self.graveyard);
        }
        self.removed.push(node);
        // The edges it feeds fade out first: the node stays in the editor,
        // rendering, until they have.
        self.editor.remove_declicked(node);
        self.sweep_gain_targets();
        self.feeds.remove(&node);
        for feeds in self.feeds.values_mut() {
            for sources in feeds.values_mut() {
                sources.retain(|s| s.node() != node);
            }
            feeds.retain(|_, s| !s.is_empty());
        }
        self.feeds.retain(|_, f| !f.is_empty());
        self.fed_edges.remove(&node);
        for sources in self.fed_edges.values_mut() {
            sources.retain(|s| s.node() != node);
        }
        self.fed_edges.retain(|_, s| !s.is_empty());
        self.edited = true;
        Ok(Removal::new(self, node))
    }

    /// The cut of [`Removal::unfaded`]: `node` leaves the editor at the next
    /// block boundary, its edges not faded out.
    pub(crate) fn cut_removal(&mut self, node: NodeKey) {
        if self.editor.is_leaving(node) {
            self.editor.remove(node);
            self.edited = true;
        }
    }

    /// Drops the replacements waiting on `node`, which left the graph, and
    /// holds what the outgoing node's controls were until its unit is gone.
    /// `current` is what the entry held: the newest incoming node's, which
    /// never ran, when a replacement waited.
    fn drop_parked(&mut self, node: NodeKey, current: Option<Held>) {
        // A replacement waiting on it would only fail at its retry, dropping
        // the incoming node then: drop it here, on the control thread.
        let mut stashes = Vec::new();
        for parked in std::mem::take(&mut self.parked) {
            if parked.node == node {
                self.dropped.push(node);
                stashes.extend(parked.stash);
            } else {
                self.parked.push_back(parked);
            }
        }
        // The first stash is the controls of the node that is rendering; the
        // rest, and the entry's, are of incoming nodes that never ran.
        let mut stashes = stashes.into_iter();
        match stashes.next() {
            Some(Stash(outgoing)) => {
                if let Some(held) = outgoing {
                    self.retiring.push((node, held.controls));
                }
            }
            None => {
                if let Some(held) = current {
                    self.retiring.push((node, held.controls));
                }
            }
        }
    }

    /// The nodes whose waiting replacements a removal dropped since the last
    /// call.
    pub(crate) fn take_dropped(&mut self) -> Vec<NodeKey> {
        std::mem::take(&mut self.dropped)
    }

    /// The nodes removed since the last call: what the session forgets its
    /// own declarations on (modulation, MIDI, inputs), whichever way the
    /// removal came, [`Session::remove`](crate::Session::remove) or the
    /// graph's own.
    pub(crate) fn take_removed(&mut self) -> Vec<NodeKey> {
        std::mem::take(&mut self.removed)
    }

    /// How many nodes the graph holds.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Whether `node` is in the graph, a modulation lane or signal node
    /// included (staged, committed or not).
    pub fn contains(&self, node: impl Into<NodeKey>) -> bool {
        let node = node.into();
        self.editor.spec().topology.nodes.contains_key(&node) && !self.editor.is_leaving(node)
    }

    /// Whether `node` is in what a bounce of the master forks: an output
    /// reads it, or it feeds one that is read. A bounce's stem names such a
    /// node. A graph whose bus edges do not lower reaches none.
    #[cfg(feature = "export")]
    pub(crate) fn reaches_outputs(&self, node: NodeKey) -> bool {
        self.editor.reaches_outputs(node).unwrap_or(false)
    }
}

impl<F: Float> Graph<F> {
    /// Applies `decl` to the editor through `m` (doc 014 §8.2), resolving the
    /// host's keys through `keys`. A native target's knob is its node's own
    /// `Param` cell, from the node's `ParamSet`. Marks the graph edited
    /// when the apply asks for a commit.
    ///
    /// # Errors
    ///
    /// The apply's refusal: the graph keeps what the last apply built.
    pub(crate) fn modulate(
        &mut self,
        m: &mut tutti_nodes::Modulation,
        decl: &tutti_modulation::Declarations<'_>,
        keys: &dyn ModKeys,
    ) -> Result<(tutti_nodes::Applied, PluginTargets), tutti_nodes::ReconcileError> {
        let host = ModHostOver {
            values: &self.values,
            #[cfg(feature = "plugin")]
            plugins: &self.plugins,
            keys,
            plugin_targets: std::cell::RefCell::new(BTreeMap::new()),
        };
        let applied = m.apply(decl, &mut self.editor, &host)?;
        if applied.commit {
            self.edited = true;
        }
        Ok((applied, host.plugin_targets.into_inner()))
    }
}

impl<F: Float> Graph<F> {
    /// Points each hosted plugin's driven parameters at the lanes `state` holds:
    /// where a set of a driven parameter goes.
    #[cfg(feature = "plugin")]
    pub(crate) fn refresh_driven(&self, state: &crate::modulation::ModState) {
        for (&node, hosted) in &self.plugins {
            hosted.driven.replace(state.plugin_lanes(node));
        }
    }
}

impl<F: Float> Graph<F> {
    /// Schedules the note-envelope resyncs `m` owes, after the commit that
    /// sent their nodes (`Modulation::send_resyncs`).
    pub(crate) fn send_mod_resyncs(&mut self, m: &mut tutti_nodes::Modulation) {
        // A resync refused for now stays owed (the next update retries); one
        // refused for good is counted (`Modulation::resyncs_dropped`).
        if let Err(e) = m.send_resyncs(&mut self.editor) {
            tracing::debug!("modulation: a resync waits: {e}");
        }
        self.pump_local();
    }
}

impl<F: Float> Graph<F> {
    // --- Edges ---

    /// The graph's raw source for an [`OutPort`].
    fn raw_source(port: OutPort) -> Option<PortSource> {
        match port.end() {
            OutEnd::Node(node, index) => Some(PortSource::Node(RawOut {
                node,
                port: u16::try_from(index).ok()?,
            })),
            OutEnd::Input(index) => Some(PortSource::Global(u16::try_from(index).ok()?)),
        }
    }

    /// The graph's raw sources for `ports`: a port whose index no `u16` holds is
    /// a source that does not exist, named, never dropped.
    fn raw_sources(&self, ports: &[OutPort]) -> Result<Vec<PortSource>, MissingSource> {
        ports
            .iter()
            .map(|&port| {
                Self::raw_source(port).ok_or_else(|| match port.end() {
                    OutEnd::Node(node, _) => MissingSource::Port {
                        node,
                        port: u16::MAX,
                        width: self
                            .shape(node)
                            .map_or(0, |s| s.audio_out().count() as usize),
                    },
                    OutEnd::Input(_) => MissingSource::Global {
                        channel: u16::MAX,
                        width: self.inputs(),
                    },
                })
            })
            .collect()
    }

    /// The graph's raw input port of a node's [`InPort`], `None` for a root
    /// channel or an index no port has.
    fn raw_in(port: InPort) -> Option<RawIn> {
        match port.end() {
            InEnd::Node(node, index) => Some(RawIn {
                node,
                port: u16::try_from(index).ok()?,
            }),
            InEnd::Output(_) => None,
        }
    }

    /// The graph's name for `edge`: a direct one, `None` for an index no
    /// port has.
    pub(crate) fn edge_id(edge: Edge) -> Option<EdgeId> {
        let sink = match edge.sink().end() {
            InEnd::Node(..) => PortSink::Port(Self::raw_in(edge.sink())?),
            InEnd::Output(channel) => PortSink::Output(u16::try_from(channel).ok()?),
        };
        Some(EdgeId {
            sink,
            from: RawEdge::Direct(Self::raw_source(edge.source())?),
        })
    }

    /// The source a raw [`PortSource`] names, `None` for a node the session did
    /// not insert (a lane) or silence.
    fn lift(&self, source: PortSource) -> Option<OutPort> {
        match source {
            PortSource::Node(p) => self
                .nodes
                .contains_key(&p.node)
                .then(|| OutPort::new(p.node, usize::from(p.port))),
            PortSource::Global(port) => Some(Root::input(usize::from(port))),
            PortSource::Zero => None,
        }
    }

    pub(crate) fn check_out(&self, port: OutPort) -> Result<(), ConnectError> {
        let fits = match port.end() {
            OutEnd::Node(node, index) => {
                if !self.contains(node) {
                    return Err(ConnectError::NoSuchNode(node));
                }
                index < self.node_outputs(node)?
            }
            OutEnd::Input(index) => index < self.inputs(),
        };
        fits.then_some(()).ok_or(ConnectError::NoSuchOutPort(port))
    }

    pub(crate) fn check_in(&self, port: InPort) -> Result<(), ConnectError> {
        let fits = match port.end() {
            InEnd::Node(node, index) => {
                if !self.contains(node) {
                    return Err(ConnectError::NoSuchNode(node));
                }
                index < self.node_inputs(node)?
            }
            InEnd::Output(index) => index < self.outputs(),
        };
        fits.then_some(()).ok_or(ConnectError::NoSuchInPort(port))
    }

    /// Checks one wire from `from` into `to`: both ends exist, and a node does
    /// not feed itself.
    pub(crate) fn check_wire(&self, from: OutPort, to: InPort) -> Result<(), ConnectError> {
        self.check_out(from)?;
        self.check_in(to)?;
        match (from.node(), to.node()) {
            (Some(a), Some(b)) if a == b => Err(ConnectError::Itself(a)),
            _ => Ok(()),
        }
    }

    /// Adds the edge `from -> to`: whether it was new (the graph is marked
    /// edited only then). A new edge **fades in** over
    /// [`DECLICK`](tutti_graph::DECLICK), and an edge that
    /// is fading out is brought back (and counts as new): see
    /// [`Connection::unfaded`](crate::Connection::unfaded).
    pub(crate) fn add_edge(&mut self, from: OutPort, to: InPort) -> Result<bool, ConnectError> {
        self.check_wire(from, to)?;
        let source = Self::raw_source(from).expect("checked: the port fits");
        let id = Self::edge_id(Edge::new(from, to)).expect("checked: the ports fit");
        let leaving = self.fading_out(id);
        let topology = &mut self.editor.spec_mut().topology;
        let added = match to.end() {
            InEnd::Node(..) => {
                let at = Self::raw_in(to).expect("checked: the port fits");
                let held = topology.edges.get(&at).map_or(0, Vec::len);
                topology.connect(at, RawEdge::Direct(source));
                topology.edges.get(&at).map_or(0, Vec::len) != held
            }
            InEnd::Output(channel) => topology.connect_output(channel, source),
        };
        if added || leaving {
            // The edge exists now, so the fade is the editor's to take.
            let _ = self.editor.set_edge_fade(id, EdgeFade::In);
            self.edited = true;
        }
        Ok(added || leaving)
    }

    /// Whether `id` is an edge the session has already taken out and is only
    /// fading to silence.
    fn fading_out(&self, id: EdgeId) -> bool {
        let spec = self.editor.spec();
        spec.has_edge(id) && spec.gains.get(&id).is_some_and(|g| g.fade == EdgeFade::Out)
    }

    /// Makes the edge `edge` a plain, unfaded connection: no fade in or out
    /// (and no entry, when the fade was all it carried). The step
    /// [`Connection::unfaded`](crate::Connection::unfaded) takes.
    pub(crate) fn harden_edge(&mut self, edge: Edge) {
        let Some(id) = Self::edge_id(edge) else {
            return;
        };
        if !self
            .editor
            .spec()
            .gains
            .get(&id)
            .is_some_and(|g| g.fade != EdgeFade::None)
        {
            return;
        }
        let _ = self.editor.set_edge_fade(id, EdgeFade::None);
        let plain = self.editor.spec().gains.get(&id).is_some_and(|g| {
            g.base == Amplitude::UNITY && g.source.is_none() && g.range == EdgeGain::DEFAULT_RANGE
        });
        if plain {
            self.editor.spec_mut().clear_edge_gain(id);
            self.sweep_gain_targets();
        }
        self.edited = true;
    }

    /// Removes the edge `from -> to`, its gain with it: whether it was there.
    /// With `hard` false an edge the plan has heard **fades out** first and
    /// leaves the spec when the fade ends (`Editor::finish_fades`), and one
    /// already fading out is not there.
    pub(crate) fn remove_edge(&mut self, from: OutPort, to: InPort, hard: bool) -> bool {
        match Self::edge_id(Edge::new(from, to)) {
            Some(id) => self.drop_edge(id, hard),
            None => false,
        }
    }

    fn drop_edge(&mut self, id: EdgeId, hard: bool) -> bool {
        let dropped = self.drop_edge_now(id, hard);
        self.sweep_gain_targets();
        dropped
    }

    fn drop_edge_now(&mut self, id: EdgeId, hard: bool) -> bool {
        if !self.editor.spec().has_edge(id) {
            return false;
        }
        let out = self.fading_out(id);
        if out && !hard {
            return false;
        }
        if !hard
            && matches!(id.from, RawEdge::Direct(_))
            && self.editor.edge_is_live(id)
            && self.editor.set_edge_fade(id, EdgeFade::Out).is_ok()
        {
            self.edited = true;
            return true;
        }
        let spec = self.editor.spec_mut();
        let removed = match id.sink {
            PortSink::Port(at) => spec.disconnect(at, id.from),
            PortSink::Output(channel) => match id.from {
                RawEdge::Direct(source) => {
                    usize::from(channel) < spec.topology.outputs.len()
                        && spec.disconnect_output(usize::from(channel), source)
                }
                RawEdge::Feedback(_) => false,
            },
        };
        self.edited |= removed;
        removed
    }

    /// Every edge of `to`'s sink the session holds and is not fading out.
    fn live_edges(&self, to: InPort) -> Vec<EdgeId> {
        let spec = self.editor.spec();
        let all: Vec<EdgeId> = match to.end() {
            InEnd::Node(..) => {
                let Some(at) = Self::raw_in(to) else {
                    return Vec::new();
                };
                spec.topology
                    .edges
                    .get(&at)
                    .into_iter()
                    .flatten()
                    .map(|&from| EdgeId {
                        sink: PortSink::Port(at),
                        from,
                    })
                    .collect()
            }
            InEnd::Output(channel) => spec
                .topology
                .outputs
                .get(channel)
                .into_iter()
                .flatten()
                .map(|&s| EdgeId {
                    sink: PortSink::Output(channel as u16),
                    from: RawEdge::Direct(s),
                })
                .collect(),
        };
        all.into_iter().filter(|id| !self.fading_out(*id)).collect()
    }

    /// What feeds `to`: every source of its set, in the order the graph sums
    /// them (the compiler's: by node, then port, then a global input). Empty
    /// for a port nothing feeds, which reads silence. `to` is an [`InPort`]
    /// (its sources are [`OutPort`]s) or an [`EventInPort`] (its sources are
    /// [`EventOutPort`]s, every edge the graph holds there, a modulation
    /// lane's included).
    ///
    /// # Errors
    ///
    /// [`ConnectError`] for a node or port that does not exist.
    pub fn sources<P: Fanin>(&self, to: P) -> Result<Vec<P::Source>, ConnectError> {
        to.sources(self)
    }

    pub(crate) fn channel_sources(&self, to: InPort) -> Vec<OutPort> {
        // An edge fading out is no longer a source: it is only the tail of
        // the old wiring, until its fade ends.
        self.live_edges(to)
            .into_iter()
            .filter_map(|id| match id.from {
                RawEdge::Direct(source) => self.lift(source),
                // This graph writes no feedback edge.
                RawEdge::Feedback(_) => None,
            })
            .collect()
    }

    /// Feeds `to` from exactly `from`, which the graph sums. A source listed
    /// twice is one source, and the order does not matter (the graph orders
    /// them). Touches nothing, and marks nothing edited, when that is what
    /// the sink already holds. An empty set clears the sink
    /// ([`disconnect_all`](Self::disconnect_all)): silence is the empty set.
    ///
    /// What is added fades in and what is taken out fades out;
    /// [`SetSources::unfaded`] makes the change step at the next block
    /// boundary instead (as [`Connection::unfaded`](crate::Connection::unfaded)
    /// does for [`connect`](Self::connect)).
    ///
    /// # Errors
    ///
    /// [`ConnectError`], with nothing changed: a node or port that does not
    /// exist, or a node among its own sources.
    pub fn set_sources<P: Fanin>(
        &mut self,
        to: P,
        from: impl IntoIterator<Item = P::Source>,
    ) -> Result<SetSources<'_, P, F>, ConnectError> {
        let from: Vec<P::Source> = from.into_iter().collect();
        let cut = to.set_sources(self, from.clone(), false)?;
        Ok(SetSources::new(self, to, from, cut))
    }

    pub(crate) fn set_channel_sources(
        &mut self,
        to: InPort,
        from: Vec<OutPort>,
        hard: bool,
    ) -> Result<Vec<EdgeId>, ConnectError> {
        let mut want: Vec<(PortSource, OutPort)> = Vec::new();
        for port in from {
            self.check_wire(port, to)?;
            let source = Self::raw_source(port).ok_or(ConnectError::NoSuchOutPort(port))?;
            want.push((source, port));
        }
        self.check_in(to)?;
        if matches!(to.end(), InEnd::Node(..)) && Self::raw_in(to).is_none() {
            return Err(ConnectError::NoSuchInPort(to));
        }
        want.sort_by_key(|(source, _)| *source);
        want.dedup_by_key(|(source, _)| *source);
        // What the sink holds and is not asked for goes (fading out, if it
        // has been heard); what it is asked for and lacks comes (fading in).
        // What it already holds is left alone, so a set of what is there
        // touches nothing.
        let mut cut = Vec::new();
        for id in self.live_edges(to) {
            let keep = matches!(id.from, RawEdge::Direct(s) if want.iter().any(|(w, _)| *w == s));
            if !keep {
                self.drop_edge(id, hard);
                cut.push(id);
            }
        }
        for (_, port) in want {
            self.add_edge(port, to)?;
            if hard {
                self.harden_edge(Edge::new(port, to));
            }
        }
        Ok(cut)
    }

    /// Takes the edges `ids` out at the next block boundary, ones already
    /// fading out included: the step [`SetSources::unfaded`] takes.
    pub(crate) fn cut_edges(&mut self, ids: &[EdgeId]) {
        for &id in ids {
            self.drop_edge(id, true);
        }
    }

    /// Takes every source out of `to`, their gains with them: whether it fed
    /// anything. (For an [`EventInPort`]: what the caller wired; a feed's
    /// and a modulation lane's edges stay.) Each edge the plan has heard fades out
    /// ([`disconnect`](Self::disconnect)).
    ///
    /// # Errors
    ///
    /// [`ConnectError`] for a node or port that does not exist; nothing
    /// changes.
    #[must_use = "the call refuses a node or port that does not exist"]
    pub fn disconnect_all<P: Fanin>(&mut self, to: P) -> Result<bool, ConnectError> {
        to.disconnect_all(self)
    }

    pub(crate) fn disconnect_all_channels(&mut self, to: InPort) -> bool {
        let mut any = false;
        for id in self.live_edges(to) {
            any |= self.drop_edge(id, false);
        }
        any
    }

    /// Connects `from` to `to` and returns the connection, which a gain can
    /// follow: `g.connect(osc.out(0), lp.input(0))?.gain(Amplitude::new(0.5))?`.
    ///
    /// What the two ends are says what is connected:
    ///
    /// - an [`OutPort`] into an [`InPort`]: **one channel**
    ///   ([`Root::output`] is the root's channel, [`Root::input`] a global
    ///   input);
    /// - a [`Key`] (or [`NodeKey`]) into a key, or into [`Root`]: **a main
    ///   bus**, channel `c` of the source into channel `c` of the sink. The two
    ///   must be the same width. A bus into the root may be narrower than it:
    ///   it is placed by speaker, on the topology the root was declared with,
    ///   else on the engine's order for the root's width
    ///   ([`fold_to_width`](tutti_graph::fold_to_width)), so a mono source
    ///   feeds both channels of a stereo root at gain 1.0. A placement that
    ///   would weight a channel (a centre into a root with none) is refused:
    ///   a weighted fold is a bus edge. A connection never truncates;
    /// - a [`BusSource`] into a [`BusRef`] (`key.bus_out(i)`,
    ///   `key.bus_in(i)`) or into [`Root`]: **a bus edge**, a fold between two
    ///   declared topologies, lowered at the commit to one weighted lane per
    ///   contribution (`Fold::contributions`) and summed with any other source
    ///   of the sink's ports. Into the root it folds to the topology the root
    ///   was declared with
    ///   ([`Session::declare_root_topology`](crate::Session::declare_root_topology)).
    ///   Its lanes carry the fold's weights and no gain of their own;
    /// - an [`EventOutPort`] into an [`EventInPort`] (`key.events_out()`,
    ///   `key.events_in()`): **an event edge**. Events merge by offset at the
    ///   sink, and an event edge is never faded (a note-off is a message, not
    ///   a level), so `.unfaded()` on a [`Connection`] or a
    ///   [`Disconnection`] changes nothing for it.
    ///
    /// It **adds**: what already feeds the sink stays (the graph sums them),
    /// and an edge already connected is kept once. To replace what feeds a
    /// sink, [`set_sources`](Self::set_sources).
    ///
    /// # Errors
    ///
    /// [`ConnectError`], with nothing stored: a node or port that does not
    /// exist, a node fed by itself, a node with no main bus on the side used,
    /// two main buses of different widths, or, for a bus edge, the
    /// [`BusError`](tutti_graph::BusError) (a fold that does not exist is `FoldError`, never a silent
    /// drop).
    pub fn connect<O: Outlet, T: Inlet<O::Wire>>(
        &mut self,
        from: O,
        to: T,
    ) -> Result<Connection<'_, O::Wire, F>, ConnectError> {
        let (from, to) = (from.out(), to.sink());
        let edges = <O::Wire as Wire>::connect(self, from, to)?;
        Ok(Connection::new(self, edges, from, to))
    }

    /// Takes the edge `from -> to` out, its gain with it (for a bus edge, its
    /// lanes and weights; the edges and gains the author set stay). The
    /// returned [`Disconnection`] says whether it was connected
    /// ([`was_connected`](Disconnection::was_connected)), and the graph is
    /// marked edited when it was. For a main bus it takes out the edges
    /// [`connect`](Self::connect) adds.
    ///
    /// **The edge fades out** over [`DECLICK`](tutti_graph::DECLICK),
    /// from the weight it has, and leaves the graph when the fade
    /// ends (the next [`update`](crate::Session::update) after that drops it
    /// from the spec). It is already not a source of its sink
    /// ([`sources`](Self::sources)), and connecting it again brings it back
    /// from where the fade has got to. An edge no plan has heard (connected
    /// since the last commit) has nothing to fade and goes at once.
    /// [`Disconnection::unfaded`] cuts at a block boundary, as
    /// `tutti_graph::Editor` does: for a test, or an analysis graph nobody
    /// listens to.
    ///
    /// # Errors
    ///
    /// [`ConnectError`] for a node or port that does not exist; nothing
    /// changes. An edge that is not there is not an error:
    /// [`was_connected`](Disconnection::was_connected) is false.
    pub fn disconnect<O: Outlet, T: Inlet<O::Wire>>(
        &mut self,
        from: O,
        to: T,
    ) -> Result<Disconnection<'_, O::Wire, F>, ConnectError> {
        let (from, to) = (from.out(), to.sink());
        let was_connected = <O::Wire as Wire>::disconnect(self, from, to, false)?;
        Ok(Disconnection::new(self, was_connected, from, to))
    }

    pub(crate) fn add_bus_edge(
        &mut self,
        from: BusSource,
        to: BusSink,
    ) -> Result<(), ConnectError> {
        let at = Self::bus_sink(to);
        let was = self.editor.spec().bus_fade(from, at);
        match to {
            BusSink::Node(to) => self.editor.connect_bus(from, to),
            BusSink::Root => self.editor.connect_bus_to_root(from),
        }?;
        // A new bus edge, or one on its way out, fades in: each lane of its
        // fold does.
        if was.is_none_or(|fade| fade == EdgeFade::Out) {
            self.editor.set_bus_fade(from, at, EdgeFade::In);
        }
        self.edited = true;
        Ok(())
    }

    fn bus_sink(to: BusSink) -> Option<BusRef> {
        match to {
            BusSink::Node(bus) => Some(bus),
            BusSink::Root => None,
        }
    }

    pub(crate) fn remove_bus_edge(&mut self, from: BusSource, to: BusSink, hard: bool) -> bool {
        let at = Self::bus_sink(to);
        let Some(fade) = self.editor.spec().bus_fade(from, at) else {
            return false;
        };
        if fade == EdgeFade::Out && !hard {
            return false;
        }
        if !hard && self.editor.bus_edge_is_live(from, at) {
            self.editor.set_bus_fade(from, at, EdgeFade::Out);
            self.edited = true;
            return true;
        }
        let removed = match to {
            BusSink::Node(to) => self.editor.disconnect_bus(from, to),
            BusSink::Root => self.editor.disconnect_bus_from_root(from),
        };
        self.edited |= removed;
        removed
    }

    /// Makes the bus edge `from -> to` a plain, unfaded one.
    pub(crate) fn harden_bus_edge(&mut self, from: BusSource, to: BusSink) {
        if self
            .editor
            .set_bus_fade(from, Self::bus_sink(to), EdgeFade::None)
        {
            self.edited = true;
        }
    }

    /// What feeds a bus edge's sink (an input bus of a node, or the root), in
    /// source order. An edge fading out is no longer a source.
    ///
    /// # Errors
    ///
    /// [`ConnectError::NoSuchNode`] when the bus's node is not in the graph.
    pub fn bus_sources(&self, to: impl Inlet<Bus>) -> Result<Vec<BusSource>, ConnectError> {
        let to = to.sink();
        if let BusSink::Node(bus) = to
            && !self.contains(bus.node)
        {
            return Err(ConnectError::NoSuchNode(bus.node));
        }
        let spec = self.editor.spec();
        let feeds = match to {
            BusSink::Node(bus) => spec.bus_edges.get(&bus),
            BusSink::Root => Some(&spec.root_sources),
        };
        Ok(feeds.map_or_else(Vec::new, |feeds| {
            feeds
                .iter()
                .filter(|f| f.fade != EdgeFade::Out)
                .map(|f| f.source)
                .collect()
        }))
    }

    /// Whether the nodes a bus edge names are in the graph.
    pub(crate) fn check_bus_nodes(&self, from: BusSource, to: BusSink) -> Result<(), ConnectError> {
        if let BusSource::Node(bus) = from
            && !self.contains(bus.node)
        {
            return Err(ConnectError::NoSuchNode(bus.node));
        }
        if let BusSink::Node(bus) = to
            && !self.contains(bus.node)
        {
            return Err(ConnectError::NoSuchNode(bus.node));
        }
        Ok(())
    }

    /// The `(output port, input port)` pairs that connect `from`'s main
    /// output to `to`'s main input (or the root's outputs), checked.
    pub(crate) fn main_wires(
        &self,
        from: NodeKey,
        to: MainSink,
    ) -> Result<Vec<(OutPort, InPort)>, ConnectError> {
        let (_, out_buses) = self.node_buses(from)?;
        let main = |buses: &Buses, node| {
            let (index, bus) = buses.main_bus().ok_or(ConnectError::NoMainBus(node))?;
            let first = buses
                .first_port(index)
                .ok_or(ConnectError::NoMainBus(node))?;
            Ok::<_, ConnectError>((first, bus.width()))
        };
        let (out_first, out_width) = main(&out_buses, from)?;
        let out_topology = out_buses
            .main_bus()
            .map(|(_, bus)| bus.topology)
            .ok_or(ConnectError::NoMainBus(from))?;
        match to {
            MainSink::Node(to) => {
                if from == to {
                    return Err(ConnectError::Itself(from));
                }
                let (in_buses, _) = self.node_buses(to)?;
                let (in_first, in_width) = main(&in_buses, to)?;
                if out_width != in_width {
                    return Err(ConnectError::Width {
                        from,
                        from_width: out_width,
                        to,
                        to_width: in_width,
                    });
                }
                Ok((0..out_width)
                    .map(|c| {
                        (
                            OutPort::new(from, out_first + c),
                            InPort::new(to, in_first + c),
                        )
                    })
                    .collect())
            }
            MainSink::Root => {
                let root_width = self.outputs();
                if out_width > root_width {
                    return Err(ConnectError::RootNarrow {
                        from,
                        from_width: out_width,
                        root_width,
                    });
                }
                if out_width == root_width {
                    return Ok((0..out_width)
                        .map(|c| (OutPort::new(from, out_first + c), Root::output(c)))
                        .collect());
                }
                // A narrower bus is placed on the root by speaker: the root's
                // declared topology, else the engine's order for its width. A
                // bus that names no speaker is read in the engine's order for
                // its width.
                let source = tutti_buffers::ChannelTopology::from(out_topology);
                let placed = if source.is_unassigned() {
                    tutti_buffers::ChannelTopology::default_for(source.width())
                } else {
                    source
                };
                let fold = match self.root_topology() {
                    Some(root) => tutti_buffers::Fold::<f32>::between(&placed, root),
                    None => tutti_graph::fold_to_width(
                        &placed,
                        // The root's width is a `ChannelLayout`'s: every call
                        // that sizes it takes one.
                        ChannelLayout::try_from(root_width).expect("a root width fits a layout"),
                    ),
                }
                .map_err(|cause| ConnectError::RootFold { from, cause })?;
                let mut wires = Vec::new();
                for c in 0..root_width {
                    for (k, weight) in fold.contributions(c) {
                        if weight != 1.0 {
                            return Err(ConnectError::RootWeighted { from });
                        }
                        wires.push((OutPort::new(from, out_first + k), Root::output(c)));
                    }
                }
                Ok(wires)
            }
        }
    }

    /// Gives `edge` a gain lane at `gain`: the declaration of a weighted
    /// wire ([`Connection::gain`](crate::Connection::gain)), which is
    /// structural, as a plain edge aliases where a lane needs a sum. An edge
    /// that has one keeps it at `gain`. The lane's value is then the
    /// [`GainTarget`]'s.
    pub(crate) fn declare_edge_gain(
        &mut self,
        edge: Edge,
        gain: Amplitude,
    ) -> Result<(), DeclarationError> {
        if !gain.get().is_finite() {
            return Err(DeclarationError::NotFinite);
        }
        let Some(id) = Self::edge_id(edge) else {
            return Ok(());
        };
        if self.fading_out(id) {
            return Ok(());
        }
        let had = self.editor.spec().gains.contains_key(&id);
        // The wire is in the graph (a connection just made it), and a bus
        // lane is not a wire a connection weights: the only refusal left is
        // the gain, checked above.
        if self.editor.set_edge_gain(id, gain).is_ok() && !had {
            self.edited = true;
        }
        Ok(())
    }

    /// Whether `edge` still has the gain lane its target was made for: it is
    /// in the graph, not fading out, and holds an entry.
    fn has_gain_lane(&self, edge: Edge) -> bool {
        let ends = [edge.source().node(), edge.sink().node()];
        ends.into_iter()
            .flatten()
            .all(|node| self.nodes.contains_key(&node))
            && Self::edge_id(edge).is_some_and(|id| {
                !self.fading_out(id) && self.editor.spec().gains.contains_key(&id)
            })
    }

    /// Forgets the gain target of every edge that has lost its lane: removed,
    /// fading out, or cleared. Control thread.
    fn sweep_gain_targets(&mut self) {
        for edge in self.values.gain_edges() {
            if !self.has_gain_lane(edge) {
                self.values.retire_gain(edge);
            }
        }
    }

    /// The gain target of `edge`, made on the first ask and seeded with the
    /// lane's value.
    ///
    /// # Errors
    ///
    /// [`GainTargetError::NoLane`] when the edge is not in the graph or has
    /// no gain lane, [`GainTargetError::BusLane`] for a lane of a bus edge.
    pub fn gain_target(&self, from: OutPort, to: InPort) -> Result<GainTarget, GainTargetError> {
        let edge = Edge::new(from, to);
        let id = Self::edge_id(edge).ok_or(GainTargetError::NoLane(edge))?;
        match self.editor.check_gain_edge(id) {
            Ok(()) => {}
            Err(GraphInvalid::GainOnBusLane { .. }) => return Err(GainTargetError::BusLane(edge)),
            Err(_) => return Err(GainTargetError::NoLane(edge)),
        }
        if !self.has_gain_lane(edge) {
            return Err(GainTargetError::NoLane(edge));
        }
        let seed = self.editor.edge_gain(id).unwrap_or(Amplitude::UNITY);
        self.values.provide_gain(edge, seed);
        Ok(GainTarget::new(edge))
    }

    pub(crate) fn clear_edge_gain_of(&mut self, edge: Edge) -> bool {
        let Some(id) = Self::edge_id(edge) else {
            return false;
        };
        if self.fading_out(id) {
            return false;
        }
        // A fade in is not the author's gain: the edge goes on rising, to unity.
        let fade = self.editor.spec().gains.get(&id).map(|g| g.fade);
        let cleared = if fade == Some(EdgeFade::In) {
            let fresh = EdgeGain {
                fade: EdgeFade::In,
                ..EdgeGain::default()
            };
            self.editor.spec_mut().set_edge_gain(id, fresh);
            true
        } else {
            self.editor.spec_mut().clear_edge_gain(id).is_some()
        };
        self.edited |= cleared;
        self.sweep_gain_targets();
        cleared
    }

    /// The buses `node` declares, input and output, as its shape holds them
    /// (`Shape::buses_in`, `buses_out`).
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn node_buses(&self, node: impl Into<NodeKey>) -> Result<(Buses, Buses), NoSuchNode> {
        self.shape_of(node.into())
            .map(|s| (*s.buses_in(), *s.buses_out()))
    }

    /// Records why `node` has no pass, if it has none, and whether it
    /// bypasses itself: what its mode and mix targets check a write against.
    /// Called wherever a node's shape is taken (an insert, a replacement).
    fn note_pass(&self, node: NodeKey) {
        let no_pass = self.editor.check_pass(node).err();
        self.values
            .provide_node_controls(node, no_pass, self.editor.bypasses_itself(node));
    }

    /// Hands the editor the controls written since the last call: each
    /// node's mode and mix and each edge's gain, as the editor's own
    /// controls, which a running node reads from its next block with no
    /// commit and no edit. A control whose node or edge has since left the
    /// graph is dropped.
    ///
    /// A hosted plugin with a bypass of its own is not bypassed by the
    /// executor: the host writes the plugin's bypass with the mode
    /// (`PluginHandle::set_bypassed`) and compares it on every control tick.
    /// Off the main thread an in-process plugin's write is lost, and it is
    /// kept only by a tick on the main thread.
    pub(crate) fn apply_controls(&mut self) {
        self.sweep_gain_targets();
        for write in self.values.take_writes() {
            match write {
                Write::Mode(node, mode) => {
                    if self.editor.is_leaving(node) || self.editor.set_mode(node, mode).is_err() {
                        continue;
                    }
                    // A plugin with a bypass of its own keeps running in
                    // `Bypassed` (its node declares it), so its bypass is
                    // written here, once per change. `set_bypassed` leaves a
                    // plugin without one alone.
                    #[cfg(feature = "plugin")]
                    self.write_own_bypass(node);
                }
                Write::Mix(node, mix) => {
                    if !self.editor.is_leaving(node) {
                        let _ = self.editor.set_mix(node, mix);
                    }
                }
                Write::Gain(edge, gain) => {
                    if let Some(id) = Self::edge_id(edge)
                        && !self.fading_out(id)
                    {
                        let _ = self.editor.set_edge_gain(id, gain);
                    }
                }
            }
        }
    }

    /// Writes the bypass of the plugin hosted at `node`, if any, to what the
    /// node's mode says: on every change of mode, and when a plugin enters the
    /// node (an insert, a host, a replacement or a reload). A plugin without a
    /// bypass of its own is left alone. The write is not assumed applied:
    /// [`reconcile_own_bypass`](Self::reconcile_own_bypass) compares on every
    /// tick, and a write that never reached the plugin is written again there.
    #[cfg(feature = "plugin")]
    pub(crate) fn write_own_bypass(&mut self, node: NodeKey) {
        let Ok(mode) = self.mode(node) else { return };
        if let Some(hosted) = self.plugins.get_mut(&node) {
            hosted
                .bypass
                .assert_mode(&hosted.handle, &hosted.mirror, mode);
        }
    }

    /// Makes every healthy hosted plugin's own bypass the node's mode, on the
    /// control tick and with no round trip (`BypassSync::reconcile`): an
    /// in-process plugin's bypass is read, without waiting for its instance, and
    /// an out-of-process plugin's is compared with its mirrored parameter values. A
    /// plugin that differs, or whose write never reached it, is written again,
    /// each episode counted once
    /// ([`Plugin::bypass_writes_unapplied`](crate::Plugin::bypass_writes_unapplied)).
    /// A plugin that refused the mode is not written again until the node's mode
    /// changes: the refusal is latched, and counted as an episode only when the
    /// plugin held another bypass.
    ///
    /// Until a plugin is back in the node's mode the executor does not bypass its
    /// node, so it plays processed audio under `Bypassed` for that long: a tick
    /// or two for an in-process plugin; for an out-of-process one until the plugin
    /// reports its change, or a dropped write is retried (the wait doubles up to
    /// 32 ticks). An out-of-process plugin that refuses a delivered write is not seen. An
    /// in-process plugin's bypass is kept only on a session ticked on the main
    /// thread (its write and read are the main thread's); an in-process plugin on
    /// another thread's tick is not refused, and its node stays unbypassed under
    /// `Bypassed`.
    #[cfg(feature = "plugin")]
    pub(crate) fn reconcile_own_bypass(&mut self) {
        let wants: Vec<(NodeKey, NodeMode)> = self
            .plugins
            .keys()
            .filter_map(|&node| Some((node, self.mode(node).ok()?)))
            .collect();
        for (node, want) in wants {
            if let Some(hosted) = self.plugins.get_mut(&node)
                && hosted.is_healthy()
            {
                hosted
                    .bypass
                    .reconcile(&hosted.handle, &hosted.mirror, want);
            }
        }
    }

    /// `node`'s mode as the editor holds it, [`NodeMode::Active`] for a node
    /// never given one: [`ModeTarget`](crate::ModeTarget) holds the value a
    /// write has just made, which the editor takes at the next update.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn mode(&self, node: impl Into<NodeKey>) -> Result<NodeMode, NoSuchNode> {
        let node = node.into();
        self.shape_of(node)?;
        Ok(self.editor.mode(node))
    }

    /// `node`'s mix as the editor holds it, [`Mix::WET`] for a node never given
    /// one: [`MixTarget`](crate::MixTarget) holds the value a write has just
    /// made, which the editor takes at the next update.
    ///
    /// Once the graph has committed the node this reads its live cell, which is
    /// also the knob of a modulation route onto the node's mix (`Mods::route`
    /// with a `MixRef`): the base the route modulates
    /// around, and what the mix returns to when the route goes. It is the base,
    /// not the lane's value.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn mix(&self, node: impl Into<NodeKey>) -> Result<Mix, NoSuchNode> {
        let node = node.into();
        self.shape_of(node)?;
        Ok(self.editor.mix(node))
    }

    /// Removes the gain entries of edges the topology no longer has: an edge
    /// a whole-set write dropped takes its weight with it (a stale entry
    /// would fail every later commit).
    fn drop_stale_gains(&mut self) {
        let spec = self.editor.spec_mut();
        let stale: Vec<_> = spec
            .gains
            .keys()
            .filter(|id| !spec.has_edge(**id))
            .copied()
            .collect();
        for id in stale {
            spec.clear_edge_gain(id);
        }
        self.sweep_gain_targets();
    }

    // --- Param modulation ---

    /// Whether `node` declares `param` modulatable (its shape's params).
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn declares_param(
        &self,
        node: impl Into<NodeKey>,
        param: impl Into<UnitAddr>,
    ) -> Result<bool, NoSuchNode> {
        Ok(self.shape_of(node.into())?.params.index_of(param).is_some())
    }

    /// Drives `node`'s declared `param` from `source`'s event output 0
    /// (`None`: from nothing, so it reads its base), clamped to `range`:
    /// replaces whatever drove it. When the source is the one the graph
    /// already holds, only the range moves: that goes through
    /// `Editor::set_param_range`, a store the running node reads on its next
    /// block, and marks nothing for commit.
    ///
    /// # Errors
    ///
    /// Refused, touching nothing, for a NaN bound
    /// (`GraphInvalid::BadParamRange`) and for a `node` or a `source` that is
    /// not in the graph (`GraphInvalid::UnknownParamNode`), the errors the
    /// commit would otherwise fail on.
    pub fn set_param_mod(
        &mut self,
        node: impl Into<NodeKey>,
        param: impl Into<UnitAddr>,
        source: Option<NodeKey>,
        range: ParamRange,
    ) -> Result<(), GraphInvalid> {
        let node = node.into();
        let at = ParamIn {
            node,
            param: param.into(),
        };
        if range.min.is_nan() || range.max.is_nan() {
            return Err(GraphInvalid::BadParamRange { at, range });
        }
        for missing in std::iter::once(node).chain(source) {
            if !self.contains(missing) {
                return Err(GraphInvalid::UnknownParamNode { at, missing });
            }
        }
        let want = source.map(|from| EventOut {
            node: from,
            port: 0,
        });
        let held = self.editor.spec().params.get(&at).map(|m| m.source);
        if want.is_some() && held == Some(want) {
            // A range move alone: a control, not a rewire.
            return self.editor.set_param_range(at, range);
        }
        let spec = self.editor.spec_mut();
        spec.params.remove(&at);
        spec.params.insert(
            at,
            ParamMod {
                range,
                source: want,
            },
        );
        self.edited = true;
        Ok(())
    }

    /// Stops modulating `node`'s `param`: it reads its own control again.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn clear_param_mod(
        &mut self,
        node: impl Into<NodeKey>,
        param: impl Into<UnitAddr>,
    ) -> Result<(), NoSuchNode> {
        let node = node.into();
        self.shape_of(node)?;
        let at = ParamIn {
            node,
            param: param.into(),
        };
        if self.editor.spec_mut().params.remove(&at).is_some() {
            self.edited = true;
        }
        Ok(())
    }

    /// How `node`'s `param` is modulated, as the graph value holds it: `None`
    /// when nothing drives it.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn param_mod(
        &self,
        node: impl Into<NodeKey>,
        param: impl Into<UnitAddr>,
    ) -> Result<Option<ParamMod>, NoSuchNode> {
        let node = node.into();
        self.shape_of(node)?;
        Ok(self
            .editor
            .spec()
            .params
            .get(&ParamIn {
                node,
                param: param.into(),
            })
            .cloned())
    }

    /// The graph's taps: open and close audio and event copies of it.
    pub fn taps(&mut self) -> Taps<'_, F> {
        Taps { graph: self }
    }

    /// The graph's level meters, the master's included.
    pub fn meters(&mut self) -> Meters<'_, F> {
        Meters { graph: self }
    }

    /// Closes the taps whose readers were dropped, and returns how many.
    /// The session's [`Reclaim`](crate::Phase::Reclaim) phase runs it.
    pub(crate) fn prune_taps(&mut self) -> usize {
        let pruned = self.editor.prune_taps();
        if pruned > 0 {
            let open = &self.editor.spec().topology.taps;
            self.output_taps.retain(|id, _| open.contains_key(id));
            // A held reader keeps its tap alive, and a claimed one has left the map.
            debug_assert!(
                self.reopened.keys().all(|id| match id {
                    RootReader::Tap(tap) => open.contains_key(tap),
                    RootReader::Meter(_) => true,
                }),
                "no held reader names a pruned tap"
            );
            self.edited = true;
        }
        pruned
    }

    /// Closes the event taps whose readers were dropped, and returns how many.
    /// The session's [`Reclaim`](crate::Phase::Reclaim) phase runs it.
    pub(crate) fn prune_event_taps(&mut self) -> usize {
        let pruned = self.editor.prune_event_taps();
        if pruned > 0 {
            let open = &self.editor.spec().topology.event_taps;
            self.event_taps.retain(|id, _| open.contains_key(id));
            self.edited = true;
        }
        pruned
    }

    /// Closes the meters whose readers were dropped, and returns how many.
    /// The session's [`Reclaim`](crate::Phase::Reclaim) phase runs it.
    pub(crate) fn prune_meters(&mut self) -> usize {
        let pruned = self.editor.prune_meters();
        if pruned > 0 {
            let open = &self.editor.spec().topology.meters;
            // A held reader keeps its meter alive, and a claimed one has left the map.
            debug_assert!(
                self.reopened.keys().all(|id| match id {
                    RootReader::Meter(meter) => open.contains_key(meter),
                    RootReader::Tap(_) => true,
                }),
                "no held reader names a pruned meter"
            );
            self.edited = true;
        }
        pruned
    }

    // --- Shape ---

    /// The graph's global input count.
    pub fn inputs(&self) -> usize {
        self.editor.spec().topology.inputs.count() as usize
    }

    /// The graph's global output count.
    pub fn outputs(&self) -> usize {
        self.editor.spec().topology.outputs.len()
    }

    pub(crate) fn shape(&self, node: NodeKey) -> Option<tutti_graph::Shape> {
        self.editor.shapes().get(&node).copied()
    }

    /// The shape of `node`, or the refusal for a key no node has.
    fn shape_of(&self, node: NodeKey) -> Result<tutti_graph::Shape, NoSuchNode> {
        if !self.contains(node) {
            return Err(NoSuchNode::new(node));
        }
        self.shape(node).ok_or(NoSuchNode::new(node))
    }

    /// `node`'s input port count.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn node_inputs(&self, node: impl Into<NodeKey>) -> Result<usize, NoSuchNode> {
        self.shape_of(node.into())
            .map(|s| s.audio_in().count() as usize)
    }

    /// `node`'s output port count.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn node_outputs(&self, node: impl Into<NodeKey>) -> Result<usize, NoSuchNode> {
        self.shape_of(node.into())
            .map(|s| s.audio_out().count() as usize)
    }

    /// The latency `node` reports: the shape the editor holds, read when the
    /// node was inserted (prepared at the graph's rate) and moved since only
    /// by a re-prepare or a hosted plugin's latency change.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn node_latency(&self, node: impl Into<NodeKey>) -> Result<Samples, NoSuchNode> {
        self.shape_of(node.into()).map(|s| s.latency.samples())
    }

    /// What a plugin's view reports of `node`: the figures the graph holds.
    #[cfg(feature = "plugin")]
    pub(crate) fn plugin_figures(&self, node: NodeKey) -> crate::plugin_diagnostics::Figures {
        crate::plugin_diagnostics::Figures {
            latency: self.node_latency(node).unwrap_or_default(),
            tail: self.node_tail(node).unwrap_or_default(),
        }
    }

    /// The tail `node` reports.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when `node` is not in the graph.
    #[must_use = "the call refuses a node that does not exist"]
    pub fn node_tail(&self, node: impl Into<NodeKey>) -> Result<Tail, NoSuchNode> {
        self.shape_of(node.into()).map(|s| s.tail)
    }

    /// Widen the global outputs to `channels`. New channels read silence.
    /// Never narrows.
    ///
    /// Two requests are refused, and the refusal is returned and kept
    /// ([`Session::root_topology_status`](crate::Session::root_topology_status)):
    ///
    /// - on a root whose topology was declared
    ///   ([`Session::declare_root_topology`](crate::Session::declare_root_topology)),
    ///   anything wider than the declaration: the root is not widened under
    ///   it ([`RootError::Width`](tutti_graph::RootError::Width));
    /// - past [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS): the root
    ///   is widened to that and the excess is not rendered
    ///   ([`FoldError::Unplaced`](tutti_buffers::FoldError::Unplaced), the first
    ///   channel with no place).
    ///
    /// A request for as many or fewer channels than the root has is a no-op.
    ///
    /// # Errors
    ///
    /// As above.
    pub fn widen_outputs(&mut self, channels: usize) -> Result<(), tutti_graph::RootError> {
        let refused = if let Some(root) = self
            .root
            .as_ref()
            .filter(|r| channels > usize::from(r.width().count()))
        {
            Some(tutti_graph::RootError::Width {
                declared: root.width(),
                // Only the report names the request; one past `u16::MAX` is
                // refused either way (it is past `MAX_CHANNELS`), so the
                // saturated figure loses nothing.
                requested: ChannelLayout::clamped(channels),
            })
        } else if channels > tutti_buffers::MAX_CHANNELS {
            Some(tutti_graph::RootError::Fold(
                tutti_buffers::FoldError::Unplaced {
                    channel: tutti_buffers::MAX_CHANNELS,
                    speaker: tutti_buffers::Speaker::Unassigned,
                },
            ))
        } else {
            None
        };
        self.widen_refusal = refused;
        // A declared root is not widened at all when refused; an over-wide
        // request widens as far as the engine renders.
        let target = match refused {
            Some(tutti_graph::RootError::Width { .. }) => 0,
            _ => channels.min(tutti_buffers::MAX_CHANNELS),
        };
        let outputs = &mut self.editor.spec_mut().topology.outputs;
        if outputs.len() < target {
            outputs.resize(target, Vec::new());
            self.edited = true;
        }
        self.publish_output_facts();
        refused.map_or(Ok(()), Err)
    }

    /// The last widening [`widen_outputs`](Self::widen_outputs) refused, until
    /// a request is not or a declaration is made.
    pub(crate) fn widen_refusal(&self) -> Option<tutti_graph::RootError> {
        self.widen_refusal
    }

    /// The generation of the root the next commit carries
    /// ([`RootId`](tutti_graph::RootId)); a restart names it.
    #[cfg(feature = "device")]
    pub(crate) fn root_id(&self) -> tutti_graph::RootId {
        self.editor.spec().root_id
    }

    /// The topology the root was declared with, if one was.
    pub fn root_topology(&self) -> Option<&tutti_buffers::ChannelTopology> {
        self.root.as_ref()
    }

    /// Declares what the root's channels are, and sizes the root to it: the
    /// root becomes exactly as wide as `root` (widened, or narrowed, its
    /// channels past the new width dropped), since a declaration states the
    /// root and the device is no floor under it. `None` takes it back and leaves
    /// the root at its width.
    ///
    /// `device` is the width the engine renders into, when there is one: a
    /// declaration it cannot place is refused before anything changes. The
    /// engine's folds are republished from the declaration, so the next block
    /// is placed by it.
    ///
    /// # Errors
    ///
    /// The [`FoldError`](tutti_buffers::FoldError) when the declaration is empty, wider than the engine
    /// renders ([`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS),
    /// [`FoldError::Unplaced`](tutti_buffers::FoldError::Unplaced)), or cannot be placed on `device`. Nothing is
    /// changed.
    /// Whether `root` can be a root, and be placed on `device` when there is
    /// one: not empty, no wider than the engine renders, and every channel
    /// has a place on the device ([`fold_to_width`](tutti_graph::fold_to_width)).
    pub(crate) fn check_root(
        root: &tutti_buffers::ChannelTopology,
        device: Option<&tutti_buffers::ChannelTopology>,
    ) -> Result<(), tutti_buffers::FoldError> {
        use tutti_buffers::{BufferError, FoldError};
        let width = usize::from(root.width().count());
        if width == 0 {
            return Err(BufferError::ZeroWidth.into());
        }
        if width > tutti_buffers::MAX_CHANNELS {
            return Err(FoldError::Unplaced {
                channel: tutti_buffers::MAX_CHANNELS,
                speaker: root
                    .positions()
                    .and_then(|list| list.get(tutti_buffers::MAX_CHANNELS))
                    .copied()
                    .unwrap_or(tutti_buffers::Speaker::Unassigned),
            });
        }
        if let Some(device) = device {
            tutti_graph::fold_to_topology::<f32>(root, device)?;
        }
        Ok(())
    }

    /// What the engine takes the device of `device` channels to be, and
    /// whether that was stated. `None` before an engine is built over this
    /// graph.
    pub(crate) fn output_placement(
        &self,
        device: ChannelLayout,
    ) -> Option<tutti_engine::OutputPlacement> {
        self.output.as_ref()?.control.device(device)
    }

    /// The device the engine renders into and what it is for a fold; `None`
    /// with no engine.
    pub(crate) fn placed_device(&self) -> Option<(ChannelLayout, tutti_buffers::ChannelTopology)> {
        let placed = self.output.as_ref()?;
        Some((placed.device, self.placed_topology()?))
    }

    /// Publishes the device and the root the output policy is checked
    /// against ([`placed_device`](Self::placed_device),
    /// [`root_in_force`](Self::root_in_force)), after any of them changed.
    fn publish_output_facts(&self) {
        let facts = self
            .placed_device()
            .map(|(device, placed)| crate::policy::OutputFacts {
                device,
                placed,
                root: self.root_in_force(),
            });
        *self.values.output_facts().lock_recover() = facts;
    }

    /// The control side of the engine's output folds; `None` with no engine.
    pub(crate) fn output_control(&self) -> Option<&tutti_engine::OutputControl<F>> {
        self.output.as_ref().map(|placed| &placed.control)
    }

    /// The root the next commit renders: what was declared, else the engine's
    /// order for the width the graph has.
    pub(crate) fn root_in_force(&self) -> tutti_buffers::ChannelTopology {
        self.root.clone().unwrap_or_else(|| {
            tutti_buffers::ChannelTopology::default_for(ChannelLayout::clamped(self.outputs()))
        })
    }

    /// What the device the engine renders into is for a fold: what was stated
    /// for it, else the engine's order for its width. `None` with no engine.
    fn placed_topology(&self) -> Option<tutti_buffers::ChannelTopology> {
        let placed = self.output.as_ref()?;
        Some(placed.control.device(placed.device).map_or_else(
            || tutti_buffers::ChannelTopology::default_for(placed.device),
            |p| p.topology,
        ))
    }

    /// Moves the engine to a new output device of `device` channels, stated
    /// as `topology` or the engine's order for the width, folding every root
    /// published into it. A restart's step; a graph with no engine has no
    /// device to move.
    ///
    /// # Errors
    ///
    /// The [`FoldError`](tutti_buffers::FoldError) when the device has no
    /// channels or `topology` is not its width. Nothing is changed.
    #[cfg(feature = "device")]
    pub(crate) fn place_device(
        &mut self,
        device: ChannelLayout,
        topology: Option<tutti_buffers::ChannelTopology>,
    ) -> Result<(), tutti_buffers::FoldError> {
        if let Some(placed) = &mut self.output {
            placed.control.support(device)?;
            placed.control.state_device(device, topology)?;
            placed.device = device;
        }
        self.publish_output_facts();
        Ok(())
    }

    /// Makes the root declaration last made a cut: the commit that carries
    /// it renders no old root. A declaration that waits for a running
    /// crossfade is applied now instead, and is the cut.
    pub(crate) fn harden_root(&mut self) {
        match self.pending_root.take() {
            Some(root) => self.apply_declared(root, true),
            None => self.editor.set_root_fade(None),
        }
    }

    /// Whether a root declaration waits for the running crossfade to end.
    pub(crate) fn root_pending(&self) -> bool {
        self.pending_root.is_some()
    }

    /// The refusal of a declaration when it was applied, taken: the session
    /// holds it as its own. See [`apply_refusal`](Self).
    pub(crate) fn take_apply_refusal(&mut self) -> Option<tutti_graph::RootError> {
        self.apply_refusal.take()
    }

    /// The refusal of a declaration when it was applied, not yet taken.
    pub(crate) fn apply_refusal(&self) -> Option<tutti_graph::RootError> {
        self.apply_refusal
    }

    /// Applies the declaration that waited, once the crossfade it waited for
    /// is over: the spec, the readers and the announcement change together.
    /// A refusal now (the device no longer places it, the roots in flight are
    /// at their limit) is [`take_apply_refusal`](Self::take_apply_refusal)'s.
    fn apply_pending_root(&mut self) {
        if self.editor.root_fade_running() {
            return;
        }
        if let Some(root) = self.pending_root.take() {
            self.apply_declared(root, false);
        }
    }

    fn apply_declared(&mut self, root: Option<tutti_buffers::ChannelTopology>, hard: bool) {
        let applied = self
            .check_declaration(&root)
            .and_then(|width| self.apply_root(root, hard, width));
        if let Err(e) = applied {
            self.apply_refusal = Some(e);
        }
    }

    /// Declares the root's topology. A declaration that changes it is the next
    /// root, and the commit that carries it crossfades the old root's outputs
    /// into the new root's over the declick length (`Editor::set_root_fade`),
    /// unless `hard`: the new root takes over at the commit's block.
    ///
    /// While a root crossfade runs, a declaration that is not `hard` is held
    /// (checked now, applied by [`collect`](Self::collect) once the fade is
    /// over, the newest replacing an older one): the spec, the readers and the
    /// root in force do not change until then. A declaration refused at the
    /// call drops nothing: the one waiting still applies. The fade is over
    /// when the executor has rendered it, so a held declaration also waits
    /// while no blocks render, and travels with the graph through a restart,
    /// to be checked against the device it finds when it is applied.
    pub(crate) fn declare_root(
        &mut self,
        root: Option<tutti_buffers::ChannelTopology>,
        hard: bool,
    ) -> Result<(), tutti_graph::RootError> {
        let width = self.check_declaration(&root)?;
        if !hard && self.editor.root_fade_running() {
            self.pending_root = Some(root);
            self.apply_refusal = None;
            return Ok(());
        }
        self.pending_root = None;
        self.apply_refusal = None;
        self.apply_root(root, hard, width)
    }

    /// The width of the root `root` declares, if it can be placed on the
    /// device: [`declare_root`](Self::declare_root)'s checks.
    fn check_declaration(
        &self,
        root: &Option<tutti_buffers::ChannelTopology>,
    ) -> Result<usize, tutti_graph::RootError> {
        let placed = self.placed_topology();
        Ok(match root {
            Some(root) => {
                Self::check_root(root, placed.as_ref())?;
                usize::from(root.width().count())
            }
            None => {
                if let Some(placed) = &placed {
                    let outputs = self.outputs();
                    let width = ChannelLayout::try_from(outputs).map_err(|e| {
                        tutti_buffers::FoldError::from(tutti_buffers::BufferError::from(e))
                    })?;
                    tutti_graph::fold_to_topology::<f32>(
                        &tutti_buffers::ChannelTopology::default_for(width),
                        placed,
                    )?;
                }
                self.outputs()
            }
        })
    }

    fn apply_root(
        &mut self,
        root: Option<tutti_buffers::ChannelTopology>,
        hard: bool,
        width: usize,
    ) -> Result<(), tutti_graph::RootError> {
        // A changed declaration is the next root: its folds are published
        // first, so the commit that carries its id finds them. The engine
        // checks the device again, and a refusal there leaves the graph as it
        // was.
        let next = if root == self.root {
            None
        } else {
            let id = self.editor.spec().root_id.next();
            if let Some(placed) = &self.output {
                placed
                    .control
                    .publish_root(id, root.clone(), placed.device)?;
            }
            Some(id)
        };
        // The bus form of the declaration; `check_root` held it to
        // `MAX_CHANNELS`, which is a bus's width.
        let bus = root
            .as_ref()
            .map(|r| tutti_buffers::BusTopology::try_from(r).expect("a checked root fits a bus"));
        // Both roots render the same nodes, in phase.
        self.editor.set_root_fade((!hard).then(|| {
            tutti_graph::Fade::seconds(tutti_graph::DECLICK, self.editor.prepare().sample_rate())
        }));
        let spec = self.editor.spec_mut();
        if spec.root != bus {
            spec.root = bus;
            // The root's bus edges fold to the new topology at the next
            // commit.
            self.edited |= !spec.root_sources.is_empty();
        }
        if let Some(id) = next {
            spec.root_id = id;
            self.edited = true;
        }
        self.root = root;
        self.widen_refusal = None;
        let mut resized = false;
        let outputs = &mut self.editor.spec_mut().topology.outputs;
        if outputs.len() != width {
            outputs.resize(width, Vec::new());
            self.drop_stale_gains();
            self.edited = true;
            resized = true;
        }
        self.reopen_root_readers(resized);
        self.publish_output_facts();
        Ok(())
    }

    // --- Latency ---

    /// Whether an edit waits for a commit: what the session's next update
    /// commits on.
    pub fn has_uncommitted_edits(&self) -> bool {
        self.edited
    }

    /// The compensation of the plan sent last: what the session publishes,
    /// since it is what the executor is handed.
    pub(crate) fn sent_compensation(&self) -> Option<Compensation> {
        self.editor.base().map(|plan| compensation_of(plan))
    }

    /// Publishes the compensation of the plan sent last: the per-channel
    /// pre-roll and the total latency. Nothing before the first plan.
    pub(crate) fn publish_sent(&mut self) {
        if let Some(sent) = self.sent_compensation() {
            self.latency = sent.total();
            self.compensation.set(sent.channels());
        }
    }

    /// The per-channel pre-roll of the plan sent last.
    pub(crate) fn compensation(&self) -> &Arc<tutti_graph::PrerollTable> {
        &self.compensation
    }

    /// The total latency of the plan sent last.
    pub(crate) fn latency(&self) -> Samples {
        self.latency
    }

    /// A node's latency moved at runtime (a plugin's latency cell): hand
    /// `latency` — the figure the node's `Shape` declares now, which for a
    /// plugin is `PluginControls::declared_latency` — to the editor
    /// (`Editor::set_latency`), clamped to what PDC compensates, so the next
    /// commit moves PDC to it without touching the running node.
    ///
    /// Returns whether the editor's figure moved.
    #[cfg(feature = "plugin")]
    pub(crate) fn refresh_node_latency(
        &mut self,
        node: impl Into<NodeKey>,
        latency: Latency,
    ) -> bool {
        let node = node.into();
        if !self.nodes.contains_key(&node) {
            return false;
        }
        // Clamped to what PDC compensates, as the editor clamps a latency it
        // probes at insert: `set_latency` refuses a figure past it, and a
        // refused one would never be retried (the poll only fires again when
        // the plugin's figure moves).
        let latency = clamp_latency(latency, self.editor.prepare().sample_rate());
        if self.node_latency(node) == Ok(latency.samples()) {
            return false;
        }
        match self.editor.set_latency(node, latency) {
            Ok(()) => {
                self.edited = true;
                true
            }
            Err(e) => {
                tracing::error!("graph: latency of {node:?} refused: {e}");
                false
            }
        }
    }

    /// A node's tail ceiling moved at runtime (a plugin's tail-changed
    /// callback): hand `tail` (the ceiling its `Shape` declares now) to the
    /// editor (`Editor::set_tail`), a finite figure past what a node may
    /// declare (`MAX_NODE_TAIL` at the editor's rate) becoming `Unbounded`, so
    /// the next commit moves the plan, the export length and the silence skip
    /// to it without touching the running node. The ceiling moves at that
    /// commit, as a latency does.
    ///
    /// Returns whether the editor's figure moved.
    #[cfg(feature = "plugin")]
    pub(crate) fn refresh_node_tail(&mut self, node: impl Into<NodeKey>, tail: Tail) -> bool {
        let node = node.into();
        if !self.nodes.contains_key(&node) {
            return false;
        }
        // A finite figure past what a node may declare is `Unbounded`, never
        // clamped (a clamp would declare a ceiling shorter than the plugin
        // rings), and so never refused: nothing retries it.
        let rate = self.editor.prepare().sample_rate();
        let tail = tutti_plugin_types::Processing::new(tail).ceiling(rate);
        if self.node_tail(node) == Ok(tail) {
            return false;
        }
        match self.editor.set_tail(node, tail) {
            Ok(()) => {
                self.edited = true;
                true
            }
            Err(e) => {
                tracing::error!("graph: tail of {node:?} refused: {e}");
                false
            }
        }
    }

    // --- Committing and rendering ---

    /// Drain what the executor sent back, freeing the retired units here. A
    /// retired in-process plugin closes on the main thread whichever thread
    /// this is: off the main thread it hands its close to the session's
    /// `MainWork`.
    pub(crate) fn collect(&mut self) {
        let retired = self.editor.collect();
        Self::bury(&mut self.retiring, &retired);
        // An edge whose declick fade has ended leaves the spec (or forgets the
        // fade); the commit that follows compiles it away.
        if self.editor.finish_fades() {
            self.edited = true;
        }
        self.apply_pending_root();
    }

    /// The output ports that lost events since the last call
    /// (`Editor::take_event_losses`).
    pub(crate) fn take_event_losses(
        &mut self,
    ) -> impl Iterator<Item = tutti_graph::EventLoss> + '_ {
        self.editor.take_event_losses()
    }

    /// Refusals no loss entry named (`Editor::take_unreported_event_losses`).
    pub(crate) fn take_unreported_event_losses(&mut self) -> u64 {
        self.editor.take_unreported_event_losses()
    }

    /// `Editor::commit`. `Sent` clears the edit flag; `Refused` and `Retry` keep it.
    pub(crate) fn commit(&mut self) -> Committed {
        if let Some(refused) = self.unplaceable() {
            return Committed::Refused(refused);
        }
        match self.editor.commit() {
            Ok(rewired) => {
                self.note_rewired(rewired);
                self.edited = false;
                self.refused = None;
                self.commits += 1;
                self.pump_local();
                Committed::Sent
            }
            Err(CommitError::Backpressure | CommitError::Repreparing) => Committed::Retry,
            // Kept edited: an edit the flag does not see (a mix, a mode) can
            // make the same spec compile.
            Err(e) => Committed::Refused(e),
        }
    }

    /// The refusal for a spec whose root has no fold into the device: the
    /// engine would render that plan as a silent, counted block. A commit
    /// names the pair, and nothing is sent.
    fn unplaceable(&self) -> Option<CommitError> {
        // No engine, no folds: a graph rendered locally places nothing.
        let Placed { control, device } = self.output.as_ref()?;
        let device = *device;
        let spec = self.editor.spec();
        let width = spec.topology.outputs.len();
        (width > 0 && !control.has_fold(spec.root_id, width, device)).then_some(
            CommitError::Unplaceable {
                root: spec.root_id,
                width,
                device,
            },
        )
    }

    /// Whether `e` differs from the refusal reported last since a commit was
    /// sent, remembering it: a refusal is reported once, not every update.
    pub(crate) fn newly_refused(&mut self, e: &CommitError) -> bool {
        if self.refused.as_ref() == Some(e) {
            return false;
        }
        self.refused = Some(e.clone());
        true
    }

    /// Render one frame on the local executor, under `transport`.
    #[cfg(feature = "testing")]
    pub(crate) fn render_local(
        &mut self,
        transport: &TransportPosition,
        output: InterleavedMut<'_, F>,
    ) {
        let local = self
            .local
            .as_mut()
            .expect("render_frame needs the audio side, and it was taken");
        render(&mut local.exec, &mut local.scratch, transport, output);
    }
}

/// Render one frame of `exec` into `output` (one sample per global output),
/// with silence on every global input.
///
/// `output` wider than the plan reads silence past it; narrower, the extra
/// channels are dropped — the width a caller passes is its own business.
#[cfg(feature = "testing")]
fn render<F: Float>(
    exec: &mut Executor<F>,
    scratch: &mut Vec<Vec<F>>,
    transport: &TransportPosition,
    mut output: InterleavedMut<'_, F>,
) {
    let output = output.samples_mut();
    exec.apply_pending();
    let Some(plan) = exec.plan() else {
        output.fill(F::ZERO);
        return;
    };
    let (ins, outs, root) = (
        plan.global_inputs() as usize,
        plan.rendered_outputs(),
        plan.global_outputs(),
    );
    scratch.resize_with(outs, || vec![F::ZERO]);
    let silence = F::ZERO;
    let inputs: Vec<&[F]> = (0..ins).map(|_| std::slice::from_ref(&silence)).collect();
    let fade = exec.root_fade();
    let mut outputs: Vec<&mut [F]> = scratch.iter_mut().map(|c| &mut c[..1]).collect();
    exec.process(1, transport, &inputs, &mut outputs);
    // With no device there is no fold: the root's channels are the output's
    // by index, and a root change crossfades channel by channel, the old
    // root's channels after the new root's.
    let gains = fade.map(|f| fade_gains::<F>(f.fade.law, f.done, f.fade.duration));
    for (c, o) in output.iter_mut().enumerate() {
        let new = scratch
            .get(c)
            .filter(|_| c < root)
            .map_or(F::ZERO, |s| s[0]);
        *o = match (fade, gains) {
            (Some(f), Some((go, gn))) => {
                let old = if c < f.width {
                    scratch[root + c][0]
                } else {
                    F::ZERO
                };
                old * go + new * gn
            }
            _ => new,
        };
    }
}

/// Resolves a host's modulation keys to graph nodes.
///
/// `tutti_modulation`'s declarations key a target by `TargetKey { node: u64, param }`
/// and a source by `SourceKey(u64)`; the `u64` is the host's own minting (an
/// entity's bits, say), and this is how the session's
/// [`Modulation`](crate::Phase::Modulation) phase reads it ([`ModInput`](crate::ModInput)).
pub trait ModKeys {
    /// The node a target's key names.
    fn node(&self, target: u64) -> Option<NodeKey>;
    /// The edge a target's key names (`TargetKey::Edge`), whose gain the
    /// target modulates. A host with no edge targets leaves the default,
    /// which names none.
    fn edge(&self, target: u64) -> Option<crate::Edge> {
        let _ = target;
        None
    }
    /// What feeds source `source`'s node.
    fn feed(&self, source: tutti_modulation::SourceKey) -> Option<tutti_nodes::Feed>;
    /// The cell holding route `route`'s depth, when the host moves it between
    /// applies: the route's lane reads the depth from it at every
    /// evaluation, so a depth edit republishes no program. A host that
    /// declares its depths with its routes leaves the default, which names
    /// none.
    fn depth(
        &self,
        route: tutti_modulation::RouteKey,
    ) -> Option<tutti_rt::Param<tutti_units::Depth>> {
        let _ = route;
        None
    }
}

/// `tutti_nodes::ModHost` over the graph's nodes and the host's keys: a
/// target's key names its node, a native target's knob is that node's
/// `Param` cell, a plugin target feeds the node's event input 0 and starts at
/// the plugin's mirrored value (the session's record of it,
/// `Plugin::param_values`).
struct ModHostOver<'a> {
    values: &'a ValueTable,
    #[cfg(feature = "plugin")]
    plugins: &'a BTreeMap<NodeKey, crate::plugin::HostedPlugin>,
    keys: &'a dyn ModKeys,
    /// Each plugin target the apply resolved, by node and parameter: which
    /// plugin parameters a lane drives (`ModState::plugin_lanes`).
    plugin_targets: std::cell::RefCell<PluginTargets>,
}

/// A plugin parameter's modulation target, by its node and parameter id.
pub(crate) type PluginTargets =
    BTreeMap<(NodeKey, tutti_units::ParamId), tutti_modulation::TargetKey>;

impl tutti_nodes::ModHost for ModHostOver<'_> {
    fn target(&self, target: tutti_modulation::TargetKey) -> Option<tutti_nodes::Target> {
        let tutti_modulation::TargetKey::Param {
            node: bits,
            param: addr,
        } = target
        else {
            // A node's mix: the node the host's key names. The mix's knob is
            // the unit's own cell, which the reconciler takes from the
            // editor: `MixTarget` is the one writer.
            if let tutti_modulation::TargetKey::Mix(bits) = target {
                return Some(tutti_nodes::Target::Mix {
                    node: self.keys.node(bits)?,
                });
            }
            // An edge's gain: the edge the host's key names, if it names one.
            let tutti_modulation::TargetKey::Edge(key) = target else {
                return None;
            };
            // The gain's knob is the edge's own cell, which the reconciler
            // takes from the editor: `GainTarget` is the one writer.
            return Some(tutti_nodes::Target::Edge {
                id: Graph::<f32>::edge_id(self.keys.edge(key)?)?,
            });
        };
        let node = self.keys.node(bits)?;
        Some(match addr.as_unit() {
            // A native param, at any index: the node's own cell for that
            // address is the lane's knob.
            Some(addr) => tutti_nodes::Target::Native {
                node,
                knob: self.values.cell(node, addr),
            },
            // A plugin target waits for its mirror: a lane built without
            // a knob could not take one later.
            None => {
                let tutti_units::ParamAddr::Id(id) = addr else {
                    return None;
                };
                // A node the session hosts no plugin at has no mirror: its
                // lane has no knob to start from.
                #[cfg(feature = "plugin")]
                let knob = self
                    .plugins
                    .get(&node)
                    .map_or(Some(None), |p| p.param_value(id))?
                    .map(f64::from);
                #[cfg(not(feature = "plugin"))]
                let knob = None;
                self.plugin_targets.borrow_mut().insert((node, id), target);
                tutti_nodes::Target::Foreign {
                    node,
                    port: 0,
                    knob,
                }
            }
        })
    }

    fn feed(&self, source: tutti_modulation::SourceKey) -> Option<tutti_nodes::Feed> {
        self.keys.feed(source)
    }

    fn depth(
        &self,
        route: tutti_modulation::RouteKey,
    ) -> Option<tutti_rt::Param<tutti_units::Depth>> {
        self.keys.depth(route)
    }
}

#[cfg(all(test, feature = "plugin"))]
mod tail_tests {
    use super::*;
    use std::sync::Arc;
    use tutti_graph::ForkByClone;
    use tutti_nodes::testing::Const;
    use tutti_tempo::TempoMap;
    use tutti_units::Bpm;

    fn graph() -> Graph {
        graph_at(SampleRate::SR_48K)
    }

    fn graph_at(rate: SampleRate) -> Graph {
        let map = TempoMap::constant(Bpm::of(120.0)).expect("a tempo");
        Graph::new(
            ChannelLayout::STEREO,
            2,
            rate,
            MaxBlock::of(Samples::new(64)),
            None,
            tutti_timeline::TempoReader::fixed(Arc::new(map)),
        )
    }

    /// **The tail and latency limits are the graph's rate's**: sixty and ten
    /// seconds in frames at the graph's rate. A tail at the limit is a figure and
    /// one past it is `Unbounded`; a tail of 4 000 000 frames (83 s at 48 kHz,
    /// 41.7 s at 96 kHz) is `Unbounded` at 48 kHz and a figure at 96 kHz. A
    /// reported latency at the limit is kept and one past it is clamped to it.
    #[test]
    fn plugin_refreshes_use_the_graphs_rate() {
        use tutti_graph::{MAX_NODE_LATENCY, MAX_NODE_TAIL};
        for rate in [SampleRate::SR_48K, SampleRate::SR_96K] {
            let tail_limit = MAX_NODE_TAIL.to_samples_ceil(rate);
            let latency_limit = MAX_NODE_LATENCY.to_samples_ceil(rate);
            // The two rates' limits, spelled out.
            let (want_tail, want_latency) = if rate == SampleRate::SR_48K {
                (Samples::new(2_880_000), Samples::new(480_000))
            } else {
                (Samples::new(5_760_000), Samples::new(960_000))
            };
            assert_eq!((tail_limit, latency_limit), (want_tail, want_latency));

            let tail_after = |frames: Samples| {
                let mut g = graph_at(rate);
                let node = g.add(ForkByClone(Const::mono(0.0)));
                g.commit();
                assert!(g.refresh_node_tail(node, Tail::Finite(frames)), "{rate:?}");
                g.node_tail(node).expect("a node")
            };
            assert_eq!(tail_after(tail_limit), Tail::Finite(tail_limit), "{rate:?}");
            assert_eq!(
                tail_after(tail_limit + Samples::new(1)),
                Tail::Unbounded,
                "{rate:?}"
            );
            let four_million = Samples::new(4_000_000);
            let finite_here = four_million <= tail_limit;
            assert_eq!(finite_here, rate == SampleRate::SR_96K);
            let want = if finite_here {
                Tail::Finite(four_million)
            } else {
                Tail::Unbounded
            };
            assert_eq!(tail_after(four_million), want, "{rate:?}");

            let latency_after = |frames: Samples| {
                let mut g = graph_at(rate);
                let node = g.add(ForkByClone(Const::mono(0.0)));
                g.commit();
                assert!(
                    g.refresh_node_latency(node, Latency::new(frames)),
                    "{rate:?}"
                );
                g.node_latency(node).expect("a node")
            };
            assert_eq!(latency_after(latency_limit), latency_limit, "{rate:?}");
            assert_eq!(
                latency_after(latency_limit + Samples::new(1)),
                latency_limit,
                "{rate:?}"
            );
        }
    }

    /// **A node's tail ceiling follows the figure the host hands the graph,
    /// a finite figure past what a node may declare becomes `Unbounded`, and edits nothing when it has
    /// not moved**: an idle poll must not recompile and republish every
    /// frame. A node the graph does not have is not touched.
    #[test]
    fn refresh_node_tail_follows_the_figure_and_bounds() {
        let mut g = graph();
        let node = g.add(ForkByClone(Const::mono(0.0)));
        g.commit();
        assert!(!g.has_uncommitted_edits());

        let new = Tail::Finite(Samples::new(500));
        assert!(g.refresh_node_tail(node, new), "a moved tail is taken");
        assert_eq!(g.node_tail(node), Ok(new));
        assert!(
            g.has_uncommitted_edits(),
            "the next commit moves the ceiling"
        );
        g.commit();
        assert!(
            !g.refresh_node_tail(node, new),
            "an unmoved tail edits nothing"
        );
        assert!(!g.has_uncommitted_edits());

        let limit = tutti_graph::MAX_NODE_TAIL.to_samples_ceil(g.editor.prepare().sample_rate());
        let past = Tail::Finite(limit + Samples::new(1));
        assert!(g.refresh_node_tail(node, past));
        assert_eq!(
            g.node_tail(node),
            Ok(Tail::Unbounded),
            "a figure past the most a node declares is unbounded: not clamped, not refused"
        );
        g.commit();
        assert!(
            !g.has_uncommitted_edits(),
            "the graph took it: nothing is left to retry"
        );
        assert!(
            !g.refresh_node_tail(node, Tail::Unbounded),
            "the same figure again edits nothing"
        );
        assert!(g.refresh_node_tail(node, Tail::None));
        assert_eq!(g.node_tail(node), Ok(Tail::None));

        assert!(
            !g.refresh_node_tail(NodeKey(9_999), Tail::None),
            "no such node"
        );
    }
}

#[cfg(test)]
mod root_fade_tests {
    use super::*;
    use std::sync::Arc;
    use tutti_buffers::Bed;
    use tutti_graph::{FadeLaw, ForkByClone, OutPort, PortSource, RootId};
    use tutti_nodes::testing::Const;
    use tutti_tempo::TempoMap;
    use tutti_units::Bpm;

    fn graph() -> (Graph, NodeKey) {
        let map = TempoMap::constant(Bpm::of(120.0)).expect("a tempo");
        let mut g = Graph::new(
            ChannelLayout::STEREO,
            2,
            SampleRate::SR_48K,
            MaxBlock::of(Samples::new(64)),
            None,
            tutti_timeline::TempoReader::fixed(Arc::new(map)),
        );
        let node = g.add(ForkByClone(Const::mono(1.0)));
        let node = NodeKey::from(node);
        g.editor.spec_mut().topology.set_outputs([
            PortSource::Node(OutPort { node, port: 0 }),
            PortSource::Zero,
        ]);
        g.commit();
        (g, node)
    }

    /// **A root declaration crossfades by the declick unless it is hard**: the
    /// plan of the commit that carries the next root also renders the old
    /// root's outputs, the old root's id and the root's width naming where
    /// they begin, over the declick length (5 ms, 240 frames at 48 kHz) along
    /// the equal-amplitude law; a hard declaration carries none.
    #[test]
    fn a_root_declaration_crossfades_for_the_declick_unless_hard() {
        let (mut g, _) = graph();
        g.declare_root(Some(Bed::Surround51.topology()), false)
            .expect("declares");
        assert!(matches!(g.commit(), Committed::Sent));
        let plan = g.editor.base().expect("a plan");
        let bank = plan.root_bank().expect("the old root is carried");
        assert_eq!(bank.old, RootId::GENESIS);
        assert_eq!(bank.base, 6, "the new root's width");
        assert_eq!(
            plan.rendered_outputs(),
            6 + 2,
            "and the old root's after it"
        );
        assert_eq!(plan.global_outputs(), 6);
        assert_eq!(
            (bank.fade.duration, bank.fade.law),
            (Samples::new(240), FadeLaw::EqualAmplitude)
        );

        g.declare_root(Some(Bed::Stereo.topology()), true)
            .expect("declares");
        assert!(matches!(g.commit(), Committed::Sent));
        let plan = g.editor.base().expect("a plan");
        assert_eq!(plan.root_bank(), None, "a hard declaration cuts");
        assert_eq!(plan.rendered_outputs(), 2);
    }

    /// **A session-owned swap fades where the shape fits and cuts where it
    /// does not**: a node of the running shape swaps in under a crossfade, and
    /// one of another width swaps in as a cut, neither refused.
    #[cfg(feature = "plugin")]
    #[test]
    fn a_session_owned_swap_fades_where_the_shape_fits_and_cuts_where_it_does_not() {
        for (incoming, fades) in [
            (Const::mono(2.0), 1),
            (Const::new(2.0, ChannelLayout::STEREO), 0),
        ] {
            let (mut g, node) = graph();
            g.collect();
            let swapped = g.swap_unit(node, ForkByClone(incoming), None, OnShape::Cut);
            assert!(swapped.is_ok(), "never refused for its shape");
            assert!(matches!(g.commit(), Committed::Sent));
            assert_eq!(g.editor.fades_in_flight(), fades);
        }
    }
}
