//! [`Session`]: the graph, its transport and engine side, and the order they
//! run in ([`PHASES`]).

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use tutti_graph::NodeKey;
use tutti_graph::PrerollTable;
use tutti_nodes::ClickSettings;
use tutti_tempo::TempoMap;
use tutti_transport::{FadeOut, LoopEdit, MotionEvent, TimedError, Transport};
use tutti_types::{At, Beat, Bpm, ChannelLayout, ChannelTopology, LoopRange, SampleRate, Samples};

use crate::RootDeclaration;
use crate::graph::{Committed, CrossfadeId, Graph, Retried};
use crate::{Clock, Offline, RealTime};
use crate::{HeardOpen, HeardTapError, Playhead, RemoveError, TempoError};

/// One step of [`Session::update`]. [`PHASES`] is their order.
///
/// A host that runs the steps itself calls [`Session::run_phase`] once per phase, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Phase {
    /// What the session's jobs sent lands first (`SessionBuilder::spawn_with`:
    /// each message, then each end, in arrival order). Then the pool's
    /// landings are reported ([`SessionEvent::Pool`], in the order the app's
    /// set names the entries), then its releases (by key), then crossfades
    /// that waited out a re-prepare are retried, what bounces
    /// sent back is dropped (feature `export`; and a bounce in flight for
    /// an hour of `PhaseCx::now` warned about, once), then
    /// `Editor::collect` frees what the audio thread retired (here, on the
    /// main thread) and, mid re-prepare, sends the resumed plan.
    Collect,
    /// Hosted plugins' health, latency, parameter edits and state
    /// snapshots (feature `plugin`): a dead pooled plugin's node stays at its
    /// key, silent, and a host-inserted plugin's is removed
    /// (`PluginChange::Died`), a moved latency handed to the graph
    /// (`PluginChange::LatencyChanged`), a due snapshot, a parameter
    /// read and a reload submitted to the session's spawner (their results land in
    /// the next `Phase::Collect`).
    Plugins,
    /// The plugins' render mode (features `plugin` and `export`): Offline
    /// while an export is in flight, Realtime otherwise, each plugin told on
    /// a change or when it was never told.
    RenderMode,
    /// Modulation's apply: the declarations (the
    /// host's, `PhaseCx::mods`, or the session's own, `Session::mods_mut`)
    /// onto the graph, staging what is new for the commit. A refusal is
    /// `SessionEvent::Modulation`.
    Modulation,
    /// The output policy staged since the last update, applied
    /// ([`Session::policy`]); then the commit, iff the graph was edited since
    /// the last one, and the compensation of the plan it sent (or of a re-prepare's resumed plan)
    /// published.
    Commit,
    /// Modulation's re-apply and resyncs: while a
    /// target is unresolved (its node was new before the commit), the
    /// declarations apply again and what that built is committed, so the
    /// lane plays from the update its node does; then the note-envelope
    /// resyncs the apply owes are sent.
    AfterCommit,
    /// Every output's event tap drained to its device or sink (feature `midi`:
    /// the `midi_sink` entries, and with `midi-hardware` the `midi_output` ones).
    MidiOut,
    /// The stream's faults, as [`SessionEvent::Fault`] (a real-time clock),
    /// and the device's own events (feature `device`).
    Poll,
    /// Frees, here, what the audio thread let go of in every cell the
    /// session publishes to (`Publisher::reclaim`): the project meter and the
    /// plugin slots following it, the modulation
    /// lanes' programs, the session's clips, the hardware input's MPE
    /// request and port set. A value a block still held when it was
    /// replaced stays retired until a scan after the block ends; a cell
    /// that is not published again gets that scan here. A lock and a
    /// length check per cell when nothing is retired. It also closes the
    /// graph's taps whose readers were dropped
    /// ([`Taps::open`](crate::Taps::open),
    /// [`Taps::open_event`](crate::Taps::open_event)), and emits what the
    /// graph's nodes logged on the audio thread as `tracing` events.
    Reclaim,
}

/// Every [`Phase`], in the order [`Session::update`] runs them.
pub const PHASES: &[Phase] = &[
    Phase::Collect,
    Phase::Plugins,
    Phase::RenderMode,
    Phase::Modulation,
    Phase::Commit,
    Phase::AfterCommit,
    Phase::MidiOut,
    Phase::Poll,
    Phase::Reclaim,
];

/// What one phase reads besides the session.
#[derive(Clone, Copy, Debug)]
pub struct PhaseCx<'a> {
    /// The time thresholds are measured on (a test passes its own). On the OS
    /// clock: it is also the time the MIDI outputs are drained at, which their
    /// sends are timed against, so it may be stale by the phases that ran
    /// before the drain.
    pub now: Instant,
    /// The host's modulation declarations and the resolver for its keys;
    /// `None`: the session's own ([`Session::mods_mut`]). Read by the
    /// [`Modulation`](Phase::Modulation) and
    /// [`AfterCommit`](Phase::AfterCommit) phases.
    pub mods: Option<ModInput<'a>>,
    /// The proof the calling thread is the main thread, when it is: the work
    /// only that thread may do (the retired plugins' drops, the editors'
    /// service, the work other threads handed over, the pool's main stages)
    /// runs in a phase that holds one and waits otherwise. [`PhaseCx::at`]
    /// asks the OS ([`MainThreadMarker::new`](tutti_rt::MainThreadMarker::new)).
    pub main: Option<tutti_rt::MainThreadMarker>,
}

impl PhaseCx<'_> {
    /// A context at `now`, with the session's own declarations, on the thread
    /// the OS says is the main one, if this is it.
    pub fn at(now: Instant) -> Self {
        Self {
            now,
            mods: None,
            main: tutti_rt::MainThreadMarker::new(),
        }
    }
}

impl<'a> PhaseCx<'a> {
    /// The same context with the host's declarations.
    #[must_use]
    pub fn with_mods(mut self, mods: ModInput<'a>) -> Self {
        self.mods = Some(mods);
        self
    }
}

/// A host's modulation declarations, borrowed for one phase, and how its
/// keys resolve to nodes.
#[derive(Clone, Copy)]
pub struct ModInput<'a> {
    /// The declarations, as `tutti_modulation` takes them.
    pub declarations: tutti_modulation::Declarations<'a>,
    /// The host's keys, resolved.
    pub keys: &'a dyn crate::ModKeys,
}

impl std::fmt::Debug for ModInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModInput").finish_non_exhaustive()
    }
}

