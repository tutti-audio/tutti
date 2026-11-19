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

pub use event::{CrossfadeControls, SessionEvent};
pub use phases::{ModInput, PHASES, Phase, PhaseCx};
pub use root::RootTopologyStatus;
#[cfg(feature = "midi")]
pub use taps::MidiNodes;

use std::fmt::Debug;
use std::sync::Arc;
#[cfg(feature = "plugin")]
use std::time::Instant;

use tutti_engine::{OutputSpec, StreamFaults};
use tutti_graph::NodeKey;
use tutti_music::MeterMap;
use tutti_tempo::TempoMap;
use tutti_units::{Amplitude, SampleRate};

use crate::clock::sealed::Streaming;
use crate::graph::Graph;
use crate::jobs::Jobs;
use crate::value::{SlotCell, ValueState, ValueTable};
use crate::{Clock, Offline, OutputPolicy, RealTime, RemoveError, SessionFloat};
use parts::{OutputPart, PoolPart, Reports, TransportPart};

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
///     .outputs(ChannelLayout::MONO).max_block(MaxBlock::of(Samples(64))).build()
///     .unwrap();
/// let (dc, _) = session.graph().add(ForkByClone(Const::mono(0.5)));
/// session.graph().set_sources_hard(Root::output(0), [OutPort::new(dc, 0)]).unwrap();
/// session.update();
///
/// let mut block = [0.0f32; 64];
/// session.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO).unwrap());
/// assert!(block.iter().all(|&s| s == 0.5));
/// ```
#[must_use = "dropping a session ends it, and on a real-time clock stops its stream"]
#[derive(Debug)]
pub struct Session<K: AppKey = &'static str, C: Clock = Offline> {
    /// The value lock and the table every node's params are written through,
    /// shared with the graph and the session's [`Knobs`](crate::Knobs).
    pub(crate) values: Arc<ValueState<K>>,
    /// Runs the work other threads handed to the main thread when the session
    /// drops on it, before anything else drops (the plugins then close, and
    /// the last field collects what they hand over).
    #[cfg(feature = "plugin")]
    pub(crate) _early_work: EarlyWork,
    /// What runs the blocks (the clock's [`Running`](Clock::Running)). First,
    /// so a stream stops before the session's own state drops.
    pub(crate) running: C::Running,
    pub(crate) graph: Graph<C::Float>,
    /// What the app declared loaded, and the ids of the session's resources.
    pub(crate) pool: PoolPart<K, C>,
    /// The plugin entries whose node waits to swap to a reloaded plugin.
    #[cfg(feature = "plugin")]
    pub(crate) pooled_swaps: std::collections::BTreeSet<crate::PoolKey>,
    /// The transport and the figures of the plan sent last.
    pub(crate) transport: TransportPart,
    /// Where the mix lands: the root, the policy, the engine's output readings.
    pub(crate) output: OutputPart<C::Float>,
    /// What the last update read, for the next one's reports.
    pub(crate) reports: Reports,
    #[cfg(feature = "midi")]
    pub(crate) midi: Option<MidiNodes>,
    /// Clips, live inputs, routes and the outputs.
    #[cfg(feature = "midi")]
    pub(crate) midi_state: crate::midi::MidiState,
    /// What modulation applied, and the session's own declarations.
    pub(crate) mods: crate::modulation::ModSession,
    #[cfg(feature = "midi-hardware")]
    pub(crate) midi_io: Option<tutti_midi_hardware::MidiSession>,
    /// Who the session is on the wire, as set ([`Session::policy`]).
    #[cfg(feature = "midi-hardware")]
    pub(crate) midi_policy: Arc<crate::value::Cell<crate::MidiPolicy>>,
    #[cfg(feature = "sampler")]
    pub(crate) disk_streamer: Option<tutti_sampler::DiskStreamer<tutti_sampler::Running, C::Float>>,
    /// Names the [`SamplerLoad`](crate::SamplerLoad)s.
    #[cfg(feature = "sampler")]
    pub(crate) sampler_opens: tutti_types::Mint,
    /// What the opens landed, until the host takes it ([`Session::take_stream`]).
    #[cfg(feature = "sampler")]
    pub(crate) sampler_streams: crate::sampler::OpenedStreams<C::Float>,
    /// The one-shot listing of the output devices, until it answers.
    #[cfg(feature = "device")]
    pub(crate) output_listing: Option<crate::watch::OutputListing>,
    /// A listing was started by [`Session::list_outputs_once`].
    #[cfg(feature = "device")]
    pub(crate) outputs_listed: bool,
    /// The capture inputs added ([`Session::add_input`]) and the host they open on.
    #[cfg(feature = "capture")]
    pub(crate) inputs: crate::input::Inputs<C::Float>,
    /// How often a healthy plugin's state is captured.
    #[cfg(feature = "plugin")]
    pub(crate) snapshot_interval: std::time::Duration,
    /// What a hosted plugin's death or hang does; `None`: a pooled plugin's
    /// node stays silent at its key, a host-inserted plugin's is removed.
    #[cfg(feature = "plugin")]
    pub(crate) recovery: Option<crate::RecoveryPolicy>,
    /// When a hosted plugin's unanswered blocks are a hang.
    #[cfg(feature = "plugin")]
    pub(crate) hang: crate::HangPolicy,
    /// The `now` of the last `Phase::Plugins`: the time a landing reload is
    /// measured at.
    #[cfg(feature = "plugin")]
    pub(crate) plugins_now: Instant,
    /// The render mode last announced.
    #[cfg(feature = "plugin")]
    pub(crate) render_mode: crate::plugin::RenderModeState,
    /// The bounce token and the leftovers' channel.
    #[cfg(feature = "export")]
    pub(crate) bounce: crate::bounce::BounceState,
    /// The blocking work running on the spawner, landed in `Phase::Collect`.
    pub(crate) jobs: Jobs,
    /// What every plugin this session loads hands to the main thread: the closes of
    /// a drop off the main thread. The handle is cloned into each load.
    #[cfg(feature = "plugin")]
    pub(crate) work: tutti_plugin_types::MainWorkHandle,
    /// The receiving end of `work`, drained on the main thread by the session's
    /// update. Declared last, so it drops after every plugin the session holds: a
    /// close handed over while the session drops is still collected, and
    /// [`MainWork`](tutti_plugin_types::MainWork)'s own drop runs what is left.
    #[cfg(feature = "plugin")]
    pub(crate) main_work: SharedWork,
}

