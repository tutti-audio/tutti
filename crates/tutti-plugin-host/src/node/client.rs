//! `InProcessClient`: the graph node (`node.rs`) that drives an in-process
//! plugin of any hosting ([`Loaded`]: an [`InProcess`](tutti_plugin_types::InProcess)
//! format, or an out-of-process plugin, `tutti-plugin-ipc`'s `Ipc`) from the host audio thread.
//!
//! The audio thread renders through the Processor the node's realtime end
//! ([`tutti_rt::Rt`]) holds, and takes no lock. The plugin's control side
//! (`super::instance::Instance`: its `Lifecycle`) is behind an `Arc<Mutex<_>>`
//! that only control threads take: the handle, the node's `shape` and
//! `prepare`, a fork. A block with no Processor in hand (it was recalled for a
//! rate change, or the plugin refused its rate) renders silence, counted as an
//! unlent block; a block the Processor failed renders silence too, counted as a
//! failed one, with its cause logged; an entry the plugin refused within a block
//! that ran is counted too ([`BlockLosses`](crate::BlockLosses), read through
//! [`NodeControls::block_losses`](crate::NodeControls::block_losses)).
//!
//! A block is one [`Processor::process`]: the graph's channel slices as an
//! `InputOutput` (`tutti-buffers`), the block's MIDI, parameter chunk and transport as a
//! [`ProcessContext`], and a [`ProcessOutput`] the client owns and clears each
//! block. A block is allocation-free in steady state (verified by the
//! `assert_no_alloc` test in `tests/`).

use crate::node_losses::LossCounts;
use crate::param_delivery::chunk_offset;
use std::marker::PhantomData;
use std::sync::Arc;
use tutti_buffers::Float;
use tutti_graph::UmpAt;
use tutti_plugin_types::{
    Loaded, MainThreadMarker, MainWorkHandle, PluginDescriptor, PluginError, PluginRef,
    ProcessContext, ProcessOutput, Processor, process_block, sanitize,
};
use tutti_rt::Rt;

use arrayvec::ArrayVec;
use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_graph::{Bus, Buses, Cx, InEvents, Offset, OutEvents, Shape};
use tutti_plugin_types::{Layout, PluginBus, PluginWiring};
use tutti_rt::AtomicCell;
use tutti_rt::Mutex;
use tutti_tempo::TempoView;
use tutti_timeline::TempoReader;
use tutti_units::{SampleRate, Samples};

use super::control_backend::Reporting;
use super::host_edits::EditPort;
use super::instance::{Figures, Instance, Lost};
use crate::{BlockTransport, TransportSource, TransportWalk};
use crate::{ParamDelivery, PluginControls, PluginTimeSignature, SegmentPoints};
use tutti_plugin_types::MidiIntake;
use tutti_plugin_types::{
    ChordChanges, ExpressiveContext, Features, MIDI_STACK_CAPACITY, MidiEventVec, ParameterChanges,
    PluginTooWide, ScaleChanges, TransportChange, TransportChanges, TransportInfo,
};
use tutti_units::ParamId;

