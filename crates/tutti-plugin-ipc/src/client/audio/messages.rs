//! In-process messages between the audio thread and the bridge thread.
//! Distinct from wire-format `AudioRequest`/`ControlRequest` and their replies in `protocol/`.

use super::ask::{Ask, Reply};
use crate::transport::control::{Announced, Announces};
use crate::{Answered, ControlReply, ControlRequest, SlabHandle, Unanswered};
use dpi::Size;
use std::time::Duration;
use tutti_plugin_types::{
    BlockStatus, PluginFailure, ResyncKind, ServerError, ServerLosses, StateError,
};
use tutti_plugin_types::{
    ChordChanges, MidiEventVec, NoteExpressionChanges, NoteExpressionIntChanges,
    NoteExpressionTextChanges, ParamEdit, ParameterChanges, PluginTail, Samples, ScaleChanges,
    TransportChanges, TransportInfo,
};
use tutti_plugin_types::{RateRefusal, RateRefusalCause};
use tutti_units::SampleRate;

/// Audio-thread bulk payload for one `Process` command. Heap-boxed and
/// recycled between calls to avoid RT allocation.
#[derive(Debug)]
pub(super) struct ProcessPayload {
    /// Which block this is. Monotonic per plugin instance, starting at 1, and
    /// the same number that indexes this block's slab ring slot — the host and
    /// the server address the shared region by it.
    pub seq: u64,
    pub num_samples: usize,
    pub midi_events: MidiEventVec,
    pub param_changes: ParameterChanges,
    pub note_expression: NoteExpressionChanges,
    /// VST3 sequencer-context inputs. Empty until a host produces them.
    pub chords: ChordChanges,
    pub scales: ScaleChanges,
    pub expr_texts: NoteExpressionTextChanges,
    pub expr_ints: NoteExpressionIntChanges,
    pub transport: TransportInfo,
    /// The transport changes inside the block (never at frame 0).
    pub transport_changes: TransportChanges,
}

impl ProcessPayload {
    pub(super) fn empty(sample_rate: SampleRate) -> Self {
        Self {
            seq: 0,
            num_samples: 0,
            midi_events: MidiEventVec::new(),
            param_changes: ParameterChanges::new(),
            note_expression: NoteExpressionChanges::new(),
            chords: ChordChanges::new(),
            scales: ScaleChanges::new(),
            expr_texts: NoteExpressionTextChanges::new(),
            expr_ints: NoteExpressionIntChanges::new(),
            transport: TransportInfo::new(sample_rate),
            transport_changes: TransportChanges::new(),
        }
    }
}

/// Audio thread → the audio lane's thread: what rides the block connection, in
/// the order the audio thread queued it.
pub(super) enum AudioCommand {
    Process(Box<ProcessPayload>),
    Reset,
    /// The parameter writes no block has carried, sent on this lane so they are
    /// in order with the blocks: it carries none itself. The lane's thread
    /// takes the writes from the slots when it dequeues this, so the taking and
    /// the sending are one step in the lane's order, and a block queued before
    /// it has already taken what was written before that block. Answered into
    /// `done` (`true` when the server applied them, or there were none).
    Params {
        done: Reply<bool>,
    },
    /// A caller's wait for everything queued before it to have been applied and
    /// heard: held behind the settings in this queue like them, answered into
    /// `done` once the control lane has been through two fences.
    Settle {
        done: Reply<()>,
    },
}

/// A setting the audio lane holds its blocks for. Requested by a caller into a slot, then written by the audio lane's
/// thread into a latest-value slot per kind ([`Channels::put_setting`](super::channels::Channels::put_setting)), never
/// queued, so a full control queue cannot lose one; taken first by the control
/// lane's thread, which answers `applied` once the server has applied it.
#[derive(Clone, Copy, Debug)]
pub(super) enum Setting {
    /// The rate the plugin runs at.
    SampleRate(SampleRate),
    /// Format-neutral render mode, encoded onto each format's own ABI at the
    /// FFI edge (VST3 `ProcessSetup::processMode`, CLAP `clap.render`, AU
    /// `kAudioUnitProperty_OfflineRender`, VST2
    /// `audioMasterGetCurrentProcessLevel`). The server answers it with
    /// nothing, so the lane's own fence after it is the answer.
    RenderMode(tutti_plugin_types::RenderMode),
    /// A round trip that proves the server has handled every request sent
    /// before it on this lane.
    Fence,
}

