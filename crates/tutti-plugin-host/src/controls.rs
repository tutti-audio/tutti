//! [`PluginControls`] — the part of a `tutti-plugin`'s `PluginClient` a
//! host drives from outside the graph.
//!
//! # Why a separate type
//!
//! A plugin node is owned by the graph once inserted: it exists exactly once,
//! and nothing downcasts to find it. Everything a host does to it afterwards —
//! give it the project meter, read the latency it reports — goes through state
//! shared with the node: the meter, latency and tail are shared cells. (What
//! it plays — MIDI, parameter automation, chords and scales — reaches it on
//! its event input, from nodes of their own.) This type is those shared cells
//! and nothing else, and it is what
//! inserting a bound plugin hands back
//! ([`IntoNode::Controls`](tutti_graph::IntoNode::Controls)), so the type
//! system gives the host its control surface at insert.
//!
//! # The transport is not here
//!
//! The node reads the transport from each block's `Env`
//! (`transport_source`), so there is nothing to install: only the meter,
//! which is a layer over the timeline and not transport state, is a control.
//!
//! # The sample rate is shared
//!
//! A handle taken before insertion is not the node: its copy of a per-node
//! field would read the rate from load time forever. So the rate lives in a
//! shared cell like the others, and the node's `prepare` updates it for every
//! holder.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tutti_rt::LockExt;

use tutti_plugin_types::{ChunkCounters, PluginInvalidation, PluginTail};
use tutti_rt::AtomicSampleRate;
use tutti_timeline::ProjectMeter;
use tutti_units::Latency;
use tutti_units::{SampleRate, Samples};

use tutti_plugin_types::MAX_CHUNK;

/// A plugin node's host-side controls: its input slots, its meter, latency
/// and tail cells, and its sample rate — every one shared with the node it
/// came from.
///
/// Handed back by inserting a bound plugin into a graph
/// ([`IntoNode`](tutti_graph::IntoNode)), or taken earlier with
/// `PluginClient::controls`. A clone is
/// another handle on the same cells, never a copy of them, so an install
/// through any handle is seen by the node the audio thread runs, lock-free
/// and with no graph commit.
///
/// Holds no audio state and no subprocess lifetime: dropping every handle
/// leaves the plugin running, and a handle outliving its plugin installs into
/// slots nothing drains.
#[derive(Clone)]
pub struct PluginControls {
    /// The plugin's own reported latency, written by the bridge thread when the
    /// plugin signals a change. A `usize` because an atomic needs a primitive;
    /// [`latency`](Self::latency) puts the unit back.
    latency: Arc<AtomicUsize>,
    /// Runtime tail. A `Mutex` rather than an atomic because [`PluginTail`]
    /// is a four-arm sum whose payload is a `usize` — "unbounded" and "never
    /// asked" are not numbers, so there is no integer encoding to
    /// compare-and-swap that does not reintroduce the sentinel the type exists
    /// to avoid. Nothing on the audio thread reads it: the bridge thread and a
    /// host write it, and the node's `shape` (the compiler's, on the control
    /// side) and an offline render's sizing read it.
    tail: Arc<Mutex<PluginTail>>,
    /// The frames the node's IPC pipeline holds: one chunk, settled by the
    /// node's `prepare` (`Batcher::prepare`). Shared so
    /// [`declared_latency`](Self::declared_latency) is right on any handle.
    pipeline: Arc<AtomicUsize>,
    /// The meter cell and the delivery counters, the part every plugin node's
    /// controls share ([`NodeControls`], which this derefs to).
    node: NodeControls,
    /// The rate stamped onto a freshly-installed source. Shared — see the module
    /// docs for why it could not stay a per-clone field. `f64` at the atomic.
    sample_rate: Arc<AtomicSampleRate>,
    /// What the node's IPC pipeline reports
    /// ([`missed_chunks`](Self::missed_chunks),
    /// [`chunk_in_flight_answered`](Self::chunk_in_flight_answered)).
    chunks: Arc<ChunkCounters>,
}