/// A plugin running inside the host process, as a graph node, whatever its
/// format `L`, activated at the width of the graph `F` it joins. The default
/// `F = f32` is the common width and converts nothing; it is staging until
/// every caller names its graph's float.
///
/// Created by [`load_client`](crate::load_client), or by
/// `tutti-plugin`'s `Plugin::open` for a `.vst`/`.dll`/`.so`
/// file when the `vst2` feature is enabled. MIDI arrives on the node's event
/// input and the plugin's MIDI-out leaves on an event output (for a plugin that
/// declares [`Features::MIDI_OUT`]).
///
/// There is no process isolation: a plugin crash takes the host down. The
/// audio thread renders through the Processor it was lent and takes no lock;
/// while it holds none it outputs silence (counted in
/// [`BlockLosses::unlent`](crate::BlockLosses::unlent)). Steady state processing
/// does not allocate.
///
/// A graph holding this node forks (`Editor::fork`, which an offline export
/// takes) **by state transfer**: a second instance of the plugin, from the same
/// library, restored from this one's saved state. See
/// [`PluginNode::fork_instance`]. The client becomes a graph node by
/// [`bind`](Self::bind)ing it to the project's tempo map.
pub struct InProcessClient<L: Loaded, F: Float = f32> {
    /// The plugin's control side. Control threads only: the audio thread
    /// renders through `rt`.
    pub(super) inner: Arc<Mutex<Instance<L>>>,
    /// The realtime end the plugin's Processor is lent to.
    pub(super) rt: Rt<L::Processor>,
    /// Whether the plugin is lost, read with no lock ([`Instance::lost_latch`]).
    lost: Arc<Lost>,
    /// The plugin's latency and tail as the control side last read them
    /// ([`Instance::figure_cell`]): what `shape` declares, with the instance
    /// locked or not.
    pub(super) figures: Arc<Figures>,
    /// Where a pump of the instance delivers what it finds, for a node whose
    /// plugin has a handle to hear it; the successor a [`Rebinder`](super::Rebinder) builds
    /// delivers through the same. `None` for an out-of-process plugin, whose own
    /// listener reports it.
    pub(super) reporting: Option<Reporting>,
    /// The host's parameter writes (`host_edits`), drained at the head of each
    /// block a Processor renders. `None` for a plugin whose writes do not come
    /// through this node (an out-of-process one's go to its bridge).
    pub(super) edits: Option<EditPort>,
    pub(super) descriptor: PluginDescriptor,
    /// Where the plugin came from, for an in-process one: what a fork loads
    /// again. `None` for a hosting that reopens another way.
    pub(super) reload: Option<Reload>,
    /// What the Processor adds to the plugin's own latency
    /// ([`Processor::latency`]), as the last `prepare` read it: the out-of-process
    /// pipeline's chunk.
    pub(super) pipeline: Samples,
    /// An out-of-process plugin's host-side controls (`PluginControls`): its latency and
    /// tail are re-reported by the bridge's thread into cells the controls
    /// share, so `shape` reads the figures from them, `retune` writes the
    /// pipeline's chunk and the rate into them, and the meter, the delivery
    /// counters and the chunk counters are the node's own. `None` for an
    /// in-process plugin, whose figures are the control side's.
    pub(super) controls: Option<PluginControls>,
    /// The switch of an out-of-process fork's per-chunk wait, which this
    /// node's `prepare` sets from the graph's render mode on every prepare
    /// ([`drive_wait`](Self::drive_wait)). `None` for any node that is not
    /// such a fork: nothing waits.
    pub(super) wait_switch: Option<tutti_plugin_types::WaitSwitch>,
    /// The parameters the delivery has a slot for, and the spare slots it
    /// learns unlisted ones into: what a successor node's delivery is made from.
    pub(super) parameters: Vec<ParamId>,
    pub(super) spares: usize,
    /// The pin counts the plugin declared when the node was built: what `shape`
    /// and `process` size by. Moves are reported through the handle
    /// (`PluginInvalidation`), not here.
    pub(super) declared: Declared,
    /// The event input of the node's shape: MIDI and parameter events.
    pub(super) event_in: InEvents,
    /// The event output of the node's shape, for a plugin that declared MIDI out.
    pub(super) event_out: Option<OutEvents>,
    /// The block's MIDI in: its event input's.
    pub(super) midi: MidiEventVec,
    /// Takes the event input's MIDI into `midi`, carrying across blocks
    /// which SysEx lost a packet to its cap.
    pub(super) intake: MidiIntake,
    /// What the plugin emits in a block: owned here and cleared at the head of
    /// each block, so its storage is the one allocation, made at construction.
    pub(super) out: ProcessOutput,
    /// The block's transport, read from the graph's [`Cx`] (live and in a
    /// fork alike), with the project meter installed through
    /// [`NodeControls::set_meter`](crate::NodeControls::set_meter).
    pub(super) transport: TransportFeed,
    /// What the loader reported for this plugin, as the gate `transport` is
    /// read against. Stored rather than passed in per block so the node's
    /// declared capability and its delivered behaviour read from one value.
    pub(super) features: Features,
    pub(super) sample_rate: SampleRate,
    /// A rate the graph handed this node that the plugin has not been told
    /// about yet; empty when there is none.
    ///
    /// Telling a plugin its rate is a deactivation and an activation, which
    /// needs the main thread. A `prepare` on it runs the change itself; one off
    /// it parks the rate here ([`queue_sample_rate`]) for the main thread's
    /// pump ([`drain_sample_rate`], the editor idle pump), the same deferral
    /// the out-of-process client gets from its command queue.
    ///
    /// Shared with the control backend, which drains it.
    pub(super) pending_sample_rate: Arc<AtomicCell<Option<SampleRate>>>,
    /// Who tells the plugin the rate the node is prepared at.
    pub(super) rate_delivery: RateDelivery,
    /// A fork's rate could not be told to the plugin (its `prepare` ran off the
    /// main thread), so it is at a rate other than the node's: the node renders
    /// silence until a `prepare` on the main thread tells it.
    pub(super) rate_refused: bool,
    /// The render mode the last `prepare` carried (realtime before one): a
    /// `prepare` tells the plugin its mode only when the one it carries
    /// changed, so a re-prepare at the same mode leaves what the session
    /// announced since.
    prepared_mode: tutti_graph::RenderMode,
    /// The events per block the node declares it accepts on its event input
    /// (`Shape::event_input_capacity`): `MIDI_STACK_BOUND`, held to `event_room`.
    pub(super) event_input: std::num::NonZeroU32,
    /// The room of the last Processor this node saw (`Processor::event_input`),
    /// kept while the Processor is away (recalled for a restart or lent to the
    /// realtime end), so the declared bound never rises to the intake's alone.
    pub(super) event_room: std::num::NonZeroU32,
    /// The parameter events on the event input, as changes only: a slot per
    /// parameter the plugin declared at load and no spares (`closed`: the
    /// node addresses only its declared parameters), the value last
    /// delivered to the instance in each (`host::node`'s `param_delivery`,
    /// the out-of-process node's own). The block is its chunk.
    ///
    /// The node is not `Clone`: a clone would drive the same instance, so
    /// two of them in one graph write it in an unordered order (the
    /// parallel executor), each with its own record of what it sent, and a
    /// `ForkByClone` of it would render an export through the live
    /// instance. A fork is a second instance (`fork.rs`).
    pub(super) params: ParamDelivery,
    /// The curve segments in flight on the event input, sent as points until
    /// each lands. Slots sized at construction, one per parameter slot.
    pub(super) segments: SegmentPoints,
    /// Frames into the Processor's chunk, from `prepare` and every `reset`: where
    /// a block of a chunked Processor ([`Processor::chunk`]) is cut. Always
    /// zero for one that is not.
    pub(super) phase: Samples,
    /// The last block of a chunked Processor returned `Status::Idle`: the
    /// statuses the next block reads are of chunks shipped before the node
    /// parked, so it cannot say the plugin still sleeps. A woken node runs
    /// until the plugin's answer to what that block shipped has come back.
    pub(super) idled: bool,
    /// The walk of the transport across the chunk being filled, between pieces.
    pub(super) walk: TransportWalk,
    /// The transport at the chunk's first frame (final at the piece that
    /// completes it), and the chunk's changes so far, on chunk frames: what
    /// every piece of a chunked Processor is handed.
    pub(super) chunk_info: TransportInfo,
    pub(super) changes: TransportChanges,
    /// MIDI the chunked Processor holds for the chunk in flight (taken by the
    /// pieces so far), which a reset or a re-prepare drops.
    pub(super) chunk_midi: u64,
    /// Dropped MIDI the next block reports to the executor (`Io::count_lost`):
    /// a reset has no `Io`.
    pub(super) owed_lost: u64,
    /// The chords and scales in the piece being rendered, for a plugin that
    /// takes sequencer context.
    pub(super) chords: ChordChanges,
    pub(super) scales: ScaleChanges,
    /// The MIDI-out of the pieces rendered so far in this block, on the
    /// block's frames.
    pub(super) out_events: MidiEventVec,
    /// The sample width the Processor was activated at: `F`, which
    /// [`Widths::negotiate`](tutti_plugin_types::Widths::negotiate) settled
    /// at load.
    width: PhantomData<fn() -> F>,
}

/// A node dropped while its realtime end holds the Processor and a recall is
/// raised (a [`Pending`](super::Pending) commit raises one) hands it back
/// through the handoff instead of dropping it: the control side collects it at
/// the instance's next pump. Without a raised recall the Processor drops with
/// the end, as for any node that leaves the graph.
///
/// The node is off the audio thread by then (a node leaves the graph before it
/// is dropped), so this call is the end's only user. The handoff runs
/// [`Processor::stop`] on the dropping thread, the control thread, as a
/// retune's poll does between blocks.
///
/// It reads no lock: the instance may be held by the control thread that
/// drops the node. A plugin that is lost (its flag, a latch set when its
/// lifecycle poisons, through which nothing else is published) has no
/// controller left to stop under, so its node does not run `stop`: the Processor drops with the end, as the Processor of a lost plugin
/// does when the session retires its node.
impl<L: Loaded, F: Float> Drop for InProcessClient<L, F> {
    fn drop(&mut self) {
        if self.lost.is_lost() || self.rt.held().is_none() {
            return;
        }
        let _ = self.rt.poll(Processor::stop);
    }
}

/// Omits `inner`: formatting must not wait on the control side's lock.
impl<L: Loaded, F: Float> std::fmt::Debug for InProcessClient<L, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessClient")
            .field("descriptor", &self.descriptor)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

/// What the plugin declared when the node was built.
#[derive(Clone, Copy, Debug)]
pub(super) struct Declared {
    /// The plugin's channels as the node's ports: its buses as the node's
    /// buses, in bus order, and the channels past them as ports.
    pub(super) ports: Shape,
}

impl Declared {
    /// The ports with the node's events: one input (MIDI and parameter
    /// events) and an output for a plugin that declared MIDI out.
    pub(super) fn event_shape(self, features: Features) -> Shape {
        self.ports
            .events(1, u16::from(features.contains(Features::MIDI_OUT)))
            .param_events()
    }

    /// What the plugin reported at load.
    ///
    /// # Errors
    ///
    /// [`PluginTooWide`] when the plugin has more channels in a direction than
    /// a block serves.
    pub(super) fn of(loaded: &PluginWiring) -> Result<Self, PluginTooWide> {
        let mut declared = Self::from_layout(&Layout::from(loaded))?;
        // A plugin with a bypass of its own keeps running in
        // `NodeMode::Bypassed`: the host writes that bypass
        // ([`PluginHandle::set_bypassed`](crate::PluginHandle::set_bypassed)).
        if loaded.own_bypass.is_some() {
            declared.ports = declared.ports.own_bypass();
        }
        Ok(declared)
    }