/// Callers and the audio thread → the control lane's thread: one request of the
/// message table, and what an answered one waits on.
///
/// A tell is the request alone, built without an allocation, so the audio thread
/// can queue one. An answered request carries its [`Call`], boxed so the slot
/// stays the request's size: the lane sends it, waits for the one reply and
/// hands it to the call. There is no shared response queue: a reply cannot be
/// misrouted between requests.
pub(super) struct ControlCommand {
    request: ControlRequest,
    call: Option<Box<Call>>,
}

/// What the lane does for an answered request besides sending it.
pub(super) struct Call {
    /// The audio lane's answer to the flush of the host's writes, when the asker
    /// did not wait for it: the control lane waits for it (up to the parameter
    /// wait) before it sends, so the answer holds the writes made before.
    pub(super) flush: Option<Ask<bool>>,
    /// The request's wait.
    pub(super) deadline: Deadline,
    /// What travels with the request or comes with its reply.
    pub(super) object: Object,
    /// Given what the lane heard, on the lane's thread. Dropped unanswered when
    /// the lane ends, which cancels what waits on it.
    pub(super) done: Box<dyn FnOnce(Outcome) + Send>,
}

/// How long the server has to answer a request, and how long a caller waits.
#[derive(Clone, Copy, Debug)]
pub(super) enum Deadline {
    /// A question the plugin answers from memory ([`Timeouts::param`]).
    ///
    /// [`Timeouts::param`]: tutti_plugin_types::Timeouts::param
    Query,
    /// A state's transfer ([`Timeouts::state`]), renewed by the server's
    /// `Working` up to its ceiling.
    ///
    /// [`Timeouts::state`]: tutti_plugin_types::Timeouts::state
    State,
    /// The editor's window ([`Timeouts::editor`]).
    ///
    /// [`Timeouts::editor`]: tutti_plugin_types::Timeouts::editor
    Editor,
}

impl Deadline {
    /// The window the server has to answer, and the ceiling it may be renewed
    /// to.
    pub(super) fn window(self, timeouts: &tutti_plugin_types::Timeouts) -> (Duration, Duration) {
        match self {
            Self::Query => (timeouts.param, timeouts.param),
            Self::State => (timeouts.state, timeouts.state_ceiling()),
            Self::Editor => (timeouts.editor, timeouts.editor),
        }
    }

    /// How long a caller waits for the lane's answer: the window and a margin,
    /// so the lane's own report of a stall is the one that arrives.
    pub(super) fn wait(self, timeouts: &tutti_plugin_types::Timeouts) -> Duration {
        match self {
            Self::Query => timeouts.param_wait(),
            Self::State => timeouts.state_wait(),
            Self::Editor => timeouts.editor_wait(),
        }
    }
}

/// An object that goes with a request, or comes with its reply.
pub(super) enum Object {
    /// None: the request is one frame.
    None,
    /// A state's blob, sent after the request that announces it.
    Blob(tutti_plugin_ipc_ffi::Blob),
    /// A slab, sent after the request that announces it.
    Slab(SlabHandle),
    /// A state's blob the server sends after its reply, read on the lane.
    State,
}

impl Object {
    /// The request of `announced` and the object it announces, queued together:
    /// the pair is the one the announcement made.
    pub(super) fn announced<M: Announces>(announced: Announced<M>) -> (M, Self)
    where
        M::Object: Into<Self>,
    {
        let (msg, object) = announced.into_parts();
        (msg, object.into())
    }
}

impl From<tutti_plugin_ipc_ffi::Blob> for Object {
    fn from(blob: tutti_plugin_ipc_ffi::Blob) -> Self {
        Self::Blob(blob)
    }
}

impl From<SlabHandle> for Object {
    fn from(slab: SlabHandle) -> Self {
        Self::Slab(slab)
    }
}

/// What the lane heard of an answered request: the reply, and the object that
/// followed it when it was an [`Object::State`].
pub(super) struct Received {
    pub(super) reply: ControlReply,
    pub(super) object: Vec<u8>,
}

/// Why an answered request has no reply to give.
#[derive(Debug)]
pub(crate) enum Lost {
    /// The queue was full, or an earlier request still owed its answer.
    Busy,
    /// The bridge is crashed, or ended with the request unanswered.
    Crashed,
    /// The server did not answer within its window.
    NoAnswer {
        /// The window it had.
        after: Duration,
    },
    /// The server refused with an error in place of the answer.
    Server(ServerError),
    /// The server answered with another kind of reply than the request's.
    Wrong,
    /// The object that followed the reply was refused.
    Object(StateError),
}