/// What every plugin node's controls hold, whichever hosting runs it: the
/// node's meter cell and its delivery counters. The in-process node's
/// controls are this;
/// the out-of-process node's [`PluginControls`] hold one and deref to it.
/// A clone is another handle on the same cells.
#[derive(Clone, Debug)]
pub struct NodeControls {
    /// The project meter, for the time-signature and bar fields of the
    /// transport the node sends. Empty until a host installs one; the node
    /// then reads 4/4 from bar 0. A nullable hot-swap slot: the node reads it
    /// once per block.
    meter: crate::PluginTimeSignature,
    /// What the node's parameter and MIDI delivery has done.
    delivery: Arc<crate::DeliveryCounters>,
}

impl NodeControls {
    /// Handles on `meter` and `delivery`, the cells the node reads and writes.
    pub(crate) fn new(
        meter: crate::PluginTimeSignature,
        delivery: Arc<crate::DeliveryCounters>,
    ) -> Self {
        Self { meter, delivery }
    }

    /// What the node has done with the parameter changes on its event input
    /// since the plugin was loaded: points sent (changes only), chunks whose
    /// changes went a chunk late (in process, a block the instance was
    /// locked for), and points dropped for a parameter it has no slot for. The node has a slot for every parameter the plugin
    /// listed at load (and some spare), so the last stays zero unless the
    /// plugin is sent addresses it does not have.
    ///
    /// The node sends a parameter only when the host's value differs from
    /// the last one it sent: an unchanged value is never re-sent, so it
    /// cannot overwrite an edit made in the plugin's own editor; a value
    /// that moves is sent and replaces it. Points merged count the points a
    /// full queue replaced (see [`ParamDeliveryStats::points_merged`]).
    /// RT-safe: four relaxed loads.
    ///
    /// [`ParamDeliveryStats::points_merged`]: crate::ParamDeliveryStats::points_merged
    pub fn param_delivery(&self) -> crate::ParamDeliveryStats {
        self.delivery.snapshot()
    }

    /// MIDI events on the node's event input that never reached the plugin,
    /// since it was bound. Counted, in either hosting: an event past the
    /// [`MIDI_STACK_CAPACITY`](tutti_plugin_types::MIDI_STACK_CAPACITY) a chunk
    /// carries, and the rest of every SysEx message that lost a packet, so a
    /// message never reaches the plugin with a gap. In process (VST2), every
    /// event of a block the plugin did not run (the instance was locked).
    /// Out of process, every event of a chunk that was not sent (its audio could
    /// not be staged, the bridge refused it because its queue was full or
    /// the server had crashed, the bridge thread skipped it as stale, or it
    /// was still queued when the server crashed); the events taken towards
    /// the next chunk when the node is reset or re-prepared; and the rest of
    /// every SysEx message that lost a packet in any of those ways or was
    /// open at a reset or re-prepare, so a message never reaches the plugin
    /// with a gap (the plugin's host counts it as one dropped SysEx). A
    /// chunk sent and then not answered in time is not counted here: the
    /// server still runs it. Nor is a chunk queued in the instant after the
    /// bridge thread drained its queue at a crash and exited: it was accepted,
    /// and nothing reads it again (and no message can splice past a dead
    /// server).
    /// Counted in the node's events (UMP packets). What the plugin's own host
    /// refuses after receiving them (a SysEx it drops whole, a message of a Group it
    /// cannot hear) is in its own unit and is not here.
    /// RT-safe: two relaxed loads.
    pub fn dropped_midi_in(&self) -> u64 {
        self.delivery.dropped_midi_in()
    }

    /// Transport changes inside a chunk that did not fit its inline list
    /// ([`MAX_TRANSPORT_CHANGES`](tutti_graph::MAX_TRANSPORT_CHANGES), the
    /// graph's own bound), since the plugin was bound: a map point, a seek, a
    /// loop wrap, a start or a stop past the eighth in one chunk. Each is
    /// counted once; its transport waits for the next chunk's first frame,
    /// and VST3, VST2 and AU, which have no transport
    /// event inside a block, never count one. RT-safe: one relaxed load.
    pub fn dropped_transport_changes(&self) -> u64 {
        self.delivery.dropped_transport_changes()
    }