/// The fallback of `Session::close`, as a field: on the main thread, runs the
/// work other threads handed over, and the plugins then drop with the fields,
/// their closes collected by the last field's own drop.
///
/// End a session that hosted an in-process plugin with `close` on the main
/// thread; dropped elsewhere, it leaks those plugins and counts them in
/// `tutti_plugin_types::MainWork::stranded()`; `MainWork::deferred_drops()` and
/// `contained_panics()`, both in `tutti_plugin_types`, are the other two
/// process-wide readings. It is a field, not a `Drop` of the session, so a
/// session moves to another clock field by field.
#[cfg(feature = "plugin")]
#[derive(Debug)]
pub(crate) struct EarlyWork(pub(crate) SharedWork);

#[cfg(feature = "plugin")]
impl Drop for EarlyWork {
    fn drop(&mut self) {
        if let Some(main) = tutti_plugin_types::MainThreadMarker::new() {
            self.0.run(main);
        }
    }
}

/// The session's collector, held by the session's last field and its first:
/// the collector's receiving end is not `Sync`, so it sits behind a lock the
/// control thread alone takes.
#[cfg(feature = "plugin")]
#[derive(Clone, Debug)]
pub(crate) struct SharedWork(Arc<std::sync::Mutex<tutti_plugin_types::MainWork>>);

#[cfg(feature = "plugin")]
impl SharedWork {
    pub(crate) fn new(work: tutti_plugin_types::MainWork) -> Self {
        Self(Arc::new(std::sync::Mutex::new(work)))
    }