impl Lost {
    /// A reply that is not the request's: the server's refusal, or the wrong
    /// kind.
    pub(super) fn of(reply: ControlReply) -> Self {
        match reply {
            ControlReply::Error { error } => Self::Server(error),
            _ => Self::Wrong,
        }
    }
}

/// What the lane made of a request: its answer, or why there is none.
pub(super) type Outcome = Result<Received, Lost>;

impl ControlCommand {
    /// A request nothing answers.
    pub(super) fn tell(msg: impl Unanswered) -> Self {
        Self {
            request: msg.into(),
            call: None,
        }
    }

    /// A request answered into `done`.
    pub(super) fn ask(msg: impl Answered, call: Call) -> Self {
        Self {
            request: msg.into(),
            call: Some(Box::new(call)),
        }
    }

    /// The request, and what the lane does for it.
    pub(super) fn into_parts(self) -> (ControlRequest, Option<Box<Call>>) {
        (self.request, self.call)
    }

    /// Whether the command has no reply: its caller learns nothing of its fate
    /// but from the counts.
    pub(super) fn is_fire_and_forget(&self) -> bool {
        self.call.is_none()
    }

    /// Answers a request the queue refused with [`Lost::Busy`], so an awaiting
    /// caller is told it was not sent and not that the plugin died.
    pub(super) fn refuse(self) {
        if let Some(call) = self.call {
            (call.done)(Err(Lost::Busy));
        }
    }
}

/// Bridge-thread → audio-thread (RT response path).
///
/// `Process` is on the RT path so it stays on a dedicated lock-free
/// queue, not on a per-request `Reply` (audio thread can't block).
///
/// **This queue carries no evidence about audio.** The slab answers that itself,
/// with a per-slot sequence number the server publishes after the last sample.
/// So these are notifications, not permissions — the host reads audio on the
/// strength of the slab, and emits silence for an unpublished block even if a
/// reply for it arrives.
///
/// What remains is the plugin's MIDI-out. The wire adapter decodes it on the
/// bridge thread (off-RT, see `dispatch`), so the list moves through the queue
/// already built; the RT thread only drains it into caller storage.
// `AudioProcessed` holds an inline-256 `MidiEventVec` (~5 KB) vs the zero-size
// `Error`. Intentional: the list is inline so popping + dropping it on the
// RT audio thread never touches the heap (see `submit`). Boxing would defeat
// that by moving the free onto the RT thread.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(super) enum AudioResponse {
    AudioProcessed {
        /// The block this answers, as echoed by the server: its MIDI-out
        /// belongs with that block's audio (`take_replies`). The slab is what
        /// establishes the audio's validity.
        seq: u64,
        midi_out: MidiEventVec,
        /// What the plugin said of itself at the end of the block.
        status: BlockStatus,
    },
    /// A block failed. `seq` is `Some` when the failure is attributable to one
    /// request (the server replied `Error` to it) and `None` for a
    /// connection-level failure that ends every in-flight block. Either way the
    /// host needs no action: the server never published, so the sequence check
    /// fails and silence follows.
    Error { seq: Option<u64> },
}

