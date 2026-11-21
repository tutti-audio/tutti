//! [`Session`]: the graph, its transport and engine side, and the order they
//! run in ([`PHASES`]). This file holds the struct, its constructors, and the
//! removal, modulation and reclaim calls; its submodules hold the rest of its
//! methods by job.

mod event;
pub(crate) mod parts;
mod phases;
mod root;
mod taps;
#[cfg(test)]
mod tests;
mod transport;

pub use event::SessionEvent;
#[cfg(feature = "midi")]
pub use parts::MidiNodes;
pub use phases::{ModInput, PHASES, Phase, PhaseCx};
pub use root::RootTopologyStatus;

use std::fmt::Debug;
use std::sync::Arc;

use tutti_engine::{OutputSpec, StreamFaults};
use tutti_graph::NodeKey;
use tutti_units::{Amplitude, SampleRate};

use crate::background::Background;
use crate::clock::sealed::Streaming;
use crate::graph::Graph;
use crate::value::{SlotCell, ValueState, ValueTable};
use crate::{Clock, Offline, OutputPolicy, RealTime, Removal, RemoveError, SessionFloat};
use parts::{
    BouncePart, DevicePart, EarlyWork, Inputs, MidiPart, OutputPart, PluginPart, Reports,
    SamplerPart, TransportPart,
};

/// The bound on an app's key type, the name an app gives a node in its model.
/// Every `Ord + Clone + Debug + Send + Sync + 'static` type is one.
pub trait AppKey: Ord + Clone + Debug + Send + Sync + 'static {}

impl<T: Ord + Clone + Debug + Send + Sync + 'static> AppKey for T {}