/// Something an [`update`](Session::update) found for the host.
#[non_exhaustive]
pub enum SessionEvent {
    /// A [`Job`](crate::Job) finished: take its result with
    /// [`Landed::take`](crate::Landed::take). Reported once, in the update
    /// that found it, unless a [`Job::wait`](crate::Job::wait) took it.
    Landed(crate::Landed),
    /// The graph's edits could not be committed: the plan sent last keeps
    /// playing. Each update tries again; a refusal is reported once, until a
    /// commit is sent or it fails another way.
    CommitRefused(tutti_graph::CommitError),
    /// A crossfade that waited out a re-prepare
    /// ([`Crossfade::Waiting`](crate::Crossfade::Waiting)) swapped its node
    /// in: `controls` are the incoming node's `IntoNode::Controls`, boxed
    /// (`()` for a plugin), its params already addressed.
    CrossfadeLanded {
        /// The node it swapped behind.
        node: NodeKey,
        /// The id [`Graph::crossfade`] returned.
        id: CrossfadeId,
        /// The incoming node's controls.
        controls: Box<dyn Any + Send>,
    },
    /// A crossfade that waited out a re-prepare was refused when it was
    /// retried (the graph is poisoned, or the node went): the incoming node
    /// was dropped.
    CrossfadeFailed {
        /// The node it was to swap behind.
        node: NodeKey,
        /// The id [`Graph::crossfade`] returned.
        id: CrossfadeId,
        /// Why.
        refused: crate::CrossfadeRefused,
    },
    /// A node re-inserted with a narrower shape (a plugin whose ports changed,
    /// a node re-declared at a new width) lost wiring to the ports it no
    /// longer has. Reported once, by the update that commits the re-insert,
    /// per node: the audio edges, the event edges and the bus edges (root feeds
    /// included) that were dropped, by the ports the app named them with.
    /// Edges to the ports the node still has are kept, gains included. A
    /// refused commit reports nothing: the record waits for the update whose
    /// commit lands.
    ///
    /// A feedback edge is listed as the pair of ports it joined (its delay
    /// is not part of an [`Edge`](crate::Edge)). A modulation route onto a
    /// dropped edge, or a param or edge-gain lane the node's lost event
    /// outputs drove, is not listed: the route stays declared and reads
    /// unresolved (`Problem::Unresolved`) until its target
    /// is wired again.
    Unwired {
        /// The node whose re-insert dropped the wiring.
        node: NodeKey,
        /// The audio edges dropped, into or out of the node (a feedback edge
        /// as the pair of ports it joined).
        edges: Vec<crate::Edge>,
        /// The event edges dropped, into or out of the node.
        event_edges: Vec<crate::EventEdge>,
        /// The bus edges dropped, into or out of the node, as the app wired
        /// them: the source bus and the sink, a node's input bus or the
        /// root.
        bus_edges: Vec<(tutti_graph::BusSource, crate::BusSink)>,
    },
    /// A node with a bus besides its main one (an aux or sidechain bus, a
    /// plugin's) had its audio wiring changed: a port gained its first source or
    /// consumer, or lost its last (an edge, a global output, a tap or a
    /// meter). Reported once per node by the update that sends the commit
    /// (or the next update, for a commit the host made itself), carrying the
    /// node's first `was` and last `now` of the commits since the last
    /// report; a node whose wiring is as it was reports nothing. It is the
    /// control-side notice a plugin needs to activate or deactivate an aux
    /// bus; the block that applies the commit reads the same answer through
    /// [`Io::wired_inputs`](tutti_graph::Io::wired_inputs). A node with only
    /// a main bus is never reported; it reads its per-block masks.
    Rewired {
        /// The node.
        node: NodeKey,
        /// Its wired ports before.
        was: tutti_graph::Wired,
        /// Its wired ports now.
        now: tutti_graph::Wired,
    },
    /// A node's event output port lost events since the last update: they
    /// were past its capacity (a note-off only past the room set aside for
    /// note-offs), a PDC or feedback delay it feeds was full, or the node
    /// left out an event because the one it pairs with was refused (a clip's
    /// note-off of a note-on the port refused). One per port per update, the
    /// sum over the blocks of the update. A note-off refused and sent again
    /// later counts as refused.
    ///
    /// `port` is `None` for a loss no output port owns. The graph's total is
    /// [`LoadReading::dropped_events`](tutti_metering::LoadReading::dropped_events), exact
    /// where this is per port: a block that loses at more than 64 places, or
    /// a host that does not run `Collect` for long enough to fill a ring of
    /// 256 entries, leaves some out here and not there.
    EventsDropped {
        /// The node.
        node: NodeKey,
        /// Its event output port, `None` for a loss the node had that no
        /// output port owns: events a plugin took in and did not deliver, a
        /// scheduled command that did not fit.
        port: Option<u16>,
        /// How many events were refused or withheld.
        count: u64,
        /// How many of them a node says it then wrote (a note-off it sent
        /// again): `count - recovered` never arrived, summed over updates.
        recovered: u64,
    },
    /// Events the graph lost that no [`EventsDropped`](Self::EventsDropped)
    /// names: a merge's, and those a block that lost at more than 64 places
    /// or a full report ring left out. The count since the last update.
    EventsDroppedElsewhere(u64),
    /// The engine rendered silence for blocks it could not render: an
    /// **audible dropout** (the listener hears a gap, and a heard tap or
    /// recording takes the silence too). The count is the blocks silenced
    /// since the last update that reported, one event per update that saw
    /// any; it is derived on the control side from
    /// [`LoadReading::silenced`](tutti_metering::LoadReading::silenced), so
    /// the audio thread does nothing more than count.
    ///
    /// A block is silenced when the graph was held by the control side as
    /// the callback came (an edit applying while the device asked for
    /// audio), when the output buffer's width is one the engine has no fold
    /// for ([`Engine::support_output`](tutti_engine::Engine::support_output)),
    /// when the graph has no output channels, or when the graph's root width
    /// cannot be placed on the device's. Blocks that merely ran past their
    /// budget are not silenced and have no event: that is the polled
    /// [`LoadReading::over_budget`](tutti_metering::LoadReading::over_budget).
    /// A [`LoadMeter::reset`](tutti_metering::LoadMeter::reset) between
    /// updates restarts the count without an event.
    BlocksSilenced(u64),
    /// The session's rate changed with its clock: a switch onto a sound card
    /// that runs at another rate, or the card's rate carried to the timer. A
    /// capture a host holds itself (not one the session added) is at the old
    /// rate and is the host's to reopen.
    RateChanged {
        /// The rate the graph ran at.
        from: tutti_types::SampleRate,
        /// The rate it runs at now.
        to: tutti_types::SampleRate,
    },
    /// The stream of a real-time clock reported a fault: a device unplugged
    /// mid-session, a callback that panicked, a timer that fell behind. The
    /// most recent one since the last poll, as
    /// [`Session::faults`] counts them.
    Fault(tutti_engine::StreamFault),
    /// The sound card or its devices: a device that came or went, a
    /// restart wanted, done or failed ([`DeviceEvent`](crate::DeviceEvent)).
    #[cfg(feature = "device")]
    Device(crate::DeviceEvent),
    /// A take of the session's: its first frame arrived, it ended, or it
    /// failed ([`TakeEvent`](crate::TakeEvent)).
    #[cfg(all(feature = "device", feature = "audio-io"))]
    Take(crate::TakeEvent),
    /// A modulation problem (design 015 §4.7), the one type
    /// [`ModState::problems`](crate::ModState::problems) lists. Reported
    /// once: a refusal when it starts or changes (the problems list holds it
    /// while it lasts), a target that has stayed unresolved for the session's
    /// warning time ([`SessionBuilder::unresolved_warning`](crate::SessionBuilder::unresolved_warning),
    /// 4 s by default) of [`PhaseCx::now`], likely a key no node is ever bound to,
    /// once per stretch it stays unresolved.
    Modulation(crate::Problem),
    /// A MIDI event of the session's, by area (design 030, convention 1):
    /// a device came or went, a held input or the output was lost, a
    /// negotiation settled, the device watch ended, or events were dropped.
    /// See [`MidiReport`](crate::MidiReport).
    #[cfg(feature = "midi")]
    Midi(crate::MidiReport),
    /// A stream an [`open_stream`](Session::open_stream) started opened or
    /// failed to. See [`SamplerEvent`](crate::SamplerEvent).
    #[cfg(feature = "sampler")]
    Sampler(crate::SamplerEvent),
    /// A hosted plugin's event, by area: what the plugin reported (its own edit of a parameter, a
    /// new tail) and every transition of its life (died, reloading, recovered,
    /// gave up). See [`PluginEvent`](crate::PluginEvent).
    #[cfg(feature = "plugin")]
    Plugin(crate::PluginEvent),
    /// A reading of one peer changed: read `fact` again on the token whose
    /// [`Resource::id`](crate::Resource::id) is `id`. The notice carries no
    /// value.
    ResourceChanged {
        /// The peer's token.
        id: crate::ResourceId,
        /// Which of its readings changed.
        fact: crate::ResourceFact,
    },
    /// A reader of the root was opened again, because the root's width is no
    /// longer the one the reader has: a tap from
    /// [`Taps::open_output`](crate::Taps::open_output), or the master meter
    /// once a host took it ([`Session::take_master_meter`]). `id` names the new
    /// reader, which the session holds until
    /// [`Session::take_reopened`] claims it (an event left unread loses
    /// nothing); `topology` is the root's now. The old reader reads closed, so
    /// no reader takes twelve channels for two: a tap's reads what its rings
    /// held when the commit that changed the root was applied and then ends
    /// ([`Pull::Ended`](tutti_buffers::Pull::Ended)), a meter's reads
    /// [`is_closed`](tutti_graph::MeterReader::is_closed). A second reopen
    /// before the claim replaces the held reader, and the earlier one ends.
    ///
    /// The new tap is fed from the first block of the plan that carries the
    /// change, and what the old one holds ends on the block before it: a root
    /// crossfade fades the engine's output, never what a tap reads, so the
    /// tap switches to the new root at the commit and never reads the old
    /// root's channels. The event comes in the update that sends the commit.
    RootReaderReopened {
        /// The reader to claim.
        id: crate::RootReader,
        /// The root's topology the reader was opened for.
        topology: ChannelTopology,
    },
    /// A pool entry's change: its load landed or failed, a reload landed, or it
    /// was released. See [`PoolEvent`](crate::PoolEvent); `key` is the key the
    /// app declared it under ([`PoolKey::get`](crate::PoolKey::get)).
    Pool {
        /// The entry's key.
        key: crate::PoolKey,
        /// What changed.
        event: crate::PoolEvent,
    },
}