/// Plugin-originated, unsolicited events observed on the control stream.
/// Delivered to a listener installed on `super::AudioBridge` via
/// `set_listener`; the bridge thread invokes it after draining replies.
#[derive(Debug, Clone)]
pub enum IpcEvent {
    /// The plugin's processing latency changed, in frames.
    LatencyChanged {
        /// The latency it reports now.
        samples: Samples,
    },
    /// The plugin reported a new tail length at runtime (CLAP only — see
    /// [`News::TailChanged`](crate::News::TailChanged)).
    TailChanged {
        /// The tail it reports now.
        tail: PluginTail,
    },
    /// The plugin edited a parameter itself (its editor, its own automation or
    /// a modulation it reports): one [`ParamEdit`], in the order the plugin
    /// made them.
    ParameterChanged {
        /// The edit: the parameter, its value now and the gesture it falls in.
        edit: ParamEdit,
    },
    /// The buses the plugin has now, which the host was not last told: sent
    /// before the [`Resync`](Self::Resync) of [`Io`](ResyncKind::Io) it goes
    /// with.
    IoLayout(tutti_plugin_types::Layout),
    /// The plugin asked the host to resync some aspect of its state at runtime
    /// (preset load, param-title change, IO change, full reload). Carries no
    /// payload — the host re-reads from the plugin in response.
    Resync(ResyncKind),
    /// The plugin re-declared its controller table: the host asks for the wiring
    /// again. Carries no payload.
    MidiMapChanged,
    /// The plugin asked to be processed again (CLAP's `request_process`, after
    /// it slept). Carries no payload: the host wakes the node it parked.
    ProcessRequested,
    /// The plugin asked the host's transport to do something, in the order it
    /// asked.
    Transport {
        /// What it asked.
        request: tutti_plugin_types::TransportRequest,
    },
    /// The plugin was rebuilt and did not take the state it was saved with: it is
    /// at its defaults. Follows the [`Resync`](Self::Resync) of
    /// [`Reloaded`](ResyncKind::Reloaded).
    StateNotRestored {
        /// Why the state was not restored.
        reason: String,
    },
    /// The plugin did not take the render mode it was asked for: it runs in the
    /// mode it had.
    RenderModeNotApplied {
        /// The mode that was asked for.
        mode: tutti_plugin_types::RenderMode,
    },
    /// A restart the host asked for did not happen: the plugin would not stop
    /// and runs as it was.
    WouldNotStop {
        /// Why the plugin would not stop.
        reason: String,
    },
    /// What the server's own work lost, cumulative.
    ServerLosses(ServerLosses),
    /// The plugin refused the rate it was asked to run at (`Some`: the rate in
    /// Hz and the plugin's reason), or took a rate after refusing one (`None`).
    RateRefusal(Option<RateRefusal>),
    /// The bridge died: the subprocess never connected, failed the handshake,
    /// or its stream dropped mid-session.
    ///
    /// Unlike every other variant, this one is not the *plugin* speaking — it
    /// is the bridge reporting that the plugin can no longer speak at all. It
    /// fires once, from the site that noticed, and is terminal: the engine
    /// offers no relaunch, so recovery means loading a fresh plugin.
    ///
    /// `cause` holds the `IpcError` that ended the stream behind an `Arc`
    /// (that type is not `Clone`, and the reason has to outlive the call that
    /// produced it), reached as the failure's `source`. A host that asks later
    /// gets the real reason instead of a placeholder.
    ///
    /// **May arrive with no listener.** Two of the three crash sites run before
    /// `set_listener`, so a subscriber is not guaranteed to see this. The
    /// authoritative answer is the latched cause behind
    /// `PluginHandle::status`; this event is how a host learns *promptly*, not
    /// how it learns *reliably*.
    Crashed {
        /// What the site that noticed saw.
        cause: PluginFailure,
    },
}

/// How many transport requests [`Unsolicited`] holds between two drains.
pub(super) const UNSOLICITED_TRANSPORT: usize = 64;

/// How many parameter events [`Unsolicited`] holds between two drains.
pub(super) const UNSOLICITED_PARAMS: usize = 128;

/// The unsolicited events read off the control stream while a command waited
/// for its reply, held until its lane's loop hands them to the listener.
///
/// A plain buffer, not a queue between threads: `recv_reply` fills it and
/// the loop drains it after each command, both on the lane's thread, so it is
/// owned by the loop and lent to the handler. The listener runs after the drain
/// rather than from `recv_reply` itself so a slow listener never delays the
/// reply a blocked caller is waiting on.
///
/// **State events cannot be lost.** A latency, a tail and each resync kind is
/// a latest-wins slot: a plugin reporting its latency twice during one
/// command is heard once, with the second figure, and no flood of anything
/// else can evict it (PDC would never hear of it). Only parameter events
/// queue, bounded and allocated once: past [`UNSOLICITED_PARAMS`] the oldest
/// is dropped, counted ([`take_dropped`](Self::take_dropped)), and a
/// `Resync(ParamValues)` is raised in its place, so a host that lost a
/// parameter value is told to re-read them all.
///
/// **Delivery order** ([`drain`](Self::drain)), per command:
///
/// 1. `Resync(Reloaded)`, then `Resync(Io)`, then `StateNotRestored`:
///    structural changes first. A
///    plugin that rebuilds itself or its buses and then reports parameters
///    reports them against the new table, so the host must rebuild before
///    it applies them.
/// 2. The parameter events, in arrival order.
/// 3. The latency, then the tail: values the host takes as they stand.
/// 4. `Resync(ParamTitles)`, then `Resync(ParamValues)`: a re-read
///    reads the plugin as it is now, so it must win over any queued value,
///    and a drop's `ParamValues` must follow the values that survived it.
/// 5. `ProcessRequested`, last: the node it wakes finds the host's view of
///    the plugin already current.
///
/// What is lost is the order of a parameter event *before* a structural
/// change in the same command: it is delivered after it, against the
/// rebuilt table. A `Reloaded` asks the host to re-read everything, which
/// covers it.
///
/// Filled one kind at a time ([`param`](Self::param),
/// [`latency`](Self::latency), [`tail`](Self::tail), [`resync`](Self::resync)),
/// so [`Crashed`](IpcEvent::Crashed), which the bridge sends from the site
/// that noticed, cannot come through here.
pub(super) struct Unsolicited {
    params: std::collections::VecDeque<ParamEdit>,
    latency: Option<Samples>,
    tail: Option<PluginTail>,
    rate: Option<Option<RateRefusal>>,
    /// What a late answer is applied to, when it is read.
    lifecycle: Option<super::lifecycle::Lifecycle>,
    reloaded: bool,
    /// Why the last rebuild's state was not restored, until drained; a later
    /// one replaces it.
    state_not_restored: Option<String>,
    /// The mode the plugin last did not take, until drained; a later one
    /// replaces it.
    render_mode_not_applied: Option<tutti_plugin_types::RenderMode>,
    /// Why the plugin last would not stop, until drained; a later one replaces
    /// it.
    would_not_stop: Option<String>,
    /// The server's latest cumulative losses, until drained.
    server_losses: Option<ServerLosses>,
    io: bool,
    /// The buses the last `PluginIoChanged` carried, until drained.
    io_layout: Option<tutti_plugin_types::Layout>,
    param_titles: bool,
    param_values: bool,
    midi_map: bool,
    process_requested: bool,
    /// Transport requests, oldest first. Every one counts, so none is merged:
    /// past [`UNSOLICITED_TRANSPORT`] the oldest is dropped.
    transport: std::collections::VecDeque<tutti_plugin_types::TransportRequest>,
    /// Events dropped (a parameter edit past the queue, a layout no plugin
    /// has) since the last [`take_dropped`](Self::take_dropped).
    dropped: u64,
}