/// The tutti engine: a [`Graph`], its transport, and the engine
/// side it renders on, in the order [`update`](Self::update) runs them.
///
/// Built with [`SessionBuilder`](crate::SessionBuilder): offline (the host
/// pulls blocks with [`render`](Self::render)), on a device (with a
/// `Device`, feature `device`), or detached (the executor stays on
/// this side: `render_frame`).
///
/// `Send`, not `Sync`: one control thread owns it, whichever thread that is.
/// Nothing the session frees needs the main thread: a plugin a removal, a
/// replacement or a crossfade retires is parked in a graveyard, which the main
/// thread empties (`Session::update`, called there), and a format closes an
/// in-process plugin that is released off the main thread there (it hands the
/// close to the session's `MainWork`, which the main thread's update runs). A
/// host with no main-thread update keeps the parked plugins until it has one, and
/// the session says so once
/// (`Session::retired_plugins`). What does need the main thread (an editor,
/// loading an in-process plugin, restoring state) takes the proof
/// (`MainThreadMarker`): `Session::editor`, `Plugin::presets`.
///
/// Dropping a session commits every take before it returns: it joins each
/// take's pump. On a thread that drives an executor (inside a future on a
/// `tutti_platform::Executor`) a join would stop the tasks that thread runs, so the drop
/// detaches the pumps instead and logs how many takes it left unfinished: their files
/// are not committed by that drop.
///
/// ```
/// use tutti_buffers::MaxBlock;
/// use tutti_units::{SampleRate, Samples};
/// use tutti_buffers::ChannelLayout;
/// use tutti_buffers::InterleavedMut;
/// use tutti_graph::ForkByClone;
/// use tutti_session::{OutPort, Root, Session};
/// use tutti_nodes::testing::Const;
///
/// // One global output: the mono device takes it as it is, unfolded.
/// let mut session = Session::builder()
///     .outputs_floor(ChannelLayout::MONO)
///     .outputs(ChannelLayout::MONO).max_block(MaxBlock::of(Samples::new(64))).build()
///     .unwrap();
/// let dc = session.graph().add(ForkByClone(Const::mono(0.5)));
/// session.graph().set_sources(Root::output(0), [OutPort::new(dc, 0)]).unwrap().unfaded();
/// session.update();
///
/// let mut block = [0.0f32; 64];
/// session.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO).unwrap());
/// assert!(block.iter().all(|&s| s == 0.5));
/// ```
#[must_use = "dropping a session ends it, and on a real-time clock stops its stream"]
#[derive(Debug)]
pub struct Session<K: AppKey = &'static str, C: Clock = Offline> {
    /// Which session this is, so an output cut from another is refused.
    pub(crate) id: crate::apply::SessionId,
    /// The value lock and the table every node's params are written through,
    /// shared with the graph and the session's [`Values`](crate::Values).
    pub(crate) values: Arc<ValueState<K>>,
    /// Runs the work other threads handed to the main thread when the session
    /// drops on it, before anything else drops (the plugins then close, and
    /// the last field collects what they hand over).
    pub(crate) _early_work: EarlyWork,
    /// What runs the blocks (the clock's [`Running`](Clock::Running)). First,
    /// so a stream stops before the session's own state drops.
    pub(crate) running: C::Running,
    pub(crate) graph: Graph<C::Float>,
    /// The source of the ids of the session's resources; a clock switch carries it over.
    #[cfg_attr(
        not(any(feature = "plugin", feature = "midi", feature = "device")),
        expect(
            dead_code,
            reason = "the ids of plugins and MIDI ports (features `plugin`, `midi`); \
                      a clock switch (`device`) carries them over"
        )
    )]
    pub(crate) resource_ids: crate::peer::ResourceIds,
    /// The transport and the figures of the plan sent last.
    pub(crate) transport: TransportPart,
    /// Where the mix lands: the root, the policy, the engine's output readings.
    pub(crate) output: OutputPart<C::Float>,
    /// What the last update read, for the next one's reports.
    pub(crate) reports: Reports,
    /// The session's MIDI declarations and the OS layer under them.
    pub(crate) midi: MidiPart,
    /// What modulation applied, and the session's own declarations.
    pub(crate) mods: crate::modulation::ModSession,
    /// The disk streamer and the streams the opens landed. Declared before `background`, so it
    /// drops first: its drop rings the streamer's bell, and the ring only reaches the
    /// streamer's task while the bells `background` owns still have their final pass to run.
    pub(crate) sampler: SamplerPart<C::Float>,
    /// The output-device listing.
    pub(crate) devices: DevicePart,
    /// The capture inputs added ([`Session::add_input`]) and the host they open on.
    pub(crate) inputs: Inputs<C::Float>,
    /// The bounce token and the leftovers' channel.
    pub(crate) bounce: BouncePart,
    /// The executor, its thread, the bells the session's futures await and the takes'
    /// pumps. Declared after `sampler` and every other part that rings a bell (fields
    /// drop in declaration order): the bells drop last.
    pub(crate) background: Background,
    /// Plugin hosting. Last, so it drops after every plugin the session holds:
    /// a close handed over while the session drops is still collected.
    pub(crate) plugins: PluginPart,
}

impl Session<&'static str, Offline> {
    /// A builder with the defaults: offline, 48 kHz, stereo, blocks of up to
    /// [`LIVE_MAX_BLOCK`](crate::LIVE_MAX_BLOCK) frames
    /// ([`SessionBuilder`](crate::SessionBuilder) has the options, and the
    /// other clocks).
    pub fn builder() -> crate::SessionBuilder<crate::Offline> {
        crate::SessionBuilder::default()
    }

    /// An offline session at `rate` with every other option at its default:
    /// shorthand for `Session::builder().sample_rate(rate).build()`.
    ///
    /// # Errors
    ///
    /// As [`SessionBuilder::build`](crate::SessionBuilder::build).
    pub fn new(rate: SampleRate) -> crate::Result<Self> {
        Self::builder().sample_rate(rate).build()
    }
}