impl std::fmt::Debug for SessionEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Landed(landed) => f.debug_tuple("Landed").field(landed).finish(),
            Self::CommitRefused(e) => f.debug_tuple("CommitRefused").field(e).finish(),
            Self::CrossfadeLanded { node, id, .. } => f
                .debug_struct("CrossfadeLanded")
                .field("node", node)
                .field("id", id)
                .finish_non_exhaustive(),
            Self::CrossfadeFailed { node, id, refused } => f
                .debug_struct("CrossfadeFailed")
                .field("node", node)
                .field("id", id)
                .field("refused", refused)
                .finish(),
            Self::RateChanged { from, to } => f
                .debug_struct("RateChanged")
                .field("from", from)
                .field("to", to)
                .finish(),
            Self::Unwired {
                node,
                edges,
                event_edges,
                bus_edges,
            } => f
                .debug_struct("Unwired")
                .field("node", node)
                .field("edges", edges)
                .field("event_edges", event_edges)
                .field("bus_edges", bus_edges)
                .finish(),
            Self::Rewired { node, was, now } => f
                .debug_struct("Rewired")
                .field("node", node)
                .field("was", was)
                .field("now", now)
                .finish(),
            Self::Fault(fault) => f.debug_tuple("Fault").field(fault).finish(),
            #[cfg(feature = "device")]
            Self::Device(event) => f.debug_tuple("Device").field(event).finish(),
            #[cfg(all(feature = "device", feature = "audio-io"))]
            Self::Take(event) => f.debug_tuple("Take").field(event).finish(),
            Self::Modulation(problem) => f.debug_tuple("Modulation").field(problem).finish(),
            #[cfg(feature = "midi")]
            Self::Midi(event) => f.debug_tuple("Midi").field(event).finish(),
            #[cfg(feature = "sampler")]
            Self::Sampler(event) => f.debug_tuple("Sampler").field(event).finish(),
            Self::EventsDropped {
                node,
                port,
                count,
                recovered,
            } => f
                .debug_struct("EventsDropped")
                .field("node", node)
                .field("port", port)
                .field("count", count)
                .field("recovered", recovered)
                .finish(),
            Self::BlocksSilenced(count) => f.debug_tuple("BlocksSilenced").field(count).finish(),
            Self::EventsDroppedElsewhere(count) => f
                .debug_tuple("EventsDroppedElsewhere")
                .field(count)
                .finish(),
            #[cfg(feature = "plugin")]
            Self::Plugin(event) => f.debug_tuple("Plugin").field(event).finish(),
            Self::ResourceChanged { id, fact } => f
                .debug_struct("ResourceChanged")
                .field("id", id)
                .field("fact", fact)
                .finish(),
            Self::RootReaderReopened { id, topology } => f
                .debug_struct("RootReaderReopened")
                .field("id", id)
                .field("topology", topology)
                .finish(),
            Self::Pool { key, event } => f
                .debug_struct("Pool")
                .field("key", key)
                .field("event", event)
                .finish(),
        }
    }
}

/// The engine's MIDI, as graph nodes the builder inserts: the sync node (Beat
/// Clock, MTC and Machine Control, sending nothing until a host sets its
/// messages), with its controls. An input port's ingest node is not here: each
/// `midi_input` entry makes its own (`MidiInput::node`), and each output its own
/// (`MidiSink::node`, `MidiOutput::node`), fed by the sync node.
#[cfg(feature = "midi")]
#[derive(Clone, Debug)]
pub struct MidiNodes {
    /// Which of an input port's event outputs carries which Group and channel
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    pub input_ports: tutti_midi_runtime::MidiInputPorts,
    /// The sync node, on Group 1.
    pub sync: NodeKey,
    /// What the sync node sends (`messages()`) and could not send (`losses()`).
    pub sync_controls: std::sync::Arc<tutti_midi_runtime::SyncOutControls>,
}

/// Whether the root is as it was asked to be
/// ([`Session::root_topology_status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RootTopologyStatus {
    /// In force: the root is what was declared (or nothing was).
    Applied,
    /// Declared and waiting for a running root crossfade to end: the root in
    /// force is the previous declaration, and
    /// [`requested_root_topology`](Session::requested_root_topology) is the
    /// waiting one. It applies in the first update after the fade has ended
    /// (which needs blocks rendered), or is `Refused` if it cannot be placed
    /// then.
    Pending,
    /// Not in force, and why. The previous state stands.
    Refused(tutti_graph::RootError),
}

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
/// use tutti_types::MaxBlock;
/// use tutti_types::{ChannelLayout, SampleRate, Samples};
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
pub struct Session<C: Clock = Offline> {
    /// Runs the work other threads handed to the main thread when the session
    /// drops on it, before anything else drops (the plugins then close, and
    /// the last field collects what they hand over).
    #[cfg(feature = "plugin")]
    pub(crate) _early_work: EarlyWork,
    /// What runs the blocks (the clock's [`Running`](Clock::Running)). First,
    /// so a stream stops before the session's own state drops.
    pub(crate) running: C::Running,
    pub(crate) graph: Graph,
    /// The source of the ids of this session's resources.
    pub(crate) resource_ids: crate::peer::ResourceIds,
    /// What the app declared loaded ([`Session::pool`]).
    pub(crate) pool: crate::Pool<C>,
    /// The plugin entries whose node waits to swap to a reloaded plugin.
    #[cfg(feature = "plugin")]
    pub(crate) pooled_swaps: std::collections::BTreeSet<crate::PoolKey>,
    pub(crate) transport: Transport,
    /// The engine's load meter, offline or on a device; `None` detached.
    pub(crate) load: Option<tutti_metering::LoadMeter>,
    /// The engine's tap after the fader and the device width it was built
    /// for; `None` detached.
    pub(crate) heard: Option<(tutti_engine::HeardTaps, ChannelLayout)>,
    /// The transport's verbs and the engine's block clock behind
    /// [`playhead`](Self::playhead); the clock is `None` detached, where
    /// nothing renders.
    pub(crate) transport_handle: crate::TransportHandle,
    /// The root topology last asked for
    /// ([`declare_root_topology`](Self::declare_root_topology)), applied or not.
    pub(crate) root_request: Option<ChannelTopology>,
    /// Why `root_request` is not in force, if it is not.
    pub(crate) root_refusal: Option<tutti_graph::RootError>,
    /// Where the mix lands, as set ([`Session::policy`]).
    pub(crate) output_policy: crate::OutputPolicy,
    /// Whether `output_policy` was set and not yet applied.
    pub(crate) output_policy_staged: bool,
    pub(crate) click: Option<NodeKey>,
    pub(crate) click_settings: Arc<ClickSettings>,
    /// The per-channel pre-roll of the plan sent last, for sources outside
    /// the graph (a disk stream), republished with every plan.
    pub(crate) compensation: Arc<PrerollTable>,
    /// The total latency of the plan sent last.
    pub(crate) latency: Samples,
    /// A re-prepare resumed in this update's collect: its plan's figures
    /// are published at the commit.
    pub(crate) resumed: bool,
    /// [`LoadReading::silenced`](tutti_metering::LoadReading::silenced) as
    /// the last update read it: the next update reports the rise
    /// ([`SessionEvent::BlocksSilenced`]).
    pub(crate) silenced_seen: u64,
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
    pub(crate) midi_policy: crate::MidiPolicy,
    #[cfg(feature = "sampler")]
    pub(crate) disk_streamer: Option<tutti_sampler::DiskStreamer>,
    /// Names the [`SamplerLoad`](crate::SamplerLoad)s.
    #[cfg(feature = "sampler")]
    pub(crate) sampler_opens: tutti_types::Mint,
    /// The count of the stream's faults the last poll saw.
    pub(crate) faults_seen: u64,
    /// The one-shot listing of the output devices, until it answers.
    #[cfg(feature = "device")]
    pub(crate) output_listing: Option<crate::watch::OutputListing>,
    /// A listing was started by [`Session::list_outputs_once`].
    #[cfg(feature = "device")]
    pub(crate) outputs_listed: bool,
    /// The capture inputs added ([`Session::add_input`]) and the host they open on.
    #[cfg(all(feature = "device", feature = "audio-io"))]
    pub(crate) inputs: crate::input::Inputs,
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
    pub(crate) jobs: crate::jobs::Jobs,
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
    /// Events the session kept for its next poll: what a clock switch reported
    /// on the clock it left.
    pub(crate) carried_events: Vec<SessionEvent>,
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

impl Session<Offline> {
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
    pub fn new(rate: tutti_types::SampleRate) -> crate::Result<Self> {
        Self::builder().sample_rate(rate).build()
    }