impl Unsolicited {
    pub(super) fn new() -> Self {
        Self {
            params: std::collections::VecDeque::with_capacity(UNSOLICITED_PARAMS),
            latency: None,
            tail: None,
            rate: None,
            lifecycle: None,
            reloaded: false,
            state_not_restored: None,
            render_mode_not_applied: None,
            would_not_stop: None,
            server_losses: None,
            io: false,
            io_layout: None,
            param_titles: false,
            param_values: false,
            midi_map: false,
            process_requested: false,
            transport: std::collections::VecDeque::with_capacity(UNSOLICITED_TRANSPORT),
            dropped: 0,
        }
    }

    /// Queue a parameter event, dropping (and counting) the oldest when full.
    pub(super) fn param(&mut self, edit: ParamEdit) {
        if self.params.len() == UNSOLICITED_PARAMS {
            self.params.pop_front();
            self.dropped += 1;
            self.resync(ResyncKind::ParamValues);
        }
        self.params.push_back(edit);
    }

    /// The plugin's new latency, replacing one already held.
    pub(super) fn latency(&mut self, samples: Samples) {
        self.latency = Some(samples);
    }

    /// The plugin's new tail, replacing one already held.
    pub(super) fn tail(&mut self, tail: PluginTail) {
        self.tail = Some(tail);
    }

    /// An inbox whose late rate answers are applied to `lifecycle`.
    pub(super) fn with_lifecycle(lifecycle: super::lifecycle::Lifecycle) -> Self {
        Self {
            lifecycle: Some(lifecycle),
            ..Self::new()
        }
    }

    /// Apply the answer to a rate change the bridge stopped waiting for,
    /// **now**: it is read in stream order, ahead of whatever is read after it,
    /// so a later answer (the request being waited on) lands after it and wins.
    pub(super) fn late_rate_answer(&mut self, rate: SampleRate, refusal: Option<RateRefusalCause>) {
        if let Some(lifecycle) = self.lifecycle.clone() {
            super::dispatch::apply_rate_reply(&lifecycle, self, rate, refusal);
        }
    }