impl<K: AppKey, F: SessionFloat> Session<K, Offline<F>> {
    /// Renders one block into `output`, as a device callback would: what an
    /// offline session's host pulls. Every update's commit lands at the
    /// start of the next block.
    pub fn render(&self, output: &mut tutti_buffers::InterleavedMut<'_, F>) {
        self.running.engine.process(output);
    }
}

impl<K: AppKey, C: RealTime> Session<K, C> {
    /// The configuration of the stream that renders the session's blocks: its
    /// rate, width and block (the callback size, or the timer's block).
    #[must_use]
    pub fn stream_spec(&self) -> &OutputSpec {
        Streaming::output_stream(&self.running).spec()
    }

    /// The stream's fault record: one entry each time the clock's stream
    /// failed, a device unplugged, a callback that panicked, a timer that fell
    /// more than four blocks behind and began its schedule again. The same
    /// handle for the session's life; [`update`](Self::update) reports each new
    /// fault as [`SessionEvent::Fault`] and takes its message from the record
    /// ([`StreamFaults::take_last`](tutti_engine::StreamFaults::take_last)), so
    /// [`count`](tutti_engine::StreamFaults::count) is the lasting reading.
    #[must_use]
    pub fn faults(&self) -> Arc<StreamFaults> {
        Streaming::output_stream(&self.running).faults()
    }
}

/// A detached session: the executor stays on this side, and the caller renders.
#[cfg(feature = "testing")]
impl<K: AppKey, F: SessionFloat> Session<K, crate::Detached<F>> {
    /// Renders one frame on the calling thread under a stopped transport:
    /// no inputs, `output` one sample per global output. Commits any edit
    /// first.
    ///
    /// # Panics
    ///
    /// On a session whose audio side was taken (`Graph::take_audio_side`).
    pub fn render_frame(&mut self, output: tutti_buffers::InterleavedMut<'_, F>) {
        self.render_frame_at(&tutti_graph::TransportPosition::default(), output);
    }

    /// `render_frame` with the transport at
    /// `transport`: what plays a clip in a graph driven by hand.
    pub fn render_frame_at(
        &mut self,
        transport: &tutti_graph::TransportPosition,
        output: tutti_buffers::InterleavedMut<'_, F>,
    ) {
        if self.graph.has_uncommitted_edits() {
            self.commit();
        }
        self.graph.render_local(transport, output);
    }
}