    /// What the node did not render, or the plugin did not take, since it was
    /// bound: see [`BlockLosses`](crate::BlockLosses). RT-safe: relaxed loads.
    pub fn block_losses(&self) -> crate::BlockLosses {
        self.delivery.losses.blocks()
    }

    /// MIDI events the plugin sent that the node had no room to put on its
    /// event output, since it was bound. Only a chunked Processor's pieces
    /// are gathered before they leave, and so can overflow here; the
    /// executor counts what the output itself refuses. RT-safe: one relaxed
    /// load.
    pub fn midi_out_dropped(&self) -> u64 {
        self.delivery.losses.midi_out.get()
    }

    /// Chords and scales on the node's event input that the sequencer context
    /// had no room for, since it was bound: past
    /// [`HARMONY_CAPACITY`](tutti_plugin_types::HARMONY_CAPACITY) in one
    /// piece of a block. RT-safe: one relaxed load.
    pub fn harmony_dropped(&self) -> u64 {
        self.delivery.losses.harmony.get()
    }

    /// Give the node the project meter, for the signature and bar it tells
    /// the plugin. The node's meter follows it, so an edit the host
    /// publishes there reaches the running plugin with nothing re-installed.
    /// Replaces any meter installed before.
    pub fn set_meter(&self, meter: ProjectMeter) {
        self.meter.set_meter(meter);
    }

    /// Drop the meter; the node tells the plugin 4/4 from bar 0.
    pub fn clear_meter(&self) {
        self.meter.clear_meter();
    }

    /// Whether a meter is installed — what a host's binding checks.
    #[must_use]
    pub fn has_meter(&self) -> bool {
        self.meter.has_meter()
    }

    /// The node's meter cell, as a handle of its own: what a host keeps to
    /// install the meter through (the same cell as
    /// [`set_meter`](Self::set_meter)).
    pub fn meter(&self) -> crate::PluginTimeSignature {
        self.meter.clone()
    }
}

impl std::ops::Deref for PluginControls {
    type Target = NodeControls;
    fn deref(&self) -> &NodeControls {
        &self.node
    }
}

/// The write half of a plugin's [`PluginControls`]: what the plugin reported
/// goes into the latency and tail cells the controls read.
///
/// Issued once, with the controls, to the plugin's bridge listener, which is
/// the only writer of the figures the graph compensates for. The controls hold
/// reads only.
#[derive(Debug)]
pub struct PluginReports {
    latency: Arc<AtomicUsize>,
    tail: Arc<Mutex<PluginTail>>,
}

impl PluginReports {
    /// Takes what the plugin reported into the controls: a new latency reaches
    /// [`PluginControls::latency`] and [`PluginControls::declared_latency`], a
    /// new tail reaches [`PluginControls::tail`]. Any other invalidation changes
    /// no cell.
    pub fn take_report(&self, reported: &PluginInvalidation) {
        match *reported {
            // The atomic is the terminus: `AtomicUsize` needs a primitive, so the
            // unit type stops here.
            PluginInvalidation::Latency { samples } => {
                self.latency.store(samples.get(), Ordering::Release);
            }
            PluginInvalidation::Tail { tail } => *lock_tail(&self.tail) = tail,
            _ => {}
        }
    }
}

impl PluginControls {
    /// Public for `tutti-plugin`'s node of an out-of-process plugin, which builds its controls at load.
    ///
    /// Returns the controls with the [`PluginReports`] that write their latency and tail.
    pub fn new(
        latency: Samples,
        tail: PluginTail,
        sample_rate: SampleRate,
    ) -> (Self, PluginReports) {
        Self::with_chunks(latency, tail, sample_rate, Arc::default())
    }

    /// [`new`](Self::new), reporting the pipeline's chunk counters (the
    /// out-of-process hosting's own, `Ipc::chunk_counters`) rather than counters of
    /// its own.
    pub fn with_chunks(
        latency: Samples,
        tail: PluginTail,
        sample_rate: SampleRate,
        chunks: Arc<ChunkCounters>,
    ) -> (Self, PluginReports) {
        let latency = Arc::new(AtomicUsize::new(latency.get()));
        let tail = Arc::new(Mutex::new(tail));
        let reports = PluginReports {
            latency: Arc::clone(&latency),
            tail: Arc::clone(&tail),
        };
        let controls = Self {
            latency,
            tail,
            pipeline: Arc::new(AtomicUsize::new(MAX_CHUNK)),
            node: NodeControls::new(crate::PluginTimeSignature::default(), Arc::default()),
            sample_rate: Arc::new(AtomicSampleRate::new(sample_rate)),
            chunks,
        };
        (controls, reports)
    }