    /// What the plugin reports now: the ports of the layout the lifecycle last
    /// read ([`State::layout`](tutti_plugin_types::State::layout)). A bypass of the plugin's own is not part of
    /// a layout: the caller keeps it ([`Rebinder`](super::Rebinder) does).
    ///
    /// # Errors
    ///
    /// [`PluginTooWide`] when the layout has more channels in a direction than
    /// a block serves.
    pub(super) fn from_layout(layout: &Layout) -> Result<Self, PluginTooWide> {
        layout.check_width()?;
        Ok(Self {
            ports: Shape::buses(buses_of(&layout.inputs), buses_of(&layout.outputs)).ports(
                ChannelLayout::clamped(layout.total_inputs()),
                ChannelLayout::clamped(layout.total_outputs()),
            ),
        })
    }
}

/// The node's buses for a plugin's, in bus order, each with the role the
/// format gave it.
///
/// A bus without a [`placement`](PluginBus::placement) (unknown, or one that
/// does not have its width's channels) is `Unassigned` at that width: a count is
/// not a placement. The list stops at the first bus past
/// [`MAX_BUSES`](tutti_graph::MAX_BUSES); the plugin's remaining channels are
/// ports the node still has.
fn buses_of(plugin: &[PluginBus]) -> Buses {
    let mut buses = Buses::NONE;
    for bus in plugin {
        let placed = bus.placement().and_then(|t| BusTopology::try_from(t).ok());
        let Some(topology) = placed.or_else(|| BusTopology::unassigned(bus.channels)) else {
            break;
        };
        match buses.with(Bus::new(topology, bus.role)) {
            Some(more) => buses = more,
            None => break,
        }
    }
    buses
}

/// Who tells the plugin the rate the node is prepared at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RateDelivery {
    /// Off the main thread, pending in `pending_sample_rate` for the main
    /// thread's editor pump (`Backend::editor_idle`) to dispatch: the live
    /// instance.
    Pending,
    /// Off the main thread, refused (silence until a `prepare` on it): a fork.
    /// Nothing pumps a fork's editor (it has no handle), so a pending rate
    /// would never reach it.
    InPrepare,
}

/// The transport a block hands the plugin: what `audioMasterGetTime`
/// answers while the plugin processes it.
///
/// One path for the live node and its fork: the transport the block's input
/// was rendered at ([`Cx::transport_at`] at its first frame, the node's
/// compiled arrival latency earlier), mapped the way the out-of-process
/// plugin node maps it (`from_cx`), with the project
/// meter for the signature and bar. For an offline fork the `Cx` is the
/// render's. The snapshot is built in `process` before the plugin is
/// entered: the Processor publishes it where the plugin reads it (VST2:
/// the cell `audioMasterGetTime` reads), then calls the plugin.
///
/// A clone shares the meter slot and reads it through a reader of its own
/// (`Publisher::reader` registers one on the same cell), and carries the counter
/// and the last snapshot as they stand. Only a test clones a feed.
#[derive(Clone, Debug)]
pub(super) struct TransportFeed {
    /// The node's meter slot, shared with its controls
    /// ([`InProcessClient::controls`]), so a meter installed through them
    /// reaches the node the audio thread runs; a fork has a slot of its own
    /// ([`forked`](Self::forked)). Control side only: the block reads
    /// through `source`.
    meter: PluginTimeSignature,
    /// The slot's reader and the clocks (`TransportSource`), the out-of-process
    /// node's own.
    source: TransportSource,
    /// The last snapshot handed out: what an empty block (no frame to read
    /// the transport at) repeats. `None` before one.
    last: Option<TransportInfo>,
}

impl TransportFeed {
    /// A handle on the node's meter cell: what its controls install through.
    pub(super) fn meter(&self) -> PluginTimeSignature {
        self.meter.clone()
    }

    /// No meter installed, nothing rendered.
    pub(super) fn new() -> Self {
        Self::over(PluginTimeSignature::default())
    }

    /// A feed reading `meter`'s cell through a reader registered here,
    /// nothing rendered, on the host clock. Control thread:
    /// registering may allocate.
    fn over(meter: PluginTimeSignature) -> Self {
        Self {
            source: TransportSource::new(meter.reader()),
            meter,
            last: None,
        }
    }

    /// A fork's feed: the meter installed now, in a slot of its own (a meter
    /// installed on the live node afterwards does not reach the fork, as for
    /// the out-of-process node's fork), a counter from zero, and, for an
    /// `offline` render, no host clock.
    pub(super) fn forked(&self, offline: bool) -> Self {
        let mut feed = Self::over(PluginTimeSignature::holding(self.meter.installed()));
        if offline {
            feed.source.go_offline();
        }
        feed
    }

    /// No host clock from here on: the render runs at its own speed, so
    /// `nanoSeconds` must not be claimed ([`TransportSource::go_offline`]).
    pub(super) fn go_offline(&mut self) {
        self.source.go_offline();
    }

    /// What one block reads of the transport, for a node that reads it at
    /// several frames ([`TransportSource::block`]); count the block's frames
    /// after it with [`advance`](Self::advance).
    pub(super) fn lend(&mut self, features: Features) -> BlockTransport<'_> {
        self.source.block(features)
    }

    /// Count `frames` rendered ([`TransportSource::advance`]).
    pub(super) fn advance(&mut self, frames: usize) {
        self.source.advance(frames);
    }

    /// The snapshot for a block of `frames`, then count them. A plugin that
    /// did not declare [`Features::TRANSPORT`] is handed a stopped snapshot at
    /// the block's rate; an empty block, which has no frame to read the
    /// transport at, repeats the last one (a stopped one before any). Audio thread: one wait-free
    /// borrow (the node's meter cell), no allocation and no free.
    pub(super) fn block(
        &mut self,
        cx: &Cx<'_>,
        frames: usize,
        features: Features,
        tempo: &mut TempoView<'_>,
    ) -> TransportInfo {
        if !features.contains(Features::TRANSPORT) {
            self.source.advance(frames);
            return TransportInfo::new(cx.env.sample_rate);
        }
        let Some(at) = Offset::new(Samples::new(0), Samples::new(frames)) else {
            return self
                .last
                .unwrap_or_else(|| TransportInfo::new(cx.env.sample_rate));
        };
        let info = self.source.block(features).at(cx, at, frames, tempo);
        self.source.advance(frames);
        self.last = Some(info);
        info
    }
}

/// An in-process client bound to the project's tempo map
/// ([`InProcessClient::bind`]): the graph node. It owns the reader, lends
/// the map once per block, read through a view at the block's rate. Only this type is a
/// [`Node<F>`](tutti_graph::Node) and an [`IntoNode<F>`](tutti_graph::IntoNode), for the
/// graph float `F` the plugin was activated at (a plugin that does not process
/// `F` is a [`WidthError`](crate::WidthError) at load, never converted);
/// the loaded client is neither. It derefs to the client for its control
/// surface (`controls`, `set_meter`, ...).
///
/// The default `F = f32` is the common width and converts nothing; it is
/// staging until every caller names its graph's float.
pub struct PluginNode<L: Loaded, F: Float = f32> {
    pub(super) client: InProcessClient<L, F>,
    pub(super) tempo: TempoReader,
}

impl<L: Loaded, F: Float> std::fmt::Debug for PluginNode<L, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginNode").finish_non_exhaustive()
    }
}

impl<L: Loaded, F: Float> std::ops::Deref for PluginNode<L, F> {
    type Target = InProcessClient<L, F>;
    fn deref(&self) -> &InProcessClient<L, F> {
        &self.client
    }
}

impl<L: Loaded, F: Float> std::ops::DerefMut for PluginNode<L, F> {
    fn deref_mut(&mut self) -> &mut InProcessClient<L, F> {
        &mut self.client
    }
}