/// `f32` because a plugin format is `f32` at its FFI.
#[cfg(feature = "plugin")]
impl<K: AppKey, C: Clock<Float = f32>> Session<K, C> {
    /// Serves the plugins: the part of a phase only an `f32` session has.
    pub(crate) fn serve_plugins(&mut self, cx: PhaseCx<'_>, out: &mut Vec<SessionEvent>) {
        self.plugins_phase(cx, out);
        self.rebind_plugins(cx.main, out);
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// The same session on the clock `running` belongs to: every field moves,
    /// the clock's own state is swapped, and the old one is handed back.
    ///
    /// The background work carries over: a take's pump names nothing of the clock, and
    /// reports to the converted session.
    #[cfg(feature = "device")]
    pub(crate) fn with_clock<N: Clock<Float = C::Float>>(
        self,
        running: N::Running,
    ) -> (Session<K, N>, C::Running) {
        let Session {
            id,
            values,
            _early_work,
            running: old_running,
            graph,
            resource_ids,
            transport,
            output,
            reports,
            midi,
            mods,
            sampler,
            devices,
            inputs,
            bounce,
            background,
            plugins,
        } = self;
        (
            Session {
                id,
                values,
                _early_work,
                running,
                graph,
                resource_ids,
                transport,
                output,
                reports,
                midi,
                mods,
                sampler,
                devices,
                inputs,
                bounce,
                background,
                plugins,
            },
            old_running,
        )
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Registers the session's own values (its tempo, meter and click) in
    /// `table`, once, as the session is built. Each is a cell the session
    /// and the audio thread already share; the table only names it for the
    /// verbs.
    pub(crate) fn provide_values(
        &self,
        table: &ValueTable,
        click: Arc<tutti_nodes::ClickSettings>,
    ) {
        use crate::value::{
            ClickVolumeSlot, LoopCell, PublishedSlot, SettingSlot, SlotId, TempoCell,
        };
        table.provide_staged(
            SlotId::Tempo,
            PublishedSlot::new(TempoCell {
                settings: self.transport.settings().clone(),
                following: self.midi.following(),
            }),
        );
        table.provide_staged(
            SlotId::Meter,
            PublishedSlot::new(self.transport.settings().project_meter().clone()),
        );
        table.provide_staged(
            SlotId::Loop,
            PublishedSlot::new(LoopCell::new(self.transport.transport().clone())),
        );
        table.provide(
            SlotId::ClickVolume,
            Arc::new(ClickVolumeSlot(Arc::clone(&click))) as SlotCell<Amplitude>,
        );
        table.provide(
            SlotId::OutputPolicy,
            Arc::clone(&self.output.policy) as SlotCell<OutputPolicy>,
        );
        self.midi.provide_values(table);
        table.provide(
            SlotId::ClickMode,
            Arc::new(SettingSlot {
                owner: click,
                project: tutti_nodes::ClickSettings::mode,
            }) as SlotCell<tutti_nodes::MetronomeMode>,
        );
    }
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Ends the session on the main thread: waits for the plugin round trips in
    /// flight (a snapshot, a parameter read, a reload), each bounded by its own IPC
    /// deadline and past it warned about, while running the work they hand to the main
    /// thread; runs the work other threads handed to it, drops the session's plugins (each closes on `main`, or hands its close
    /// to the session's collector), and runs what that handed over. Nothing a
    /// plugin owes the main thread is left undone.
    ///
    /// End a session that hosted an in-process plugin with `close` on the main
    /// thread; dropped elsewhere, it leaks those plugins and counts them in
    /// [`stranded()`](tutti_plugin_types::MainWork::stranded). Plugins dropped off their
    /// own thread are counted in
    /// [`deferred_drops()`](tutti_plugin_types::MainWork::deferred_drops). Both counts are
    /// process-wide, as are [`contained_panics()`](tutti_plugin_types::contained_panics),
    /// the panics plugin callbacks caused and the host contained, and
    /// [`refused_off_thread_calls()`](tutti_plugin_types::refused_off_thread_calls),
    /// the main-thread callbacks plugins made from another thread.
    #[cfg(feature = "plugin")]
    pub fn close(self, main: tutti_plugin_types::MainThreadMarker) {
        let mut session = self;
        session.plugins.drain_tasks(Some(main));
        let collected = session.plugins.take_collector(main);
        drop(session);
        collected.run(main);
    }

    /// The graph, to edit. An edit marks it edited; the next
    /// [`update`](Self::update) commits every edit since the last at once.
    pub fn graph(&mut self) -> &mut Graph<C::Float> {
        &mut self.graph
    }

    /// The graph, to read.
    pub fn graph_ref(&self) -> &Graph<C::Float> {
        &self.graph
    }

    /// The [`Reclaim`](Phase::Reclaim) phase: every cell the session
    /// publishes to, scanned and what no block holds freed. Returns how many
    /// retired values are still held (by a block in progress).
    ///
    /// The nodes are the graph's table: each node's reclaimer, kept by node
    /// as it went in ([`SessionNode::reclaimer`](crate::SessionNode::reclaimer)),
    /// and each hosted plugin's meter. Beside it, the cells the session
    /// publishes to itself: the project meter (and
    /// every slot following it), the transport's tempo map, the modulation lanes (inserted by the
    /// reconciler, not through `add`), the hardware inlets' clock and an
    /// offline engine's output folds.
    pub(crate) fn reclaim(&self) -> usize {
        let mut pinned = self.graph.reclaim();
        pinned += self.transport.settings().project_meter().reclaim();
        pinned += self.transport.settings().reclaim_tempo();
        // The engine's output folds are swept by the graph (`Graph::reclaim`),
        // offline or on a device, through the `OutputControl` it kept.
        pinned += self.mods.state.reclaim();
        pinned += self.midi.reclaim();
        pinned
    }

    /// Takes `node` out of the graph, whatever it is (a plugin, an inlet, a
    /// clip): every edge to and from it reads silence afterwards, so a sink
    /// still naming it is left silent rather than dangling, and the feeds into
    /// and from it go. The edges fade out first (a declicked removal): the node
    /// stays in the editor, rendering, until they have. [`Removal::unfaded`] cuts
    /// it at the next block boundary instead. A replacement waiting on it is
    /// dropped and reported
    /// ([`SessionEvent::ReplaceDropped`]).
    ///
    /// Any thread that owns the session: the removal frees nothing itself. The unit is freed
    /// when the executor hands it back, at a collect, which is where the
    /// main-thread guard is (see the type docs).
    ///
    /// The session's own declarations on `node` go with it: its modulation
    /// ranges, automation and the routes into it (`mods`, feature
    /// `modulation`), and, when it is a clip or live input of the
    /// session's (`midi`, feature `midi`), that; a clip's MPE transform goes
    /// with the clip. A clip or live input
    /// feeding `node` stays, to be re-aimed (`Midi::retarget`) or removed.
    ///
    /// # Errors
    ///
    /// [`RemoveError::NoSuchNode`] when `node` is not in the graph (removed
    /// already, or never added); nothing changes, the session's declarations
    /// on it included.
    pub fn remove(
        &mut self,
        node: impl Into<NodeKey>,
    ) -> Result<Removal<'_, C::Float>, RemoveError> {
        let node = node.into();
        self.graph.remove(node)?;
        self.forget_removed();
        Ok(Removal::new(&mut self.graph, node))
    }

    /// Forgets what the session declared on every node removed since the
    /// last call, through this session's [`remove`](Self::remove) or a removal of
    /// the session's own: modulation, MIDI routes, the capture of an input.
    fn forget_removed(&mut self) {
        // Until nothing more goes: a clip's transform removed with its clip
        // is a removal too.
        loop {
            let removed = self.graph.take_removed();
            if removed.is_empty() {
                break;
            }
            for node in removed {
                self.forget_node(node);
            }
        }
    }

    /// What the session holds of `node`, which left the graph, goes.
    fn forget_node(&mut self, node: NodeKey) {
        self.mods.own.forget_node(node);
        if let Some(pair) = self.midi.forget_node(node) {
            // A clip and its transform are one wiring: the other goes too
            // (it is in the graph unless the caller removed it first).
            let _ = self.graph.remove(pair);
        }
        self.inputs.forget(node);
    }

    /// The session's own modulation declarations, read: its routes and
    /// sources ([`ModsView::routes`](crate::ModsView::routes),
    /// [`routes_to`](crate::ModsView::routes_to),
    /// [`sources`](crate::ModsView::sources)). Reading marks nothing
    /// edited; [`mods_mut`](Self::mods_mut) edits.
    #[must_use]
    pub fn mods(&self) -> crate::ModsView<'_> {
        crate::Mods::view_of(&self.mods.own)
    }

    /// The session's own modulation declarations, to edit (design 015
    /// §4.3, §5.1): the next update applies them. A session has one
    /// declarer: while these hold anything, a host's
    /// [`ModInput`] is refused.
    pub fn mods_mut(&mut self) -> crate::Mods<'_> {
        // Whatever the handle edits is built into the next apply's
        // declarations.
        self.mods.own.edited();
        crate::Mods {
            own: &mut self.mods.own,
        }
    }

    /// The modulation as applied: the lanes, what is unresolved, the
    /// refusal held.
    pub fn mod_state(&self) -> &crate::ModState {
        &self.mods.state
    }
}