    /// The cells the node's IPC pipeline reports to, for the node.
    pub fn chunk_counters(&self) -> Arc<ChunkCounters> {
        Arc::clone(&self.chunks)
    }

    /// How many chunks the plugin-server had not answered by the time the
    /// node played them, since the plugin was bound: each played as
    /// silence (a dropout). A crashed plugin's silence is not counted. An
    /// offline fork waits for every chunk, so its count stays zero unless
    /// it gave up. RT-safe: one relaxed load.
    pub fn missed_chunks(&self) -> u64 {
        self.chunks.missed()
    }

    /// Of the [`missed_chunks`](Self::missed_chunks), how many the plugin-server
    /// published after the node had played the silence: a late answer, not a
    /// lost one. Telemetry: a late chunk means an overloaded plugin or machine
    /// (raise the device buffer size). It is seen in the shared ring or in the
    /// server's reply, so it can be counted a few chunks after it was missed.
    /// Every missed chunk ends as exactly one of late, [refused](Self::refused_chunks),
    /// [skipped](Self::skipped_chunks) or [unanswered](Self::unanswered_chunks).
    /// RT-safe: one relaxed load.
    pub fn late_chunks(&self) -> u64 {
        self.chunks.late()
    }

    /// Of the [`missed_chunks`](Self::missed_chunks), how many the plugin-server
    /// refused: it did not publish the chunk's audio, and said so in its reply.
    /// Its reasons are the chunk's input slot in the shared ring recycled
    /// before the server ran it, a block that did not fit, an output channel
    /// not written, and a refused sample rate. Telemetry beside
    /// [`late_chunks`](Self::late_chunks): an overloaded plugin or machine.
    /// RT-safe: one relaxed load.
    pub fn refused_chunks(&self) -> u64 {
        self.chunks.refused()
    }

    /// Of the [`missed_chunks`](Self::missed_chunks), how many are neither
    /// [late](Self::late_chunks), [refused](Self::refused_chunks) nor
    /// [skipped](Self::skipped_chunks): the server never said what became of
    /// them within the last few chunks. Not zero is a loss no counter explains:
    /// a server that drops chunks silently. RT-safe: one relaxed load.
    pub fn unanswered_chunks(&self) -> u64 {
        self.chunks.unanswered()
    }

    /// Chunks dropped because the plugin fell more than one chunk behind: the
    /// bridge skipped the chunk (the shared ring holds one chunk behind the
    /// newest) and never sent it, so it plays as silence and is never
    /// answered. Telemetry beside [`missed_chunks`](Self::missed_chunks): it
    /// means an overloaded plugin or machine, so raise the device buffer size.
    /// RT-safe: one relaxed load.
    pub fn skipped_chunks(&self) -> u64 {
        self.chunks.skipped()
    }

    /// Commands the audio side could not hand to the plugin's bridge: a full
    /// command queue refused a reset or rate command, or a block
    /// found no payload or no room for its parameter buffer, so the block was
    /// refused. The command is lost (a block plays as silence). Telemetry
    /// beside [`skipped_chunks`](Self::skipped_chunks): an overloaded plugin or
    /// machine. RT-safe: one relaxed load.
    pub fn refused_commands(&self) -> u64 {
        self.chunks.commands_refused()
    }

    /// Audio responses (a block's MIDI-out and status) the bridge could not
    /// queue for the audio thread because its queue was full. Lost.
    /// RT-safe: one relaxed load.
    pub fn lost_responses(&self) -> u64 {
        self.chunks.responses_lost()
    }

    /// Parameter-change buffers the audio thread leaked rather than free on
    /// its own thread: a broken sizing bound, apart from a full command queue
    /// ([`refused_commands`](Self::refused_commands)). RT-safe: one relaxed load.
    pub fn leaked_buffers(&self) -> u64 {
        self.chunks.leaked_buffers()
    }