/// Park `rate` for the main thread to dispatch. Audio-thread safe: one
/// `Release` store, no lock and no allocation.
///
/// Last write wins. A rate superseded before anyone drained it never reached
/// the plugin, so dropping it loses nothing — the plugin only needs to hear the
/// rate it is about to run at.
pub(super) fn queue_sample_rate(cell: &AtomicCell<Option<SampleRate>>, rate: SampleRate) {
    cell.store(Some(rate));
}

/// Dispatch a pending rate, if any, and report whether one reached the plugin.
///
/// Main thread only: `Instance::set_sample_rate` deactivates and activates the
/// plugin, which allocates. This is the drain half of the deferral
/// [`queue_sample_rate`] opens. The Processor is on the audio thread, so the
/// change starts with a recall and finishes at a later pump, once the audio
/// thread has handed it back.
pub(super) fn drain_sample_rate<L: Loaded>(
    main: MainThreadMarker,
    cell: &AtomicCell<Option<SampleRate>>,
    inner: &Mutex<Instance<L>>,
) -> bool {
    let Some(rate) = cell.swap(None) else {
        return false;
    };
    // A refusal is kept in the instance and reported by the control side's next
    // pump (`Instance::take_reports`), so the result is not read here.
    let _ = inner.lock().set_sample_rate(main, rate, None);
    true
}

/// What a fork loads a plugin again from: where it came from, and the collector
/// of its owner, which the new instance hands its close to.
#[derive(Clone)]
pub(super) struct Reload {
    pub(super) origin: PluginRef,
    pub(super) work: MainWorkHandle,
}

impl<L: Loaded, F: Float> InProcessClient<L, F> {
    /// A client over `inner`, which it locks once for the plugin's lost flag:
    /// a caller must not hold that lock.
    pub(super) fn new(
        inner: Arc<Mutex<Instance<L>>>,
        rt: Rt<L::Processor>,
        descriptor: PluginDescriptor,
        reload: Option<Reload>,
        declared: Declared,
        features: Features,
        sample_rate: SampleRate,
        pending_sample_rate: Arc<AtomicCell<Option<SampleRate>>>,
        counters: Option<Arc<crate::DeliveryCounters>>,
        parameters: impl IntoIterator<Item = ParamId>,
        spares: usize,
        controls: Option<PluginControls>,
        pipeline: Option<Samples>,
        event_room: Option<std::num::NonZeroU32>,
    ) -> Self {
        // A slot per declared parameter: every parameter can move in one
        // block. Its point room is sized in `prepare`. `spares` is how many
        // addresses the plugin did not list the delivery may learn (none:
        // the node addresses only its declared parameters).
        let mut rt = rt;
        // The pipeline is the Processor's own figure; a node built over an end
        // the Processor has not reached yet is given the one its predecessor
        // read, and keeps it until a `prepare` that retunes the Processor
        // reads it again.
        let pipeline = pipeline.unwrap_or_else(|| {
            rt.poll(Processor::stop)
                .map_or(Samples::new(0), |processor| processor.latency())
        });
        if let Some(controls) = &controls {
            controls.set_pipeline(pipeline);
        }
        // A served plugin's controls own the counters; an in-process node's are
        // the loader's, or a predecessor's successor's own.
        let counters = controls
            .as_ref()
            .map(PluginControls::delivery_counters)
            .or(counters)
            .unwrap_or_default();
        let parameters: Vec<ParamId> = parameters.into_iter().collect();
        let params = ParamDelivery::with_spares(parameters.iter().copied(), counters, spares);
        let transport = controls
            .as_ref()
            .map_or_else(TransportFeed::new, |c| TransportFeed::over(c.meter()));
        let mut segments = SegmentPoints::default();
        segments.prepare(params.room());
        let (lost, figures) = {
            let instance = inner.lock();
            (instance.lost_latch(), instance.figure_cell())
        };
        // The Processor's room: the one on offer, else the predecessor's, else
        // the node's own intake (a node built before its Processor reached it,
        // with no predecessor).
        let event_room = rt
            .poll(Processor::stop)
            .map(|processor| processor.event_input())
            .or(event_room)
            .unwrap_or(tutti_plugin_types::MIDI_STACK_BOUND);
        let event_input = event_room;
        let shape = declared.event_shape(features);
        let event_in = shape.event_input(0).expect("one event input is declared");
        let event_out = shape.event_output(0);
        Self {
            inner,
            rt,
            lost,
            figures,
            reporting: None,
            edits: None,
            descriptor,
            reload,
            pipeline,
            declared,
            event_in,
            event_out,
            midi: MidiEventVec::new(),
            intake: MidiIntake::default(),
            out: ProcessOutput::default(),
            transport,
            controls,
            wait_switch: None,
            parameters,
            spares,
            features,
            sample_rate,
            pending_sample_rate,
            rate_delivery: RateDelivery::Pending,
            rate_refused: false,
            prepared_mode: tutti_graph::RenderMode::Realtime,
            event_input,
            event_room,
            params,
            segments,
            phase: Samples::new(0),
            walk: TransportWalk::default(),
            chunk_info: TransportInfo::new(sample_rate),
            changes: TransportChanges::default(),
            chunk_midi: 0,
            owed_lost: 0,
            idled: false,
            chords: ChordChanges::default(),
            scales: ScaleChanges::default(),
            out_events: MidiEventVec::new(),
            width: PhantomData,
        }
    }

    /// Binds the client to the reader of the project's tempo map it tells the
    /// plugin the tempo from, lent once per
    /// block: the step from a loaded client to a graph node, as
    /// `PluginClient::bind` (`tutti-plugin`'s) is for
    /// the out-of-process one. `Session` hands every plugin it inserts a
    /// reader of its own; a host with no session builds one over a fixed map
    /// ([`TempoReader::fixed`]). A fork clones the reader. Control thread.
    #[must_use]
    pub fn bind(self, tempo: TempoReader) -> PluginNode<L, F> {
        PluginNode {
            client: self,
            tempo,
        }
    }

    /// Sets the render mode the plugin runs in, as its
    /// [`render_mode_support`](tutti_plugin_types::Control::render_mode_support)
    /// says: told live, or a reconfiguration through the lifecycle whose
    /// activation carries the mode, or (where the plugin has no mode) kept for
    /// its next activation with the plugin left running.
    ///
    /// `false` off the main thread, where the plugin cannot be told, for a
    /// plugin with no mode, and when the plugin refuses the mode or the
    /// reconfiguration. Takes the instance lock, so call it from a control
    /// thread, not the audio thread.
    pub fn set_render_mode(&self, mode: tutti_plugin_types::RenderMode) -> bool {
        match MainThreadMarker::new() {
            Some(main) => self.inner.lock().set_render_mode(main, mode, None),
            None => false,
        }
    }

    /// Seeds the parameter delivery with what a live node had delivered
    /// ([`PluginControls::delivered`]): a value the live node sent is the
    /// fork's too, so a host value held still is not sent to a fork over the
    /// state it was loaded with.
    pub fn seed_delivery(&mut self, delivered: &[(ParamId, f64)]) {
        self.params.seed(delivered);
    }

    /// Drops the host clock the transport tells the plugin
    /// (`TransportInfo::system_time_ns`): for an offline render, which has none.
    /// A fork of the in-process node does this itself; an out-of-process plugin's fork
    /// (`tutti-plugin`) does it through this.
    pub fn go_offline(&mut self) {
        self.transport.go_offline();
    }
}