    /// Renders one block into `output`, as a device callback would: what an
    /// offline session's host pulls. Every update's commit lands at the
    /// start of the next block.
    pub fn render(&self, output: &mut tutti_buffers::InterleavedMut<'_>) {
        self.running.engine.process(output);
    }
}

impl<C: RealTime> Session<C> {
    /// The configuration of the stream that renders the session's blocks: its
    /// rate, width and block (the callback size, or the timer's block).
    #[must_use]
    pub fn stream_spec(&self) -> &tutti_engine::OutputSpec {
        crate::clock::sealed::Streaming::output_stream(&self.running).spec()
    }

    /// The stream's fault record: one entry each time the clock's stream
    /// failed, a device unplugged, a callback that panicked, a timer that fell
    /// more than four blocks behind and began its schedule again. The same
    /// handle for the session's life; [`update`](Self::update) reports each new
    /// fault as [`SessionEvent::Fault`] and takes its message from the record
    /// ([`StreamFaults::take_last`](tutti_engine::StreamFaults::take_last)), so
    /// [`count`](tutti_engine::StreamFaults::count) is the lasting reading.
    #[must_use]
    pub fn faults(&self) -> Arc<tutti_engine::StreamFaults> {
        crate::clock::sealed::Streaming::output_stream(&self.running).faults()
    }
}

/// A detached session: the executor stays on this side, and the caller renders.
#[cfg(feature = "testing")]
impl Session<crate::Detached> {
    /// Renders one frame on the calling thread under a stopped transport:
    /// no inputs, `output` one sample per global output. Commits any edit
    /// first.
    ///
    /// # Panics
    ///
    /// On a session whose audio side was taken (`Graph::take_audio_side`).
    pub fn render_frame(&mut self, output: &mut [f32]) {
        self.render_frame_at(&tutti_graph::TransportPosition::default(), output);
    }

    /// `render_frame` with the transport at
    /// `transport`: what plays a clip in a graph driven by hand.
    pub fn render_frame_at(
        &mut self,
        transport: &tutti_graph::TransportPosition,
        output: &mut [f32],
    ) {
        if self.graph.is_edited() {
            self.commit();
        }
        self.graph.render_local(transport, output);
    }
}

impl<C: Clock> Session<C> {
    /// The same session on the clock `running` belongs to: every field moves,
    /// the clock's own state is swapped, and the old one is handed back.
    ///
    /// Jobs in flight carry over: a job's lander is typed to nothing of the
    /// clock, and lands on the converted session. The runs waiting for an
    /// entry to load (`when_loaded`) are typed to the clock they were asked of
    /// and do not: the caller has run or refused them first, so this runs no
    /// closure of the app's.
    #[cfg(feature = "device")]
    pub(crate) fn with_clock<K: Clock>(self, running: K::Running) -> (Session<K>, C::Running) {
        let Session {
            #[cfg(feature = "plugin")]
            _early_work,
            running: old_running,
            graph,
            resource_ids,
            pool,
            #[cfg(feature = "plugin")]
            pooled_swaps,
            output_policy,
            output_policy_staged,
            transport,
            load,
            heard,
            transport_handle,
            root_request,
            root_refusal,
            click,
            click_settings,
            compensation,
            latency,
            resumed,
            silenced_seen,
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
            faults_seen,
            #[cfg(feature = "device")]
            output_listing,
            #[cfg(feature = "device")]
            outputs_listed,
            #[cfg(all(feature = "device", feature = "audio-io"))]
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
            carried_events,
        } = self;
        let pool = pool.rebind();
        (
            Session {
                #[cfg(feature = "plugin")]
                _early_work,
                running,
                graph,
                resource_ids,
                pool,
                #[cfg(feature = "plugin")]
                pooled_swaps,
                output_policy,
                output_policy_staged,
                transport,
                load,
                heard,
                transport_handle,
                root_request,
                root_refusal,
                click,
                click_settings,
                compensation,
                latency,
                resumed,
                silenced_seen,
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
                faults_seen,
                #[cfg(feature = "device")]
                output_listing,
                #[cfg(feature = "device")]
                outputs_listed,
                #[cfg(all(feature = "device", feature = "audio-io"))]
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
                carried_events,
            },
            old_running,
        )
    }
}

impl<C: Clock> Session<C> {
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
    pub fn graph(&mut self) -> &mut Graph {
        &mut self.graph
    }

    /// The graph, to read.
    pub fn graph_ref(&self) -> &Graph {
        &self.graph
    }