    /// Transfers between the plugin and its editor instance that were refused:
    /// a parameter write the mirror's full queue dropped (the plugin still got
    /// it; only the open editor's display missed it), an edit the user made in
    /// the editor that the plugin did not take, and a state that could not be
    /// read or loaded either way (an editor opened at the plugin's defaults
    /// among them). RT-safe: one relaxed load.
    pub fn mirror_transfers_refused(&self) -> u64 {
        self.chunks.mirror_transfers_refused()
    }

    /// Requests the plugin's editor instance made that were dropped: every
    /// [`Request`](tutti_plugin_types::Request) but a transport request (a
    /// restart, an I/O change, a parameter rescan, a latency or tail change, a
    /// reload, a callback, a MIDI-CC or note-expression change, a timer or a
    /// file descriptor), which the plugin's audio instance cannot be told.
    /// RT-safe: one relaxed load.
    pub fn editor_requests_dropped(&self) -> u64 {
        self.chunks.editor_requests_dropped()
    }

    /// Whether the chunk the node submitted last, and has not played yet,
    /// has been answered: `true` with none in flight.
    ///
    /// A manual-render helper: for a host (or a test) driving the graph by
    /// hand, block by block, with no device clock to keep. It waits on this
    /// before the next block, so that block plays the answer rather than
    /// silence. A live host does not wait on it: the device's callback
    /// period is the server's deadline, and a late answer is a dropout,
    /// counted by [`missed_chunks`](Self::missed_chunks).
    pub fn chunk_in_flight_answered(&self) -> bool {
        self.chunks.in_flight_answered()
    }

    /// The counters the node's parameter delivery writes, for the node.
    pub fn delivery_counters(&self) -> Arc<crate::DeliveryCounters> {
        Arc::clone(&self.node.delivery)
    }

    /// What the node has delivered so far, per listed parameter: what a fork
    /// is seeded with. Control thread: allocates.
    pub fn delivered(&self) -> Vec<(tutti_units::ParamId, f64)> {
        self.node.delivery.delivered()
    }

    /// The plugin's reported latency, in **frames** — its own figure, without
    /// the chunk the IPC pipeline adds (see
    /// [`declared_latency`](Self::declared_latency)).
    ///
    /// RT-safe: one atomic load, no allocation.
    pub fn latency(&self) -> Samples {
        Samples::new(self.latency.load(Ordering::Acquire))
    }

    /// The node's whole processing latency, as its `Shape` declares it to
    /// PDC: the plugin's own figure **plus** the chunk its out-of-process
    /// pipeline holds (audio is submitted now and collected a chunk later).
    ///
    /// What a host hands `Editor::set_latency` when the plugin's figure
    /// moves, so the next commit re-plans PDC around it: one number from one
    /// place, whether the editor asks the node at insert or the host asks
    /// this handle later.
    pub fn declared_latency(&self) -> Latency {
        Latency::new(self.latency() + self.pipeline())
    }

    /// The chunk the node's pipeline holds.
    pub(super) fn pipeline(&self) -> Samples {
        Samples::new(self.pipeline.load(Ordering::Acquire))
    }

    /// Record the chunk the node's `prepare` settled on.
    pub fn set_pipeline(&self, frames: Samples) {
        self.pipeline.store(frames.get(), Ordering::Release);
    }

    /// What the plugin currently reports for its tail. Control side: takes a
    /// short lock.
    pub fn tail(&self) -> PluginTail {
        *lock_tail(&self.tail)
    }

    /// The sample rate a source installed now would be stamped with.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate.load(Ordering::Acquire)
    }

    /// Update the shared rate and re-stamp every installed source. Called from
    /// the node's `prepare`; a no-op on a slot with nothing installed (a
    /// later install reads the new rate).
    pub(crate) fn restamp(&self, sample_rate: SampleRate) {
        self.sample_rate.store(sample_rate, Ordering::Release);
    }

    /// What a fork takes of the live node's sources, read now (a fork's
    /// gather, on the control thread): its meter, the one control its
    /// transport snapshot reads besides its own graph's `Env` (which for an
    /// offline fork is the render's). Every other input (MIDI, automation,
    /// harmony) reaches a plugin on its event input, from nodes that fork
    /// themselves.
    pub fn sources_now(&self) -> ForkedSources {
        ForkedSources {
            meter: self.node.meter.installed(),
        }
    }
}