/// Moves the host's parameter writes since the last call into the block's
/// changes, at chunk frame `at`: each is sent though the plugin has the value
/// from this node already, since the write says where it stands, and carries
/// the epoch it was drained in. Allocation-free, and takes no lock.
pub(super) fn take_edits(edits: Option<&EditPort>, params: &mut ParamDelivery, at: usize) {
    let Some(edits) = edits else {
        return;
    };
    let offset = chunk_offset(at);
    edits.drain(|id, value, epoch| {
        params.write(id, offset, value.get(), epoch);
    });
}

/// Drops the host writes a state or preset load made stale since they were
/// drained, before the chunk that holds them is sent.
pub(super) fn void_stale_edits(edits: Option<&EditPort>, params: &mut ParamDelivery) {
    if let Some(edits) = edits {
        params.void_writes_before(edits.epoch());
    }
}

impl<L: Loaded, F: Float> InProcessClient<L, F> {
    /// The chunk in flight is dropped (a reset, a re-prepare): the walk of its
    /// transport ends, and the MIDI the Processor held for it is lost, counted
    /// now in the delivery's dropped MIDI and in the executor's at the next
    /// block.
    pub(super) fn drop_chunk(&mut self) {
        self.walk.clear();
        self.idled = false;
        let lost = std::mem::take(&mut self.chunk_midi);
        self.params.count_midi_dropped(lost);
        self.owed_lost += lost;
    }

    /// Offers `p`'s rate, block ceiling and quantum to the Processor
    /// ([`Processor::retune`]) and reads the latency it adds afterwards.
    /// `true` when it took them live: the plugin is told no restart, needs no
    /// main thread, and [`set_rate`](Self::set_rate) is not run. `false` when
    /// it has none in hand or does not follow the graph (an in-process one).
    pub(super) fn retune(&mut self, p: &tutti_graph::Prepare) -> bool {
        let rate = p.sample_rate();
        let max_block = Samples::new(p.max_block().get());
        let Some(processor) = self.rt.poll(Processor::stop) else {
            return false;
        };
        if !processor.retune(
            rate,
            max_block,
            p.quantum().map(tutti_buffers::MaxBlock::samples),
        ) {
            return false;
        }
        self.pipeline = processor.latency();
        if let Some(controls) = &self.controls {
            // What a host holding the controls reads as the declared latency
            // and stamps a source with.
            controls.set_pipeline(self.pipeline);
            controls.restamp(rate);
        }
        self.sample_rate = rate;
        self.rate_refused = false;
        self.inner.lock().retuned(rate);
        true
    }

    /// Whether no Processor is held by the realtime end: lent away or not yet
    /// arrived. A test's view of the recall the declared bound survives.
    #[cfg(test)]
    pub(super) fn rt_unlent(&mut self) -> bool {
        self.rt.held().is_none()
    }

    /// The events per block the node declares: the node's own intake
    /// (`MIDI_STACK_BOUND`), at most the room of the Processor's input storage.
    /// The room is read from the Processor when it is held here and kept across
    /// a recall.
    pub(super) fn event_bound(&mut self) -> std::num::NonZeroU32 {
        if let Some(processor) = self.rt.held() {
            self.event_room = processor.event_input();
        }
        tutti_plugin_types::MIDI_STACK_BOUND.min(self.event_room)
    }

    /// Sets the per-chunk wait of an out-of-process fork from the graph's
    /// render mode: armed for an offline graph, disarmed for any other. What
    /// the node's `prepare` does on every prepare, with no guard and whether
    /// or not the Processor is in hand; it is the only writer of the switch,
    /// and the plugin's own render mode ([`set_render_mode_from`](Self::set_render_mode_from))
    /// is a separate thing.
    pub(super) fn drive_wait(&self, p: &tutti_graph::Prepare) {
        if let Some(switch) = &self.wait_switch {
            switch.arm(p.render_mode().is_offline());
        }
    }

    /// Hands the node the switch of its fork's per-chunk wait: the one writer
    /// ([`WaitSwitch`](tutti_plugin_types::WaitSwitch), moved in and not
    /// cloneable), which its `prepare` then drives. Only a served fork's build calls
    /// this, after the node exists (the node is made by `client_over`, which
    /// takes no switch); a node that is not such a fork never gets one, and
    /// nothing about it waits.
    pub fn adopt_wait_switch(&mut self, switch: tutti_plugin_types::WaitSwitch) {
        self.wait_switch = Some(switch);
    }

    /// Tells the plugin the render mode `p` carries when it is not the mode
    /// the plugin runs in and not the one the last `prepare` carried. What the
    /// node's `prepare` does with its mode; main thread only, as a mode change
    /// is a main-thread call on every format.
    pub(super) fn set_render_mode_from(&mut self, p: &tutti_graph::Prepare) {
        let mode = p.render_mode();
        // The guard keeps a mode the handle or `Phase::RenderMode` set from
        // being undone by a re-prepare at the same mode, and by a first prepare
        // in realtime (`prepared_mode` starts there): a plugin set offline on
        // its handle stays offline through a realtime prepare until a prepare
        // carries a change.
        if self.prepared_mode == mode {
            return;
        }
        let mut inner = self.inner.lock();
        if inner.render_mode() != mode {
            // The node is out of any executor (`Node::prepare`'s contract), so
            // its realtime end is here: the exchange finishes in this call.
            let Some(main) = MainThreadMarker::new() else {
                // Not recorded, so the next prepare on the main thread tells it;
                // the handle's subscribers hear it as the rate's refusal is.
                inner.note_mode_not_applied(mode);
                return;
            };
            inner.set_render_mode(main, mode, Some(&mut self.rt));
        }
        self.prepared_mode = mode;
    }

    /// Take `sample_rate` as the rate blocks run at, and hand it to the
    /// plugin as [`RateDelivery`] says: pending for the main thread (see
    /// `pending_sample_rate`), or, on a fork, dispatched here. What the
    /// node's `prepare` does with its rate. (The transport snapshot's rate is
    /// the block's own, from its `Env`.)
    pub(super) fn set_rate(&mut self, sample_rate: tutti_units::SampleRate) {
        let rate = sample_rate;
        let changed = rate != self.sample_rate;
        self.sample_rate = rate;
        // Only on a change: a redundant bracket makes a plugin reallocate its
        // buffers.
        if !changed && !self.rate_refused {
            return;
        }
        // `prepare` has no token to be handed and cannot fail, so it asks for
        // the proof. On the main thread the change runs here, to the end: the
        // node is out of any executor (`Node::prepare`'s contract), so its
        // realtime end hands the Processor back and takes the new one in this
        // call.
        if let Some(main) = MainThreadMarker::new() {
            let outcome = self
                .inner
                .lock()
                .set_sample_rate(main, rate, Some(&mut self.rt));
            // A refusal renders silence (no Processor is lent) and is reported
            // through the handle (`PluginInvalidation::RateRefused`).
            self.rate_refused = outcome.is_err();
            if let Err(reason) = outcome {
                tracing::error!("a plugin refused its rate: {reason}");
            }
            return;
        }
        match self.rate_delivery {
            // Off the main thread the bracket cannot run: pending for the main
            // thread's pump.
            RateDelivery::Pending => queue_sample_rate(&self.pending_sample_rate, rate),
            // A fork has no pump: the refusal is recorded (`rate_refused`), the
            // node renders silence rather than audio at the wrong rate, and the
            // next `prepare` on the main thread clears it.
            RateDelivery::InPrepare => {
                self.rate_refused = true;
                tracing::error!(
                    "a forked plugin was prepared off the main thread; \
                     it renders silence until it is prepared on it"
                );
            }
        }
    }
}