    /// The server's editor asked its window for `size`.
    pub(super) fn editor_wants_size(&mut self, size: Size) {
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.set_editor_wants(size);
        }
    }

    /// The server closed its editor on its own account.
    pub(super) fn editor_closed(&mut self) {
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.set_editor_closed();
        }
    }

    /// The plugin asked to be processed again; a second request adds nothing.
    pub(super) fn process_requested(&mut self) {
        self.process_requested = true;
    }

    /// Queue a transport request, dropping the oldest when full.
    pub(super) fn transport(&mut self, request: tutti_plugin_types::TransportRequest) {
        if self.transport.len() == UNSOLICITED_TRANSPORT {
            self.transport.pop_front();
        }
        self.transport.push_back(request);
    }

    /// A rate refusal, or its clearing, replacing one already held.
    pub(super) fn rate_refusal(&mut self, refusal: Option<RateRefusal>) {
        self.rate = Some(refusal);
    }

    /// The buses a `PluginIoChanged` carried; a later one replaces an earlier
    /// one not yet drained. A layout no plugin has
    /// ([`is_plausible`](tutti_plugin_types::Layout::is_plausible)) is dropped
    /// and counted with the dropped events.
    pub(super) fn io_layout(&mut self, layout: tutti_plugin_types::Layout) {
        if layout.is_plausible() {
            self.io_layout = Some(layout);
        } else {
            self.dropped += 1;
        }
    }

    /// A rebuilt plugin's refusal of its saved state. One already held is
    /// replaced and counted with the dropped events.
    pub(super) fn state_not_restored(&mut self, reason: String) {
        if self.state_not_restored.replace(reason).is_some() {
            self.dropped += 1;
        }
    }

    /// A render mode the plugin did not take. One already held is replaced and
    /// counted with the dropped events.
    pub(super) fn render_mode_not_applied(&mut self, mode: tutti_plugin_types::RenderMode) {
        if self.render_mode_not_applied.replace(mode).is_some() {
            self.dropped += 1;
        }
    }

    /// A restart the plugin would not make. One already held is replaced and
    /// counted with the dropped events.
    pub(super) fn would_not_stop(&mut self, reason: String) {
        if self.would_not_stop.replace(reason).is_some() {
            self.dropped += 1;
        }
    }

    /// The server's cumulative losses, replacing any held: a figure that is
    /// cumulative loses nothing by being replaced.
    pub(super) fn server_losses(&mut self, losses: ServerLosses) {
        self.server_losses = Some(losses);
    }

    /// The plugin re-declared its controller table; a second adds nothing.
    pub(super) fn midi_map_changed(&mut self) {
        self.midi_map = true;
    }

    /// A resync request; a second of the same kind adds nothing.
    pub(super) fn resync(&mut self, kind: ResyncKind) {
        *match kind {
            ResyncKind::Reloaded => &mut self.reloaded,
            ResyncKind::Io => &mut self.io,
            ResyncKind::ParamTitles => &mut self.param_titles,
            ResyncKind::ParamValues => &mut self.param_values,
        } = true;
    }

    /// The held events, leaving the buffer empty, in the order the type
    /// documents: structural resyncs, parameters, latency, tail, refresh
    /// resyncs.
    pub(super) fn drain(&mut self) -> impl Iterator<Item = IpcEvent> + '_ {
        let raised = |flag: &mut bool, kind| std::mem::take(flag).then_some(IpcEvent::Resync(kind));
        let layout = self.io_layout.take().map(IpcEvent::IoLayout);
        let structural = [
            raised(&mut self.reloaded, ResyncKind::Reloaded),
            raised(&mut self.io, ResyncKind::Io),
        ];
        let refresh = [
            raised(&mut self.param_titles, ResyncKind::ParamTitles),
            raised(&mut self.param_values, ResyncKind::ParamValues),
        ];
        let params = self
            .params
            .drain(..)
            .map(|edit| IpcEvent::ParameterChanged { edit });
        let latency = self
            .latency
            .take()
            .map(|samples| IpcEvent::LatencyChanged { samples });
        let tail = self.tail.take().map(|tail| IpcEvent::TailChanged { tail });
        let transport = self
            .transport
            .drain(..)
            .map(|request| IpcEvent::Transport { request });
        let rate = self.rate.take().map(IpcEvent::RateRefusal);
        let process =
            std::mem::take(&mut self.process_requested).then_some(IpcEvent::ProcessRequested);
        let midi_map = std::mem::take(&mut self.midi_map).then_some(IpcEvent::MidiMapChanged);
        let unrestored = self
            .state_not_restored
            .take()
            .map(|reason| IpcEvent::StateNotRestored { reason });
        let mode_refused = self
            .render_mode_not_applied
            .take()
            .map(|mode| IpcEvent::RenderModeNotApplied { mode });
        let stuck = self
            .would_not_stop
            .take()
            .map(|reason| IpcEvent::WouldNotStop { reason });
        let losses = self.server_losses.take().map(IpcEvent::ServerLosses);
        layout
            .into_iter()
            .chain(structural.into_iter().flatten())
            .chain(unrestored)
            .chain(mode_refused)
            .chain(stuck)
            .chain(losses)
            .chain(params)
            .chain(latency)
            .chain(tail)
            .chain(transport)
            .chain(rate)
            .chain(refresh.into_iter().flatten())
            .chain(midi_map)
            .chain(process)
    }

    /// How many parameter events were dropped since the last call.
    pub(super) fn take_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_units::{Normalized, ParamId};

    /// **A refusal of the saved state replaced before it was drained is
    /// counted**, and the latest reason is the one delivered.
    #[test]
    fn a_replaced_state_refusal_is_counted_and_the_latest_is_delivered() {
        let mut inbox = Unsolicited::new();
        inbox.state_not_restored("first".into());
        assert_eq!(inbox.take_dropped(), 0);
        inbox.state_not_restored("second".into());
        assert_eq!(inbox.take_dropped(), 1);
        let heard: Vec<_> = inbox.drain().collect();
        assert!(
            matches!(heard.as_slice(), [IpcEvent::StateNotRestored { reason }] if reason == "second"),
            "{heard:?}"
        );
    }

    /// **A queued command is one request and a pointer**, the slot size the audio
    /// thread's push path was built for: an answered request's state travels
    /// boxed, and a tell is the request alone.
    #[test]
    fn a_queued_command_is_a_request_and_a_pointer() {
        assert_eq!(
            std::mem::size_of::<ControlCommand>(),
            std::mem::size_of::<ControlRequest>() + std::mem::size_of::<usize>()
        );
        assert!(std::mem::size_of::<ControlCommand>() <= 96);
    }

    /// **A request the queue refused is answered `Busy`, not left to read as a
    /// dead plugin**: the answer is filled, where a dropped reply would cancel it.
    #[test]
    fn a_refused_request_answers_busy() {
        let (answer, reply) = super::super::ask::awaited();
        ControlCommand::ask(
            crate::GetParameterList,
            Call {
                flush: None,
                deadline: Deadline::Query,
                object: Object::None,
                done: Box::new(move |outcome| reply.send(matches!(outcome, Err(Lost::Busy)))),
            },
        )
        .refuse();
        assert_eq!(futures_lite::future::block_on(answer), Some(true));
    }

    /// Each drained event as a short label, for whole-sequence asserts.
    fn labels(inbox: &mut Unsolicited) -> Vec<String> {
        inbox
            .drain()
            .map(|ev| match ev {
                IpcEvent::ParameterChanged { edit } => format!("p{}", edit.id()),
                IpcEvent::LatencyChanged { samples } => format!("latency {}", samples.get()),
                IpcEvent::TailChanged { .. } => "tail".to_owned(),
                IpcEvent::ProcessRequested => "process".to_owned(),
                IpcEvent::Transport { request } => format!("transport {request:?}"),
                IpcEvent::Resync(kind) => format!("{kind:?}"),
                IpcEvent::IoLayout(_) => "io layout".to_owned(),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    /// **The buses a `PluginIoChanged` carried are heard before its resync,
    /// once, the latest of two.** A resync with no layout is heard with none.
    #[test]
    fn the_announced_layout_precedes_its_resync() {
        let layout = |channels| tutti_plugin_types::Layout {
            inputs: vec![tutti_plugin_types::PluginBus::main(
                tutti_buffers::ChannelLayout::from_count(channels),
            )],
            outputs: Vec::new(),
        };
        let mut inbox = Unsolicited::new();
        inbox.io_layout(layout(2));
        inbox.io_layout(layout(4));
        inbox.resync(ResyncKind::Io);
        let heard: Vec<IpcEvent> = inbox.drain().collect();
        assert!(
            matches!(
                heard.as_slice(),
                [IpcEvent::IoLayout(l), IpcEvent::Resync(ResyncKind::Io)]
                    if *l == layout(4)
            ),
            "{heard:?}"
        );
        inbox.resync(ResyncKind::Io);
        assert_eq!(labels(&mut inbox), ["Io"]);
    }

    /// **A layout no plugin has is dropped and counted, and does not replace
    /// the one held.**
    #[test]
    fn an_implausible_layout_is_dropped_and_counted() {
        use tutti_plugin_types::{Layout, MAX_LAYOUT_BUSES, PluginBus};
        let stereo = || PluginBus::main(tutti_buffers::ChannelLayout::STEREO);
        let good = Layout {
            inputs: vec![stereo()],
            outputs: vec![stereo()],
        };
        let mut inbox = Unsolicited::new();
        inbox.io_layout(good.clone());
        inbox.io_layout(Layout {
            inputs: vec![
                PluginBus::main(tutti_buffers::ChannelLayout::from_count(0));
                MAX_LAYOUT_BUSES + 1
            ],
            outputs: Vec::new(),
        });
        inbox.io_layout(Layout {
            inputs: vec![stereo(); 9],
            outputs: Vec::new(),
        });
        assert_eq!(inbox.take_dropped(), 2);
        let heard: Vec<IpcEvent> = inbox.drain().collect();
        assert!(
            matches!(heard.as_slice(), [IpcEvent::IoLayout(l)] if *l == good),
            "{heard:?}"
        );
    }

    /// **A request to be processed again is heard once, after everything the
    /// host re-reads for**, so the node it wakes finds the host's view of the
    /// plugin current.
    #[test]
    fn a_process_request_is_heard_once_and_last() {
        let mut inbox = Unsolicited::new();
        inbox.process_requested();
        inbox.resync(ResyncKind::ParamValues);
        inbox.latency(Samples::new(64));
        inbox.process_requested();
        assert_eq!(labels(&mut inbox), ["latency 64", "ParamValues", "process"]);
        assert!(labels(&mut inbox).is_empty());
    }

    /// **The inbox hands parameter events over in arrival order and
    /// empties.**
    #[test]
    fn the_inbox_keeps_arrival_order_and_empties() {
        let mut inbox = Unsolicited::new();
        for i in 0..3 {
            inbox.param(ParamEdit::change(
                ParamId::new(i as u32),
                Normalized::default(),
            ));
        }
        inbox.latency(Samples::new(64));
        assert_eq!(labels(&mut inbox), ["p0", "p1", "p2", "latency 64"]);
        assert!(labels(&mut inbox).is_empty());
        assert_eq!(inbox.take_dropped(), 0);
    }

    /// **A flood of parameter events cannot evict a state event.** Latency,
    /// tail and resync arrive first, then more parameter events than the
    /// inbox holds (a preset load inside one `LoadState`): every state event
    /// is still delivered, latest-wins, the oldest parameter events are the
    /// ones dropped, the drops are counted, a `ParamValues` resync stands in
    /// for them, and the inbox never reallocates.
    #[test]
    fn a_parameter_flood_drops_only_parameters() {
        let mut inbox = Unsolicited::new();
        let cap = inbox.params.capacity();
        inbox.latency(Samples::new(32));
        inbox.tail(PluginTail::default());
        inbox.resync(ResyncKind::Reloaded);
        inbox.latency(Samples::new(64));
        let n = UNSOLICITED_PARAMS as i32;
        for i in 0..n + 2 {
            inbox.param(ParamEdit::change(
                ParamId::new(i as u32),
                Normalized::default(),
            ));
        }
        assert_eq!(inbox.params.capacity(), cap, "the inbox reallocated");
        assert_eq!(inbox.take_dropped(), 2);
        assert_eq!(inbox.take_dropped(), 0, "taking the count resets it");

        let got = labels(&mut inbox);
        let (reloaded, rest) = got.split_at(1);
        let (params, state) = rest.split_at(UNSOLICITED_PARAMS);
        assert_eq!(reloaded, ["Reloaded"]);
        assert_eq!(params.first().map(String::as_str), Some("p2"));
        assert_eq!(params.last(), Some(&format!("p{}", n + 1)));
        assert_eq!(state, ["latency 64", "tail", "ParamValues"]);
    }

    /// **Structural resyncs come before the parameter events, refresh
    /// resyncs after them.** A plugin that reloads (or rebuilds its buses)
    /// and then reports parameters reports them against its new table, so
    /// the host must hear `Reloaded` and `Io` first; a `ParamValues` re-read
    /// must win over every queued value, so it comes last. Every kind is
    /// raised, in the reverse of the delivery order, around the parameters.
    #[test]
    fn structural_resyncs_lead_and_refreshes_trail() {
        let mut inbox = Unsolicited::new();
        inbox.resync(ResyncKind::ParamValues);
        inbox.resync(ResyncKind::ParamTitles);
        inbox.tail(PluginTail::default());
        inbox.latency(Samples::new(16));
        inbox.resync(ResyncKind::Io);
        inbox.resync(ResyncKind::Reloaded);
        inbox.param(ParamEdit::change(ParamId::new(0), Normalized::default()));
        inbox.param(ParamEdit::change(ParamId::new(1), Normalized::default()));
        inbox.resync(ResyncKind::Reloaded);
        assert_eq!(
            labels(&mut inbox),
            [
                "Reloaded",
                "Io",
                "p0",
                "p1",
                "latency 16",
                "tail",
                "ParamTitles",
                "ParamValues"
            ]
        );
        assert!(labels(&mut inbox).is_empty(), "every slot was taken");
    }
}