    /// Declares what the graph root's channels are: the session's output as the
    /// engine and an export place it on the device and in a file.
    /// `Some(`[`ChannelTopology::unassigned`]`)` is independent buses, kept by
    /// index (a 4-wide one on a 6-wide device stays at 0..4, where a bare
    /// 4 is quad and its rears land at 4 and 5); a named topology is speaker
    /// feeds, folded by position. `None` takes the declaration back, and the
    /// root is the engine's order for its width again. **Nothing changes
    /// until one is declared.**
    ///
    /// The declaration states the root, so it **sets the root's width**: the
    /// graph's outputs become exactly as many as `root` has, widened or
    /// narrowed (channels past it are dropped), with no floor of the device's
    /// ([`root_width`](crate::root_width)). Afterwards the width and the
    /// declaration cannot disagree: [`Graph::widen_outputs`] refuses to widen a
    /// declared root. Declare again to change both.
    ///
    /// A declaration that changes the root crossfades: the old root's outputs
    /// fade out and the new root's fade in over the declick length (5 ms),
    /// both rendering the same nodes, so a change of layout never steps the
    /// sound. [`.hard()`](RootDeclaration::hard) on the returned declaration
    /// cuts at one block instead.
    ///
    /// A declaration made while a root crossfade runs, and not
    /// [`hard`](RootDeclaration::hard), waits: it is checked at the call (the
    /// errors below are named here), and held until the running fade ends. The
    /// newest such declaration wins. The first [`update`](Self::update) after
    /// the fade has ended applies it, and the root's width and readers
    /// ([`RootReaderReopened`](crate::SessionEvent::RootReaderReopened)) change in
    /// that update with the audio; until then the root in force, its taps and
    /// [`root_topology`](Self::root_topology) are the running fade's, and
    /// [`requested_root_topology`](Self::requested_root_topology) is the
    /// waiting one ([`Pending`](RootTopologyStatus::Pending)). It is heard up
    /// to one fade length (5 ms) late, and the fade only ends as blocks render:
    /// with none rendering (a stopped stream, an offline session nobody
    /// pulls) it waits. It stays with the graph through a device restart or a
    /// clock switch, and is checked against the device it finds when it is
    /// applied. A declaration refused at its call drops nothing: the waiting
    /// one still applies. If the
    /// device no longer places it when it is applied,
    /// [`root_topology_status`](Self::root_topology_status) is `Refused` with
    /// the error and the root in force stays. A
    /// [`hard`](RootDeclaration::hard) declaration applies at once and drops
    /// the waiting one. An export of the master
    /// places the file from the declaration (`tutti_export::Setup::root`, set by the export's fork).
    ///
    /// The device a declaration must place on is the one the session renders
    /// to (the offline `outputs`, or the open device's, and the new one on a
    /// restart); a detached session has none, and the file is checked at the
    /// export.
    ///
    /// # Errors
    ///
    /// [`Error::Output`](crate::Error::Output) when the declaration is empty, wider than the engine
    /// renders ([`MAX_CHANNELS`](tutti_types::MAX_CHANNELS)), or the
    /// device cannot place it (an unassigned root wider than the device, a channel
    /// no speaker names). Nothing is changed.
    pub fn declare_root_topology(
        &mut self,
        root: Option<ChannelTopology>,
    ) -> crate::Result<RootDeclaration<'_>> {
        self.sync_root_refusal();
        // The same request again, in force, is nothing to do.
        if !(root == self.root_request && self.root_refusal.is_none()) {
            self.root_request = root.clone();
            match self.graph.declare_root(root, false) {
                Ok(()) => self.root_refusal = None,
                Err(e) => {
                    self.root_refusal = Some(e);
                    return Err(e.into());
                }
            }
        }
        Ok(RootDeclaration {
            graph: &mut self.graph,
        })
    }

    /// Takes the refusal of a declaration that waited and could not be
    /// applied into `root_refusal`, where a device restart re-attempts it.
    pub(crate) fn sync_root_refusal(&mut self) {
        if let Some(e) = self.graph.take_apply_refusal() {
            self.root_refusal = Some(e);
        }
    }

    /// The topology last asked for with
    /// [`declare_root_topology`](Self::declare_root_topology) (or
    /// [`SessionBuilder::root_topology`](crate::SessionBuilder::root_topology)),
    /// whether or not it is in force: see
    /// [`root_topology_status`](Self::root_topology_status).
    pub fn requested_root_topology(&self) -> Option<&ChannelTopology> {
        self.root_request.as_ref()
    }

    /// Whether the root is as it was asked to be. A refusal is a value here,
    /// never only a log line. The first that holds is reported, in this order:
    ///
    /// - [`RootTopologyStatus::Refused`] with the declaration's own
    ///   [`FoldError`](tutti_buffers::FoldError) when the last request could not
    ///   be placed on the device. The previous declaration (the one
    ///   [`root_topology`](Self::root_topology) reads) stays in force, and a
    ///   **device restart re-attempts the request**: it is
    ///   [`Applied`](RootTopologyStatus::Applied) as soon as the new device
    ///   places it.
    /// - The same when a declaration that waited for a root crossfade
    ///   ([`declare_root_topology`](Self::declare_root_topology)) could not be
    ///   placed once the fade ended; a later widening does not replace it.
    /// - Otherwise `Refused` with [`Graph::widen_outputs`]'s last refusal: a
    ///   widening the declared root could not take
    ///   ([`RootError::Width`](tutti_graph::RootError::Width)), or
    ///   channels past `MAX_CHANNELS`
    ///   ([`FoldError::Unplaced`](tutti_buffers::FoldError::Unplaced), as a
    ///   [`RootError::Fold`](tutti_graph::RootError::Fold)); that
    ///   is what a host's `MasterSources` longer than the root comes to.
    /// - Otherwise [`Pending`](RootTopologyStatus::Pending) while a declaration
    ///   waits for a root crossfade, and `Applied`.
    ///
    /// "The previous declaration" includes one that is waiting: a declaration
    /// refused at its call drops nothing, so a waiting one still applies.
    pub fn root_topology_status(&self) -> RootTopologyStatus {
        match self
            .root_refusal
            .or(self.graph.apply_refusal())
            .or(self.graph.widen_refusal())
        {
            Some(e) => RootTopologyStatus::Refused(e),
            None if self.graph.root_pending() => RootTopologyStatus::Pending,
            None => RootTopologyStatus::Applied,
        }
    }

    /// The topology the root was declared with
    /// ([`declare_root_topology`](Self::declare_root_topology)), if one was.
    pub fn root_topology(&self) -> Option<&ChannelTopology> {
        self.graph.root_topology()
    }

    /// What the output device's channels are as the session folds the root
    /// into them, and whether that was
    /// [`Stated`](tutti_types::Placement::Stated)
    /// ([`SessionBuilder::device_topology`](crate::SessionBuilder::device_topology),
    /// `DeviceRestart::topology`) or
    /// [`Assumed`](tutti_types::Placement::Assumed) from the device's width
    /// alone ([`ChannelTopology::assumed`]). `None` for a session with no
    /// engine (a detached one).
    #[must_use]
    pub fn output_device(&self) -> Option<crate::OutputPlacement> {
        let (_, layout) = self.heard.as_ref()?;
        self.graph.output_placement(*layout)
    }

    /// Runs every phase in [`PHASES`] order at `Instant::now()` with the
    /// session's own declarations, and returns what it found.
    ///
    /// This is the one update. What only the main thread may do (dropping the
    /// plugins the session retired, closing the editors of those, servicing
    /// the open `Editor`s (feature `plugin`), running the work other threads
    /// handed over, and the pool's main stages) runs when it is called on the
    /// thread the OS says is the main one; called elsewhere that work waits
    /// for an update on that thread. A host whose main thread is not the OS's
    /// calls [`update_on`](Self::update_on).
    #[must_use = "the events carry faults and dropouts (a refused commit, a plugin that died, a lost block): handle them, or bind `let _events` with a reason"]
    pub fn update(&mut self) -> Vec<SessionEvent> {
        self.update_in(PhaseCx::at(Instant::now()))
    }

    /// [`update`](Self::update) on a thread that stands in for the main thread:
    /// a host whose UI thread is not the process's main one, or a test.
    #[must_use = "the events carry faults and dropouts (a refused commit, a plugin that died, a lost block): handle them, or bind `let _events` with a reason"]
    pub fn update_on(&mut self, main: tutti_rt::MainThreadMarker) -> Vec<SessionEvent> {
        let mut cx = PhaseCx::at(Instant::now());
        cx.main = Some(main);
        self.update_in(cx)
    }

    fn update_in(&mut self, cx: PhaseCx<'_>) -> Vec<SessionEvent> {
        // What a `Job::wait` updated through comes first: it happened first.
        let mut out = self.jobs.take_held();
        for &phase in PHASES {
            self.run_phase(phase, cx, &mut out);
        }
        out
    }