/// The per-block context handed to the plugin, carrying this block's MIDI,
/// parameter chunk and transport snapshot.
///
/// `transport` is always attached. The decision of *what* to attach happens one
/// level up, in [`TransportFeed::block`], which yields a default snapshot for a
/// plugin that did not declare [`Features::TRANSPORT`] — so there is no second
/// format check here. The parameter chunk is always attached too: it carries
/// the changes only, and a Processor that takes parameters through a coarse
/// write (VST2) reads it as well as one that takes curves.
///
/// Named rather than inlined so the shape of the context the node builds is
/// observable without a plugin binary on disk: whether `ctx.transport` is
/// populated at all is precisely what decides if the plugin has a transport
/// to read.
pub(super) fn block_context<'a>(
    midi_events: &'a [UmpAt],
    params: &'a ParameterChanges,
    transport: &'a TransportInfo,
) -> ProcessContext<'a> {
    ProcessContext::new()
        .midi(midi_events)
        .params(params)
        .transport(transport)
}

/// What a block did to the plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rendered {
    /// The plugin ran the block; its MIDI-out is in the client's `out`.
    Ran,
    /// No Processor in hand: the block was not rendered.
    Unlent,
    /// The Processor failed the block, for this reason.
    Failed(&'static str),
}

/// What a failed block's log says: the reason when the Processor gave a
/// constant one ([`PluginError::Block`]), else that it failed. A reason that is
/// text (`PluginError::Process`) is not logged: the log is a constant.
fn cause_of(error: &PluginError) -> &'static str {
    match error {
        PluginError::Block(why) => why,
        _ => "the plugin failed a block",
    }
}

/// What a block, or the piece of one a chunked Processor is handed, carries
/// beside its MIDI and parameter changes.
pub(super) struct Inputs<'a> {
    /// The transport at the first frame.
    pub(super) transport: &'a TransportInfo,
    /// The transport changes inside, each on a frame from the first.
    pub(super) changes: &'a [TransportChange],
    /// Sequencer context, for a plugin that takes it.
    pub(super) expressive: Option<ExpressiveContext<'a>>,
}

/// Render one block through the plugin, from `inputs` into `outputs`: the
/// graph's own channel buffers, each `size` frames, through the Processor the
/// node took from its realtime end (`None`: it holds none). A free function so
/// it can take disjoint borrows of the fields on the caller side without a
/// self-borrow conflict. A chunked Processor's node calls it once per piece of
/// a block.
///
/// On anything but [`Rendered::Ran`] `outputs` is untouched: the caller writes
/// the silence.
pub(super) fn render_with<P: Processor, S: Float>(
    processor: Option<&mut P>,
    losses: &LossCounts,
    params: &ParamDelivery,
    midi: &mut MidiEventVec,
    out: &mut ProcessOutput,
    with: Inputs<'_>,
    inputs: &[&[S]],
    outputs: &mut [&mut [S]],
    size: usize,
) -> Rendered {
    // Taken whether or not the plugin runs: a block with no Processor drops
    // its MIDI rather than play it a block late. Sorted, stably: a queue may
    // hold it out of order.
    let mut midi_events = std::mem::take(midi);
    tutti_plugin_types::insertion_sort_by_key(&mut midi_events, |e| e.offset);
    // The block's MIDI as the graph's `(Offset, Ump)`: every offset came from
    // an `Offset` of this block (the node's intake), so the clamp only keeps
    // a stray one inside the block.
    let block = Samples::new(size);
    let timed: ArrayVec<UmpAt, MIDI_STACK_CAPACITY> = midi_events
        .iter()
        .map(|e| (Offset::clamped(e.offset, block), e.ump))
        .collect();
    let Some(processor) = processor else {
        losses.unlent.add(1);
        return Rendered::Unlent;
    };
    out.clear();
    let mut ctx =
        block_context(&timed, params.chunk(), with.transport).transport_changes(with.changes);
    if let Some(expressive) = with.expressive {
        ctx = ctx.expressive(expressive);
    }
    let processed = process_block(processor, inputs, outputs, block, &ctx, out);
    match processed {
        Ok(()) => {
            // A plugin's NaN or infinity would poison every node downstream of it.
            let mut wrote = false;
            for channel in outputs.iter_mut() {
                wrote |= sanitize(channel);
            }
            losses.nonfinite.add(u64::from(wrote));
            Rendered::Ran
        }
        Err(error) => {
            losses.failed.add(1);
            Rendered::Failed(cause_of(&error))
        }
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use tutti_graph::{Env, TransportChanges, TransportHistory, TransportPosition};
    use tutti_music::TimeSignature;
    use tutti_music::{BeatsPerBar, MeterChange, MeterMap, NoteValue};
    use tutti_tempo::TempoMap;
    use tutti_timeline::ProjectMeter;
    use tutti_units::Latency;
    use tutti_units::{Beat, Bpm, Frame, SampleRate};

    /// A rolling block of 64 frames at 132 BPM from beat 4, at 48 kHz: 132,
    /// not the 120 the default snapshot carries, so a live read is told
    /// apart from the "nothing" answer.
    fn env() -> Env {
        Env {
            frame: Frame::new(0),
            sample_rate: SampleRate::SR_48K,
            block_len: Samples::new(64),
            transport: TransportPosition::new(true, Bpm::of(132.0), Beat::new(4.0), None),
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        }
    }

    /// The project's tempo map for these blocks: the constant 132 BPM
    /// `env()`'s transport carries, so a block reports the tempo the test's
    /// transport has.
    fn map() -> TempoMap {
        TempoMap::constant(Bpm::of(132.0)).expect("a tempo")
    }

    fn feed() -> TransportFeed {
        TransportFeed::new()
    }

    fn cx(e: &Env) -> Cx<'_> {
        Cx {
            env: e,
            arrival: Latency::ZERO,
            history: TransportHistory::none(),
            meter: None,
        }
    }

    fn seven_eight() -> TimeSignature {
        TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH)
    }

    fn seven_eight_meter() -> ProjectMeter {
        ProjectMeter::new(MeterMap::new([MeterChange::new(
            Beat::new(0.0),
            seven_eight(),
        )]))
    }

    /// The feature set the in-process loader builds for a plugin reporting no
    /// optional capability: `TRANSPORT` is unconditional there.
    fn loader_features() -> Features {
        Features::TRANSPORT
    }

    /// The context the node hands the plugin carries the block's transport
    /// from its `Cx`: the only thing that gives `audioMasterGetTime`
    /// something to read. With no meter installed it is 4/4.
    #[test]
    fn the_block_context_carries_the_transport_from_cx() {
        let e = env();
        let mut feed = feed();
        let snapshot = feed.block(
            &cx(&e),
            64,
            loader_features(),
            &mut map().view(e.sample_rate),
        );
        let params = ParameterChanges::default();
        let ctx = block_context(&[], &params, &snapshot);
        let delivered = ctx.transport.expect("a transport snapshot");
        assert_eq!(delivered.timing.tempo.get(), 132.0);
        assert!(delivered.state.playing);
        assert_eq!(delivered.position.beat.get(), 4.0);
        assert_eq!(delivered.sample_rate.get(), 48_000.0);
        assert_eq!(
            delivered.timing.signature,
            TimeSignature::new(BeatsPerBar::of(4), NoteValue::QUARTER)
        );
    }

    /// The declared bit gates delivery: a plugin without `TRANSPORT` is
    /// handed the default (stopped, 120 BPM) snapshot, whatever the `Cx`.
    #[test]
    fn a_plugin_not_declaring_transport_is_handed_the_default() {
        let e = env();
        let mut feed = feed();
        let got = feed.block(
            &cx(&e),
            64,
            Features::empty(),
            &mut map().view(e.sample_rate),
        );
        let default = TransportInfo::new(e.sample_rate);
        assert_eq!(got.timing.tempo.get(), default.timing.tempo.get());
        assert_eq!(got.state.playing, default.state.playing);
        assert_eq!(got.position.beat, default.position.beat);
    }

    /// A meter installed through one handle on the feed's cell reaches the
    /// feed the node runs (a host installs through the node's controls), and
    /// the snapshot carries its signature.
    #[test]
    fn a_meter_installed_on_one_clone_reaches_another() {
        let e = env();
        let held = feed();
        let mut running = held.clone();
        held.meter.set_meter(seven_eight_meter());
        let got = running.block(
            &cx(&e),
            64,
            loader_features(),
            &mut map().view(e.sample_rate),
        );
        assert_eq!(got.timing.signature, seven_eight());
    }

    /// A fork's feed holds the meter installed when it forked, and nothing
    /// installed on the live node afterwards; its counter starts at zero.
    #[test]
    fn a_fork_has_the_meter_installed_when_it_forked() {
        let e = env();
        let mut live = feed();
        live.meter.set_meter(seven_eight_meter());
        let _ = live.block(
            &cx(&e),
            64,
            loader_features(),
            &mut map().view(e.sample_rate),
        );
        let mut fork = live.forked(false);
        live.meter.clear_meter();
        let got = fork.block(
            &cx(&e),
            64,
            loader_features(),
            &mut map().view(e.sample_rate),
        );
        assert_eq!(got.timing.signature, seven_eight());
        assert_eq!(got.position.continuous_samples, 0);
    }

    /// The free-running counter is the frames the node rendered, block after
    /// block.
    #[test]
    fn the_counter_counts_the_frames_rendered() {
        let e = env();
        let mut feed = feed();
        let _ = feed.block(
            &cx(&e),
            64,
            loader_features(),
            &mut map().view(e.sample_rate),
        );
        let second = feed.block(
            &cx(&e),
            64,
            loader_features(),
            &mut map().view(e.sample_rate),
        );
        assert_eq!(second.position.continuous_samples, 64);
    }

    /// An empty block has no frame to read the transport at: the plugin is
    /// handed the last snapshot again, not the stopped default.
    #[test]
    fn an_empty_block_repeats_the_last_snapshot() {
        let e = env();
        let mut feed = feed();
        let _ = feed.block(
            &cx(&e),
            64,
            loader_features(),
            &mut map().view(e.sample_rate),
        );
        let empty = feed.block(
            &cx(&e),
            0,
            loader_features(),
            &mut map().view(e.sample_rate),
        );
        assert_eq!(empty.timing.tempo.get(), 132.0);
        assert!(empty.state.playing);
        assert_eq!(empty.position.continuous_samples, 0);
    }

    /// **A live feed tells the plugin the host clock; an offline fork's
    /// does not.** No clock relates to a render that runs at its own speed,
    /// so `nanoSeconds` must not be claimed there. A live fork keeps it.
    #[test]
    fn only_a_live_feed_carries_the_host_clock() {
        let e = env();
        let mut live = feed();
        assert!(
            live.block(
                &cx(&e),
                64,
                loader_features(),
                &mut map().view(e.sample_rate),
            )
            .system_time_ns
            .is_some()
        );
        let mut live_fork = live.forked(false);
        assert!(
            live_fork
                .block(
                    &cx(&e),
                    64,
                    loader_features(),
                    &mut map().view(e.sample_rate),
                )
                .system_time_ns
                .is_some()
        );
        let mut offline = live.forked(true);
        assert_eq!(
            offline
                .block(
                    &cx(&e),
                    64,
                    loader_features(),
                    &mut map().view(e.sample_rate),
                )
                .system_time_ns,
            None
        );
    }

    /// The tempo a block reports is the project map's at the playhead, not
    /// the transport's own (`Env`'s 132).
    #[test]
    fn a_block_reports_the_maps_tempo() {
        let e = env();
        let mut live = feed();
        let map = TempoMap::constant(Bpm::of(100.0)).expect("a tempo");
        let got = live.block(&cx(&e), 64, loader_features(), &mut map.view(e.sample_rate));
        assert_eq!(got.timing.tempo.get(), 100.0);
    }
}