    /// [`MainWork::run`](tutti_plugin_types::MainWork::run).
    pub(crate) fn run(&self, main: tutti_plugin_types::MainThreadMarker) -> usize {
        use tutti_rt::LockExt;
        self.0.lock_recover().run(main)
    }
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
        if self.graph.is_edited() {
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
    /// Jobs in flight carry over: a job's lander is typed to nothing of the
    /// clock, and lands on the converted session. The runs waiting for an
    /// entry to load (`when_loaded`) are typed to the clock they were asked of
    /// and do not: the caller has run or refused them first, so this runs no
    /// closure of the app's.
    #[cfg(feature = "device")]
    pub(crate) fn with_clock<N: Clock<Float = C::Float>>(
        self,
        running: N::Running,
    ) -> (Session<K, N>, C::Running) {
        let Session {
            values,
            #[cfg(feature = "plugin")]
            _early_work,
            running: old_running,
            graph,
            pool,
            #[cfg(feature = "plugin")]
            pooled_swaps,
            transport,
            output,
            reports,
            #[cfg(feature = "midi")]
            midi,
            #[cfg(feature = "midi")]
            midi_state,
            mods,
            #[cfg(feature = "midi-hardware")]
            midi_io,
            #[cfg(feature = "midi-hardware")]
            midi_policy,
            #[cfg(feature = "sampler")]
            disk_streamer,
            #[cfg(feature = "sampler")]
            sampler_opens,
            #[cfg(feature = "sampler")]
            sampler_streams,
            #[cfg(feature = "device")]
            output_listing,
            #[cfg(feature = "device")]
            outputs_listed,
            #[cfg(feature = "capture")]
            inputs,
            #[cfg(feature = "plugin")]
            snapshot_interval,
            #[cfg(feature = "plugin")]
            recovery,
            #[cfg(feature = "plugin")]
            hang,
            #[cfg(feature = "plugin")]
            plugins_now,
            #[cfg(feature = "plugin")]
            render_mode,
            #[cfg(feature = "export")]
            bounce,
            jobs,
            #[cfg(feature = "plugin")]
            work,
            #[cfg(feature = "plugin")]
            main_work,
        } = self;
        let pool = pool.rebind();
        (
            Session {
                values,
                #[cfg(feature = "plugin")]
                _early_work,
                running,
                graph,
                pool,
                #[cfg(feature = "plugin")]
                pooled_swaps,
                transport,
                output,
                reports,
                #[cfg(feature = "midi")]
                midi,
                #[cfg(feature = "midi")]
                midi_state,
                mods,
                #[cfg(feature = "midi-hardware")]
                midi_io,
                #[cfg(feature = "midi-hardware")]
                midi_policy,
                #[cfg(feature = "sampler")]
                disk_streamer,
                #[cfg(feature = "sampler")]
                sampler_opens,
                #[cfg(feature = "sampler")]
                sampler_streams,
                #[cfg(feature = "device")]
                output_listing,
                #[cfg(feature = "device")]
                outputs_listed,
                #[cfg(feature = "capture")]
                inputs,
                #[cfg(feature = "plugin")]
                snapshot_interval,
                #[cfg(feature = "plugin")]
                recovery,
                #[cfg(feature = "plugin")]
                hang,
                #[cfg(feature = "plugin")]
                plugins_now,
                #[cfg(feature = "plugin")]
                render_mode,
                #[cfg(feature = "export")]
                bounce,
                jobs,
                #[cfg(feature = "plugin")]
                work,
                #[cfg(feature = "plugin")]
                main_work,
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
        use crate::value::{ClickVolumeSlot, MeterSlot, SettingSlot, SlotId, TempoSlot};
        #[cfg(feature = "midi")]
        let following = self.midi_state.following_flag();
        #[cfg(not(feature = "midi"))]
        let following = Arc::default();
        table.provide(
            SlotId::Tempo,
            Arc::new(TempoSlot {
                settings: self.transport.settings().clone(),
                following,
            }) as SlotCell<Arc<TempoMap>>,
        );
        table.provide(
            SlotId::Meter,
            Arc::new(MeterSlot(self.transport.settings().project_meter().clone()))
                as SlotCell<Arc<MeterMap>>,
        );
        table.provide(
            SlotId::ClickVolume,
            Arc::new(ClickVolumeSlot(Arc::clone(&click))) as SlotCell<Amplitude>,
        );
        table.provide(
            SlotId::OutputPolicy,
            Arc::clone(&self.output.policy) as SlotCell<OutputPolicy>,
        );
        #[cfg(feature = "midi-hardware")]
        table.provide(
            SlotId::MidiPolicy,
            Arc::clone(&self.midi_policy) as SlotCell<crate::MidiPolicy>,
        );
        #[cfg(feature = "midi")]
        table.provide(
            SlotId::OutputLatency,
            self.midi_state.output_latency_cell() as SlotCell<tutti_midi_runtime::OutputLatency>,
        );
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
    /// Ends the session on the main thread: runs the work other threads handed to
    /// it, drops the session's plugins (each closes on `main`, or hands its close
    /// to the session's collector), and runs what that handed over. Nothing a
    /// plugin owes the main thread is left undone.
    ///
    /// End a session that hosted an in-process plugin with `close` on the main
    /// thread; dropped elsewhere, it leaks those plugins and counts them in
    /// [`stranded()`](tutti_plugin_types::MainWork::stranded). Plugins dropped off their
    /// own thread are counted in
    /// [`deferred_drops()`](tutti_plugin_types::MainWork::deferred_drops). Both counts are
    /// process-wide, as is [`contained_panics()`](tutti_plugin_types::contained_panics),
    /// the panics plugin callbacks caused and the host contained.
    #[cfg(feature = "plugin")]
    pub fn close(self, main: tutti_plugin_types::MainThreadMarker) {
        let mut session = self;
        session.main_work.run(main);
        // The plugins close in the drop below and hand what they cannot do
        // there to their handles, which feed the collector kept out here;
        // the session keeps an empty one.
        let (inert, _) = tutti_plugin_types::MainWork::new();
        let collected = std::mem::replace(&mut session.main_work, SharedWork::new(inert));
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
        {
            pinned += self.mods.state.reclaim();
        }
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = &self.midi_io {
            use tutti_rt::Reclaim;
            pinned += io.ports().inlets().reclaim();
        }
        pinned
    }

    /// Takes `node` out of the graph ([`Graph::remove`]). Any thread that
    /// owns the session: the removal frees nothing itself. The unit is freed
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
    pub fn remove(&mut self, node: impl Into<NodeKey>) -> Result<(), RemoveError> {
        let node = node.into();
        self.graph.remove(node)?;
        self.forget_removed();
        Ok(())
    }

    /// Forgets what the session declared on every node removed since the
    /// last call, through this session's [`remove`](Self::remove) or the
    /// graph's own: modulation, MIDI routes, the capture of an input.
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
        #[cfg(feature = "midi")]
        if let Some(pair) = self.midi_state.forget_node(node) {
            // A clip and its transform are one wiring: the other goes too
            // (it is in the graph unless the caller removed it first).
            let _ = self.graph.remove(pair);
        }
        #[cfg(feature = "capture")]
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