    /// Runs one phase, pushing what it found onto `out`. A host running the
    /// phases itself runs them in [`PHASES`] order, once each per update.
    pub fn run_phase(&mut self, phase: Phase, cx: PhaseCx<'_>, out: &mut Vec<SessionEvent>) {
        match phase {
            Phase::Collect => {
                // First, so what a job landed is in the graph this update commits.
                self.land_jobs(out);
                // What those landings and the holds' ends changed, and the pool's
                // stages on this thread.
                self.pool_events(cx.main, out);
                // What the graph's own removals left declared.
                self.forget_removed();
                // Before the collect, as a frame's spawns run before its
                // commit: a crossfade lands on the update that first finds
                // the re-prepare resumed.
                for retried in self.graph.retry_crossfades() {
                    out.push(match retried {
                        Retried::Landed { node, id, controls } => {
                            SessionEvent::CrossfadeLanded { node, id, controls }
                        }
                        Retried::Failed { node, id, refused } => {
                            SessionEvent::CrossfadeFailed { node, id, refused }
                        }
                    });
                }
                // What an export left behind (a render's fork, an edit
                // that never ran) drops here, on the main thread.
                #[cfg(feature = "export")]
                {
                    self.bounce.drop_leftovers();
                    self.bounce.watch_stall(cx.now);
                }
                let repreparing = self.graph.is_repreparing();
                self.graph.collect();
                self.sync_root_refusal();
                // Closes a plugin that was dropped off its own thread (a pool thread at an
                // export's end, say): it handed the close to this session's collector, which
                // runs on the main thread (`PhaseCx::main` is `Some` only there).
                #[cfg(feature = "plugin")]
                if let Some(main) = cx.main {
                    self.main_work.run(main);
                }
                self.report_event_losses(out);
                self.report_silenced(out);
                // Last: the landings this update's own stages queued.
                self.jobs.report_landed(out);
                // A re-prepare resumed in that collect: it sent the resumed
                // plan, whose shapes are new (a lookahead is a time, so a
                // rate change moves latencies).
                self.resumed |= repreparing && !self.graph.is_repreparing();
            }
            Phase::Commit => {
                self.apply_output_policy();
                let sent = self.graph.is_edited() && self.commit_reporting(out);
                out.extend(
                    self.graph
                        .announce_reopened()
                        .into_iter()
                        .map(|(id, topology)| SessionEvent::RootReaderReopened { id, topology }),
                );
                for u in self.graph.take_unwired() {
                    out.push(SessionEvent::Unwired {
                        node: u.node,
                        edges: u.edges,
                        event_edges: u.event_edges,
                        bus_edges: u.bus_edges,
                    });
                }
                self.report_rewired(out);
                if !sent && self.resumed {
                    self.publish_sent();
                }
                self.resumed = false;
            }
            Phase::Modulation => self
                .mods
                .modulation_phase(&mut self.graph, cx.mods, cx.now, out),
            Phase::AfterCommit => {
                {
                    // The re-apply's commit, as the update's own: published
                    // on `Sent`.
                    if self.mods.reapply(&mut self.graph, cx.mods, cx.now, out) {
                        self.commit_reporting(out);
                    }
                    self.mods.resync(&mut self.graph);
                    self.report_rewired(out);
                }
            }
            Phase::MidiOut => {
                #[cfg(feature = "midi")]
                {
                    if self.midi_state.midi_out_phase(&mut self.graph, cx.now) {
                        self.commit();
                    }
                }
            }
            Phase::Plugins => {
                #[cfg(feature = "plugin")]
                {
                    self.plugins_phase(cx, out);
                    self.rebind_plugins(cx.main, out);
                }
            }
            Phase::RenderMode =>
            {
                #[cfg(feature = "plugin")]
                self.render_mode_phase()
            }
            Phase::Poll => {
                // Who the session is on the wire is in force before the devices
                // are stepped.
                #[cfg(feature = "midi-hardware")]
                self.apply_midi_policy();
                // A device that connected is heard from the plan that holds
                // its inlet: committed now, not an update later.
                #[cfg(feature = "midi-hardware")]
                if self.midi_state.midi_input_phase(
                    self.midi_io.as_mut(),
                    self.midi.as_ref(),
                    &mut self.graph,
                    cx.now,
                    out,
                ) {
                    self.commit();
                }
                // The tempo the session follows is read after the devices:
                // what a device sent in the block before is in its tap.
                #[cfg(feature = "midi")]
                {
                    let follow = crate::midi_follow::FollowCx {
                        settings: self.transport.settings(),
                        meter: &self.click_settings.project_meter(),
                        playhead: self.playhead(),
                    };
                    let tempo_edited = self.midi_state.follow_phase(&follow, &mut self.graph, out);
                    let (actions, clock_edited) =
                        self.midi_state.clock_phase(&follow, &mut self.graph, out);
                    let (machine_actions, machine_edited) =
                        self.midi_state.machine_phase(&follow, &mut self.graph, out);
                    if tempo_edited || clock_edited || machine_edited {
                        self.commit();
                    }
                    let refused: u64 = actions
                        .into_iter()
                        .chain(machine_actions)
                        .map(|action| u64::from(!self.send_clock_action(action)))
                        .sum();
                    if refused > 0 {
                        out.push(crate::MidiReport::ClockCommandsRefused(refused).into());
                    }
                }
                out.append(&mut self.carried_events);
                let lost = self.fault_poll(out);
                #[cfg(feature = "device")]
                self.listing_poll(cx.now, out);
                crate::clock::sealed::Running::poll(&mut self.running, cx.now, lost, out);
            }
            Phase::Reclaim => {
                // Any thread that owns the session: what it frees is
                // tables, meters, programs and clips, never a unit. A tap
                // or meter whose reader was dropped goes out of the next plan.
                self.graph.reap_taps();
                self.graph.reap_event_taps();
                self.graph.reap_meters();
                if let Some((taps, _)) = &self.heard {
                    taps.reclaim();
                }
                self.reclaim();
                self.graph.drain_rt_log();
                // What the main thread owns of the retired and the open.
                #[cfg(feature = "plugin")]
                if let Some(main) = cx.main {
                    self.graph.graveyard.exhume(main);
                    self.graph.graveyard.service(main);
                }
            }
        }
    }

    /// The blocks the engine silenced since the last update, as one
    /// [`SessionEvent::BlocksSilenced`]. The meter's count is cumulative
    /// (and zeroed by a reset), so the rise is read against the last
    /// reading; a reset restarts from the new count.
    fn report_silenced(&mut self, out: &mut Vec<SessionEvent>) {
        let Some(load) = &self.load else { return };
        let now = load.peek().silenced;
        let rise = now.saturating_sub(self.silenced_seen);
        self.silenced_seen = now;
        if rise > 0 {
            out.push(SessionEvent::BlocksSilenced(rise));
        }
    }