/// Lock a tail cell. A poisoned lock still holds a whole `PluginTail` (a
/// `Copy` value written in one assignment), so it is read through.
fn lock_tail(tail: &Mutex<PluginTail>) -> std::sync::MutexGuard<'_, PluginTail> {
    tail.lock_recover()
}

/// The live node's sources as a fork's gather read them
/// ([`PluginControls::sources_now`]), to install on the fork when it is
/// built, wherever that is: a source the host installs on the live node
/// afterwards does not reach the fork.
///
/// Public for `tutti-plugin`'s node of an out-of-process plugin, which drives the same piece.
#[derive(Debug)]
pub struct ForkedSources {
    meter: Option<ProjectMeter>,
}

impl ForkedSources {
    /// Give `fork` what was read.
    pub fn install(self, fork: &PluginControls) {
        if let Some(meter) = self.meter {
            fork.set_meter(meter);
        }
    }
}

impl std::fmt::Debug for PluginControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginControls")
            .field("latency", &self.latency())
            .field("pipeline", &self.pipeline())
            .field("meter", &self.has_meter())
            .field("tail", &self.tail())
            .field("sample_rate", &self.sample_rate())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_music::MeterMap;

    fn controls() -> PluginControls {
        PluginControls::new(Samples::new(0), PluginTail::default(), SampleRate::SR_44K1).0
    }

    /// A handle taken *before* a rate change reads the new rate — the
    /// property that lets a host take the handle at load and keep it.
    ///
    /// `node` stands for the node's own handle, which its `prepare` restamps;
    /// `held` for the host's copy, taken first.
    #[test]
    fn a_handle_taken_before_a_rate_change_reads_the_new_rate() {
        let node = controls();
        let held = node.clone();
        node.restamp(SampleRate::SR_48K);
        assert_eq!(held.sample_rate(), SampleRate::SR_48K);
    }

    /// A fork has the live node's meter as it was at the gather, the one
    /// control its transport snapshot reads besides its own graph's `Env`:
    /// a meter installed on the live node between the gather and the build
    /// does not reach it.
    #[test]
    fn a_fork_gets_the_live_meter() {
        let live = controls();
        let meter = ProjectMeter::new(MeterMap::default());
        live.set_meter(meter.clone());
        let (fork, _) =
            PluginControls::new(Samples::new(0), PluginTail::default(), SampleRate::SR_96K);
        let gathered = live.sources_now();
        live.set_meter(ProjectMeter::new(MeterMap::default()));
        gathered.install(&fork);
        let shared_meter = fork
            .node
            .meter
            .installed()
            .is_some_and(|m| m.ptr_eq(&meter));
        assert!(shared_meter, "the fork reads the live node's meter");
    }

    /// Latency and tail written through one handle are read through another.
    #[test]
    fn latency_and_tail_are_shared_between_handles() {
        let node = controls();
        let held = node.clone();
        node.latency.store(512, Ordering::Release);
        *lock_tail(&node.tail) = PluginTail::Unbounded;
        assert_eq!(held.latency(), Samples::new(512));
        assert_eq!(held.tail(), PluginTail::Unbounded);
    }

    /// The declared latency is the plugin's figure plus the pipeline's chunk,
    /// both read live off shared cells: a host holding a handle taken at load
    /// sees a latency change and a re-prepared chunk alike, which is what it
    /// hands `Editor::set_latency`.
    #[test]
    fn the_declared_latency_is_the_plugins_plus_the_pipelines() {
        let node = controls();
        let held = node.clone();
        assert_eq!(
            held.declared_latency(),
            Latency::new(Samples::new(MAX_CHUNK))
        );
        node.set_pipeline(Samples::new(64));
        node.latency.store(512, Ordering::Release);
        assert_eq!(held.declared_latency(), Latency::new(Samples::new(576)));
        node.set_pipeline(Samples::new(32));
        assert_eq!(held.declared_latency(), Latency::new(Samples::new(544)));
    }
}