/// `render_with`, over a Processor of the test's own: what a block does to the
/// counters and to the plugin's output, for any format.
#[cfg(test)]
mod render_tests {
    use super::*;
    use tutti_buffers::Float;
    use tutti_plugin_types::{PluginError, PluginIo, PluginResult, WireMidi};
    use tutti_ump::Ump;

    /// A Processor that fails every block it is told to, and emits one MIDI
    /// event in each block it is told to.
    struct Scripted {
        fail: bool,
        emit: bool,
    }

    impl Processor for Scripted {
        fn event_input(&self) -> std::num::NonZeroU32 {
            tutti_plugin_types::MIDI_STACK_BOUND
        }

        fn process<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
            if self.fail {
                return Err(PluginError::Block("scripted failure"));
            }
            if self.emit {
                io.out.emit_midi([WireMidi {
                    offset: 3,
                    ump: Ump([0x2090_3C40, 0, 0, 0]),
                }]);
                self.emit = false;
            }
            Ok(())
        }

        fn stop(&mut self) {}

        fn reset(&mut self) {}
    }

    /// One block of 8 frames through `render_with`, the rest of its arguments at
    /// their quiet values.
    fn block(rt: &mut Rt<Scripted>, losses: &LossCounts, out: &mut ProcessOutput) -> Rendered {
        let params = ParamDelivery::closed([], Arc::default());
        let mut midi = MidiEventVec::new();
        let mut channel = [0.0f32; 8];
        render_with(
            rt.poll(Processor::stop),
            losses,
            &params,
            &mut midi,
            out,
            Inputs {
                transport: &TransportInfo::new(SampleRate::SR_48K),
                changes: &[],
                expressive: None,
            },
            &[],
            &mut [&mut channel[..]],
            8,
        )
    }

    /// **A block the Processor fails is counted as failed, and a block with no
    /// Processor as unlent, each apart from the other**; the caller renders
    /// silence for both. A failure does not take the Processor away: the next
    /// block runs.
    #[test]
    fn a_failed_block_is_counted_apart_from_an_unlent_one() {
        let losses = LossCounts::default();
        let mut out = ProcessOutput::default();
        let (mut hand, mut rt) = tutti_rt::handoff();
        assert_eq!(block(&mut rt, &losses, &mut out), Rendered::Unlent);
        assert_eq!(losses.unlent.get(), 1, "unlent");
        assert_eq!(losses.failed.get(), 0, "failed");

        assert!(
            hand.lend(Scripted {
                fail: true,
                emit: false
            })
            .is_ok()
        );
        assert_eq!(
            block(&mut rt, &losses, &mut out),
            Rendered::Failed("scripted failure"),
            "the Processor's constant reason is the logged cause"
        );
        assert_eq!(losses.unlent.get(), 1, "unlent");
        assert_eq!(losses.failed.get(), 1, "failed");

        rt.held().expect("still in hand").fail = false;
        assert_eq!(block(&mut rt, &losses, &mut out), Rendered::Ran);
        assert_eq!(losses.failed.get(), 1, "failed, once");
    }

    /// **The plugin's MIDI out is what it emitted in this block**: the block
    /// after an emitting one reads none of it, because `out` is cleared at the
    /// head of every block.
    #[test]
    fn a_blocks_midi_out_is_what_the_plugin_emitted_in_it() {
        let losses = LossCounts::default();
        let mut out = ProcessOutput::default();
        let (mut hand, mut rt) = tutti_rt::handoff();
        assert!(
            hand.lend(Scripted {
                fail: false,
                emit: true
            })
            .is_ok()
        );
        assert_eq!(block(&mut rt, &losses, &mut out), Rendered::Ran);
        assert_eq!(out.midi_events.len(), 1);
        assert_eq!(out.midi_events[0].offset, 3);
        assert_eq!(block(&mut rt, &losses, &mut out), Rendered::Ran);
        assert!(
            out.midi_events.is_empty(),
            "last block's MIDI is not this one's"
        );
    }
}