    /// The events the graph's ports lost since the last update, as
    /// [`SessionEvent::EventsDropped`], one per port.
    fn report_event_losses(&mut self, out: &mut Vec<SessionEvent>) {
        let mut sums: std::collections::BTreeMap<(tutti_graph::NodeKey, Option<u16>), (u64, u64)> =
            std::collections::BTreeMap::new();
        for loss in self.graph.take_event_losses() {
            let sum = sums.entry((loss.node, loss.port)).or_default();
            sum.0 += u64::from(loss.count);
            sum.1 += u64::from(loss.recovered);
        }
        out.extend(
            sums.into_iter()
                .filter(|(_, (count, recovered))| *count > 0 || *recovered > 0)
                .map(
                    |((node, port), (count, recovered))| SessionEvent::EventsDropped {
                        node,
                        port,
                        count,
                        recovered,
                    },
                ),
        );
        let elsewhere = self.graph.take_unreported_event_losses();
        if elsewhere > 0 {
            out.push(SessionEvent::EventsDroppedElsewhere(elsewhere));
        }
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
        pinned += self.click_settings.project_meter().reclaim();
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

    /// Commits every edit since the last commit, now, and on `Sent`
    /// publishes the plan's compensation. For a host that must send an edit
    /// before the next update; [`update`](Self::update) commits iff the
    /// graph was edited.
    pub fn commit(&mut self) -> Committed {
        let committed = self.graph.commit();
        if committed == Committed::Sent {
            self.publish_sent();
        }
        committed
    }

    /// Pushes the nodes the commits since the last report rewired.
    fn report_rewired(&mut self, out: &mut Vec<SessionEvent>) {
        out.extend(
            self.graph
                .take_rewired()
                .into_iter()
                .map(|r| SessionEvent::Rewired {
                    node: r.node,
                    was: r.was,
                    now: r.now,
                }),
        );
    }

    /// [`commit`](Self::commit), a refusal pushed onto `out`: whether it sent.
    fn commit_reporting(&mut self, out: &mut Vec<SessionEvent>) -> bool {
        match self.commit() {
            Committed::Sent => true,
            Committed::Refused(e) => {
                if self.graph.newly_refused(&e) {
                    out.push(SessionEvent::CommitRefused(e));
                }
                false
            }
            Committed::Retry => false,
        }
    }

    /// Publish the compensation of the plan sent last: the per-channel
    /// pre-roll to [`compensation`](Self::compensation), the total to
    /// [`latency`](Self::latency). Nothing before the first plan.
    pub(crate) fn publish_sent(&mut self) {
        if let Some(sent) = self.graph.sent_compensation() {
            self.latency = sent.total();
            self.compensation.set(sent.channels());
        }
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
        #[cfg(all(feature = "device", feature = "audio-io"))]
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

    /// The session's MIDI: live inputs, clips, the hardware input's routes
    /// and the outputs.
    #[cfg(feature = "midi")]
    pub fn midi(&mut self) -> crate::Midi<'_> {
        self.midi_and_ids().0
    }

    /// [`midi`](Self::midi) and, beside it, the source of the ids of the
    /// session's resources: the two borrow disjoint fields, so a stage that
    /// opens a peer holds both.
    #[cfg(feature = "midi")]
    pub(crate) fn midi_and_ids(&mut self) -> (crate::Midi<'_>, &mut crate::peer::ResourceIds) {
        let midi = crate::Midi {
            graph: &mut self.graph,
            state: &mut self.midi_state,
            nodes: self.midi.as_ref(),
            jobs: &mut self.jobs,
            settings: self.transport.settings(),
            meter: self.click_settings.project_meter(),
            #[cfg(feature = "midi-hardware")]
            io: self.midi_io.as_mut(),
        };
        (midi, &mut self.resource_ids)
    }

    /// Reports the newest fault the stream raised since the last poll, if the
    /// clock has a stream. Whether the stream was lost: its device left.
    ///
    /// The message is *taken* from the record: a fault is reported once, by
    /// its own kind, and a later fault that only bumps the count (an overrun
    /// the record keeps no message for beside a latched panic) is not read as
    /// the old message again.
    pub(crate) fn fault_poll(&mut self, out: &mut Vec<SessionEvent>) -> bool {
        let Some(stream) = crate::clock::sealed::Running::stream(&self.running) else {
            return false;
        };
        let faults = stream.faults();
        let count = faults.count();
        if count == self.faults_seen {
            return false;
        }
        // A take that lags (a record mid-write) retries on the next poll; an
        // empty one (a counted fault whose text was lost) is seen.
        let Ok(taken) = faults.take_last() else {
            return false;
        };
        self.faults_seen = count;
        let Some(fault) = taken else {
            return false;
        };
        let lost = fault.kind == tutti_engine::StreamFaultKind::Disconnected;
        out.push(SessionEvent::Fault(fault));
        lost
    }

    /// The rate the graph is prepared at; it follows the device's.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        self.transport.sample_rate()
    }

    /// The project's tempo map as last published.
    #[must_use]
    pub fn tempo_map(&self) -> Arc<TempoMap> {
        self.transport.settings().tempo_map()
    }

    /// The playhead, in one read: its beat, the graph frame, the transport
    /// time, whether it is rolling, and the OS clock reading they were taken
    /// at ([`Playhead`]). The audio thread writes it once per render call
    /// through a sequence lock, so the fields are one instant, never a beat
    /// of one call beside the play state of another.
    ///
    /// It moves in block steps; [`beat_now`](Self::beat_now) (or
    /// [`Playhead::beat_now`] on a snapshot already held) is the beat
    /// extrapolated to the moment a UI draws. A session with no engine
    /// rendering (detached, or before the first block) reads
    /// [`Playhead::default`]: stopped at the origin, with no clock reading
    /// ([`Playhead::at`] is `None`). An offline bounce renders a fork on
    /// its own timeline and never moves this one.
    #[must_use]
    pub fn playhead(&self) -> Playhead {
        self.transport_handle.playhead()
    }

    /// The playhead's beat now, extrapolated from the last block by
    /// [`Playhead::beat_now`] on [`tutti_rt::monotonic_ns`], the clock the
    /// block was stamped on.
    ///
    /// On a session dated by [`BlockDates::Frames`](crate::BlockDates::Frames)
    /// the blocks are not on that clock: the extrapolation is measured from
    /// the block's frame time, so it reads the block's beat or at most one
    /// block past it.
    #[must_use]
    pub fn beat_now(&self) -> Beat {
        self.playhead().beat_now(tutti_rt::monotonic_ns())
    }

    /// Starts the transport rolling from where it stands, at the next block.
    /// A no-op while rolling. [`TransportHandle::play`](crate::TransportHandle::play) is the one
    /// implementation.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] when the transport's command queue is full; the
    /// event is handed back in it.
    pub fn play(&self) -> Result<(), TimedError<MotionEvent>> {
        self.transport_handle.play()
    }

    /// Stops the transport where it stands, at the next block, after the
    /// [`FadeOut`] it names.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`], as [`play`](Self::play).
    pub fn stop(&self, fade: FadeOut) -> Result<(), TimedError<MotionEvent>> {
        self.transport_handle.stop(fade)
    }

    /// Jumps to `beat` at the next block, keeping whatever the transport was
    /// doing (a locate while rolling keeps rolling), with a declick fade.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`], as [`play`](Self::play).
    pub fn locate(&self, beat: Beat) -> Result<(), TimedError<MotionEvent>> {
        self.transport_handle.locate(beat)
    }

    /// Sends a motion `event` to take effect at `at`; see
    /// [`TransportHandle::schedule`](crate::TransportHandle::schedule), the one implementation.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] when the timed-command list is full, and
    /// [`TimedError::BeatWhileStopped`] for a command that starts playback at
    /// a beat while the transport is stopped.
    pub fn schedule(
        &self,
        at: impl Into<At>,
        event: MotionEvent,
    ) -> Result<(), TimedError<MotionEvent>> {
        self.transport_handle.schedule(at, event)
    }

    /// A [`TransportHandle`](crate::TransportHandle) on this session's transport: `Send`, so another
    /// thread drives play, stop, locate and schedule.
    #[must_use]
    pub fn transport_handle(&self) -> crate::TransportHandle {
        self.transport_handle.clone()
    }

    /// Sends what a followed MIDI clock asked of the transport, at the next
    /// block, and says whether the transport took it. A command the transport's
    /// list has no room for is dropped and logged, and the caller counts it
    /// ([`MidiReport::ClockCommandsRefused`](crate::MidiReport::ClockCommandsRefused)):
    /// the source says its position again with the next message.
    #[cfg(feature = "midi")]
    fn send_clock_action(&self, action: crate::midi_follow::ClockAction) -> bool {
        use crate::midi_follow::ClockAction;
        let event = match action {
            ClockAction::Play => MotionEvent::Play,
            ClockAction::Stop => MotionEvent::stop(),
            ClockAction::Locate(beat) => MotionEvent::locate(beat),
            ClockAction::PlayFrom(beat) => MotionEvent::play_from(beat),
        };
        match self.schedule(At::NextBlock, event) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!("midi: a followed clock's {event:?} was not sent: {error}");
                false
            }
        }
    }

    /// Loops the transport over `range` from the next block, and arms it: the
    /// engine owns the loop region and takes this as one edit that lands whole
    /// ([`LoopEdit::over`]). Build the range with `LoopRange::try_from(a..b)`,
    /// which names why a region is refused.
    ///
    /// A playhead already at or past the loop's end does not jump: the loop takes
    /// effect once the playhead is inside it.
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`] when the timed-command list is full; the edit is handed
    /// back in it.
    pub fn set_loop(&self, range: LoopRange) -> Result<(), TimedError<LoopEdit>> {
        self.transport
            .motion()
            .schedule(At::NextBlock, LoopEdit::over(range))
    }

    /// Stops looping from the next block. The loop's bounds are kept, so
    /// [`set_loop`](Self::set_loop) with them (or arming through the transport)
    /// restores it ([`LoopEdit::armed`]).
    ///
    /// # Errors
    ///
    /// [`TimedError::Full`], as [`set_loop`](Self::set_loop).
    pub fn clear_loop(&self) -> Result<(), TimedError<LoopEdit>> {
        self.transport
            .motion()
            .schedule(At::NextBlock, LoopEdit::armed(false))
    }

    /// Sets a constant project tempo (the project's [`TempoMap`], one step).
    /// The engine takes it at the next block. A tempo that is not finite and
    /// positive leaves the map as it was.
    ///
    /// # Errors
    ///
    /// [`TempoError::Following`] while the session follows a live tempo:
    /// nothing is published.
    pub fn set_tempo(&self, bpm: Bpm) -> std::result::Result<(), TempoError> {
        self.tempo_is_ours()?;
        self.transport.settings().set_tempo(bpm);
        Ok(())
    }

    /// Publishes `map` as the project's tempo (steps and ramps): the engine
    /// takes it at the next block. A map by value works: `map.into()`.
    ///
    /// # Errors
    ///
    /// [`TempoError::Following`] while the session follows a live tempo:
    /// nothing is published.
    pub fn set_tempo_map(
        &self,
        map: impl Into<Arc<TempoMap>>,
    ) -> std::result::Result<(), TempoError> {
        self.tempo_is_ours()?;
        self.transport.settings().set_tempo_map(map);
        Ok(())
    }

    /// Refuses a tempo the app sets while a live tempo owns the map.
    fn tempo_is_ours(&self) -> std::result::Result<(), TempoError> {
        #[cfg(feature = "midi")]
        if self.midi_state.is_following() {
            return Err(TempoError::Following);
        }
        Ok(())
    }

    /// The per-channel compensation of the plan sent last, by output channel:
    /// what the compiler delays each output by, for a host that displays it.
    /// Republished with every plan; empty until the first. A node in the graph
    /// (a disk voice included) is aligned by the compiler, not by this table.
    ///
    /// Shared, not copied: a clone of the `Arc` reads every later plan's table. Read a channel with
    /// [`PrerollTable::preroll`], `None` past the table's end. No audio
    /// thread reads it, so it is a plain lock, not a published cell.
    pub fn compensation(&self) -> &Arc<PrerollTable> {
        &self.compensation
    }

    /// The total latency of the plan sent last: what a DAW displays.
    pub fn latency(&self) -> Samples {
        self.latency
    }

    /// The metronome's node (unwired: the host declares where it lands),
    /// when the builder inserted one.
    pub fn click_node(&self) -> Option<NodeKey> {
        self.click
    }

    /// The metronome's settings.
    pub fn click_settings(&self) -> &Arc<ClickSettings> {
        &self.click_settings
    }

    /// Opens the tap on **what the engine renders**: every output channel at
    /// the device's width, after the declick fader. It is the engine's f32
    /// output buffer, not the device's bytes: the device layer clamps a long
    /// callback to its `MAX_FRAMES` and converts to the device's sample type
    /// after the engine has written the tap. It is read through the same
    /// [`TapReader`](tutti_graph::TapReader) as a plan tap, and is the default way to record what the
    /// output sounds like. A transport stop fades in it as it does in the
    /// engine's output, so a take has no click there; [`Taps::open_output`](crate::Taps::open_output) is the
    /// pre-fader signal of the graph, aligned by the compiler, and steps at a
    /// stop for a transport-gated source.
    ///
    /// It is written by the engine, on every path that renders through it: a
    /// device callback, [`render`](Self::render), and a block the engine
    /// refuses (silence, whole frames, so a take keeps its timing). An export
    /// forks the graph and renders without the engine, so it never writes to
    /// it. There is one at a time; its ring holds `capacity` frames and what
    /// the reader does not drain in time is dropped and counted
    /// ([`TapReader::take_lost`](tutti_graph::TapReader::take_lost)). It flows from the first
    /// render that begins after this returns, and is closed by
    /// [`close_heard_tap`](Self::close_heard_tap) or, once its reader is
    /// dropped, by the next [`Reclaim`](Phase::Reclaim) phase. The width is
    /// the device's when the session was built: a restart onto a device of
    /// another width counts the tap's blocks lost until it is opened again.
    ///
    /// # Errors
    ///
    /// [`HeardTapError::Detached`] on a session without an engine,
    /// and [`HeardTapError::Tap`]`(Busy)` while another reader is alive. While the
    /// audio thread has not taken the last installs it is not an error: the
    /// call returns [`HeardOpen::Retry`] and opens nothing.
    pub fn open_heard_tap(&self, capacity: Samples) -> Result<HeardOpen, HeardTapError> {
        let (taps, layout) = self.heard.as_ref().ok_or(HeardTapError::Detached)?;
        Ok(taps.open(*layout, capacity)?)
    }

    /// Closes the tap [`open_heard_tap`](Self::open_heard_tap) opened, now
    /// rather than at the [`Reclaim`](Phase::Reclaim) phase that follows a
    /// dropped reader. The reader keeps what it was written.
    ///
    /// # Errors
    ///
    /// [`HeardTapError::Detached`] on a session without an engine;
    /// [`HeardTapError::Tap`]`(NotOpen)` when no heard tap is open (never opened, or
    /// closed already); nothing changes.
    pub fn close_heard_tap(&self) -> Result<(), HeardTapError> {
        let (taps, _) = self.heard.as_ref().ok_or(HeardTapError::Detached)?;
        Ok(taps.close()?)
    }

    /// The master meter: the level of each graph output channel, one
    /// [`Level`](tutti_graph::Level) per channel, with the peak held since
    /// the last read. It is an op of the plan on the outputs (see
    /// [`Meters::master`](crate::Meters::master)), declared and on
    /// from the build, so every render feeds it: a device callback,
    /// [`render`](Self::render), and a detached session's
    /// `render_frame`. It reads before the engine's fold
    /// to the device and its declick, aligned to the outputs' time. A block
    /// the engine refuses (rendered silent because the control side held the
    /// graph for that callback) runs no op, so it does not touch the meter:
    /// the meter holds what it held and counts no frames for it. `None` once
    /// a host has taken it.
    pub fn master_meter(&mut self) -> Option<&mut tutti_graph::MeterReader> {
        self.graph.meters().master()
    }

    /// Claims the reader reopened under `id`
    /// ([`SessionEvent::RootReaderReopened`]): held until then.
    ///
    /// # Errors
    ///
    /// [`NotReopened`](crate::NotReopened) when none is held under `id`.
    pub fn take_reopened(
        &mut self,
        id: crate::RootReader,
    ) -> Result<crate::Reopened, crate::NotReopened> {
        self.graph.take_reopened(id)
    }

    /// Hands the master meter's reader to its one consumer; see
    /// [`master_meter`](Self::master_meter) and
    /// [`Meters::take_master`](crate::Meters::take_master).
    pub fn take_master_meter(&mut self) -> Option<tutti_graph::MeterReader> {
        self.graph.meters().take_master()
    }

    /// The engine's DSP-load meter: each block's render time against its
    /// real-time budget, the smoothed mean, the peak since the last
    /// [`take`](tutti_metering::LoadMeter::take) and the blocks over budget.
    /// Fed by every block the device callback (or, offline,
    /// [`render`](Self::render)) renders; `None` on a detached session,
    /// which has no engine.
    pub fn load_meter(&self) -> Option<&tutti_metering::LoadMeter> {
        self.load.as_ref()
    }

    /// The same meter, mutably: [`take`](tutti_metering::LoadMeter::take)
    /// reads the peak since the last take, so it is the one reader's (the
    /// session holds it; there is no second).
    pub fn load_meter_mut(&mut self) -> Option<&mut tutti_metering::LoadMeter> {
        self.load.as_mut()
    }

    /// The engine's MIDI nodes, when the builder inserted them.
    #[cfg(feature = "midi")]
    pub fn midi_nodes(&self) -> Option<&MidiNodes> {
        self.midi.as_ref()
    }
}