/// A plugin that emits a non-finite sample does not reach the graph with it.
#[cfg(test)]
mod sanitize_tests {
    use super::*;
    use tutti_buffers::{ByWidth, Float};
    use tutti_plugin_types::PluginIo;
    use tutti_plugin_types::PluginResult;

    /// Writes NaN, +inf and -inf among finite samples to its first output channel.
    struct Poisoned;

    impl Processor for Poisoned {
        fn event_input(&self) -> std::num::NonZeroU32 {
            tutti_plugin_types::MIDI_STACK_BOUND
        }

        fn process<S: Float>(&mut self, io: &mut PluginIo<'_, S>) -> PluginResult<()> {
            let ByWidth::F32(buffer) = io.buffer.by_width() else {
                unreachable!("the graph is f32");
            };
            buffer
                .output()
                .plane_mut(0)
                .samples_mut()
                .copy_from_slice(&[0.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY]);
            Ok(())
        }

        fn stop(&mut self) {}

        fn reset(&mut self) {}
    }

    /// **The in-process path zeroes NaN and both infinities of a block the
    /// plugin rendered and keeps its finite samples**, as the plugin server does
    /// before a block leaves its process, and counts the block once, however
    /// many samples were bad.
    #[test]
    fn an_in_process_plugins_non_finite_output_is_zeroed_finite_output_kept_and_the_block_counted()
    {
        let losses = LossCounts::default();
        let params = ParamDelivery::closed([], Arc::default());
        let mut midi = MidiEventVec::new();
        let mut out = ProcessOutput::default();
        let mut channel = [1.0f32; 4];
        let (mut hand, mut rt) = tutti_rt::handoff();
        assert!(hand.lend(Poisoned).is_ok());
        let rendered = render_with(
            rt.poll(Processor::stop),
            &losses,
            &params,
            &mut midi,
            &mut out,
            Inputs {
                transport: &TransportInfo::new(SampleRate::SR_48K),
                changes: &[],
                expressive: None,
            },
            &[],
            &mut [&mut channel[..]],
            4,
        );
        assert_eq!(rendered, Rendered::Ran);
        assert_eq!(channel, [0.5, 0.0, 0.0, 0.0]);
        assert_eq!(losses.nonfinite.get(), 1, "one block, three bad samples");
    }
}

#[cfg(test)]
mod declared_buses {
    use super::*;
    use tutti_graph::BusRole;
    use tutti_plugin_types::MAX_BLOCK_CHANNELS;
    use tutti_plugin_types::{ChannelTopology, Speaker};

    fn width(n: u16) -> ChannelLayout {
        ChannelLayout::from(n)
    }

    fn stereo() -> ChannelTopology {
        ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight]).unwrap()
    }

    /// A plugin's buses are the node's, in order, each with the plugin's role
    /// and placement: a stereo main, a mono sidechain and an aux. A bus
    /// without a placement (or with one that disagrees with its width) is
    /// `Unassigned`, and the shape's ports are every channel.
    #[test]
    fn a_plugins_buses_are_the_nodes_in_order_placed_and_with_their_roles() {
        let loaded = PluginWiring {
            inputs: vec![
                PluginBus::new(width(2), BusRole::Main).placed(stereo()),
                PluginBus::new(width(1), BusRole::Sidechain),
                // Declares two speakers for a three-channel bus.
                PluginBus::new(width(3), BusRole::Aux).placed(stereo()),
            ],
            outputs: vec![PluginBus::main(width(2)).placed(stereo())],
            ..PluginWiring::default()
        };
        let shape = Declared::of(&loaded).unwrap().ports;
        assert_eq!(shape.audio_in(), width(6));
        let ins: Vec<_> = shape
            .buses_in()
            .iter()
            .map(|b| (b.role, b.topology.positions().unwrap().to_vec()))
            .collect();
        assert_eq!(
            ins,
            [
                (BusRole::Main, vec![Speaker::FrontLeft, Speaker::FrontRight]),
                (BusRole::Sidechain, vec![Speaker::Unassigned]),
                (BusRole::Aux, vec![Speaker::Unassigned; 3]),
            ]
        );
        assert_eq!(shape.buses_in().first_port(2), Some(3));
        assert_eq!(
            shape
                .buses_out()
                .get(0)
                .unwrap()
                .topology
                .positions()
                .unwrap(),
            &[Speaker::FrontLeft, Speaker::FrontRight]
        );
    }

    /// A plugin with no bus list has no buses; one with more than the node
    /// holds keeps the first ones and every channel as a port.
    #[test]
    fn what_does_not_fit_stays_a_port() {
        let none = Declared::of(&PluginWiring::default()).unwrap().ports;
        assert!(none.buses_in().is_empty() && none.buses_out().is_empty());

        let many = PluginWiring {
            inputs: vec![PluginBus::main(width(1)); 5],
            outputs: vec![PluginBus::main(width(2)); 5],
            ..PluginWiring::default()
        };
        let shape = Declared::of(&many).unwrap().ports;
        assert_eq!(shape.audio_in(), width(5));
        assert_eq!(shape.buses_in().len(), tutti_graph::MAX_BUSES);
        assert_eq!(shape.audio_out(), width(10));
        assert_eq!(shape.buses_out().len(), tutti_graph::MAX_BUSES);
    }

    /// **A plugin wider than a block serves is refused by name, never narrowed.**
    /// A direction past `MAX_BLOCK_CHANNELS` over all its buses has no node;
    /// the widest out-of-process plugin has one.
    #[test]
    fn a_plugin_wider_than_a_block_is_refused_by_name() {
        let max = MAX_BLOCK_CHANNELS;
        let wiring = |inputs: &[usize], outputs: &[usize]| PluginWiring {
            inputs: inputs
                .iter()
                .map(|&n| PluginBus::main(width(n as u16)))
                .collect(),
            outputs: outputs
                .iter()
                .map(|&n| PluginBus::main(width(n as u16)))
                .collect(),
            ..PluginWiring::default()
        };
        assert!(Declared::of(&wiring(&[max], &[max])).is_ok());
        assert_eq!(
            Declared::of(&wiring(&[8, 9], &[2])).unwrap_err(),
            PluginTooWide {
                direction: "input",
                channels: max + 1
            }
        );
        assert_eq!(
            Declared::of(&wiring(&[2], &[max, 1])).unwrap_err(),
            PluginTooWide {
                direction: "output",
                channels: max + 1
            }
        );
    }

    /// A plugin with a bypass of its own gives its node a shape that declares
    /// it, so the executor leaves the node running in `Bypassed`; a plugin
    /// without one keeps the executor's bypass.
    #[test]
    fn only_a_plugin_with_its_own_bypass_declares_one() {
        let with = PluginWiring {
            own_bypass: Some(tutti_plugin_types::OwnBypass::Property),
            ..PluginWiring::default()
        };
        assert!(
            Declared::of(&with)
                .expect("a plugin of a few channels")
                .ports
                .own_bypass
        );
        assert!(
            !Declared::of(&PluginWiring::default())
                .expect("an empty plugin")
                .ports
                .own_bypass
        );
    }
}
