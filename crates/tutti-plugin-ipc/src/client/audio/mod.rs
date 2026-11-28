//! RT-safe audio bridge between the host audio thread and the
//! plugin-server subprocess.
//!
//! Composition:
//! - [`Channels`] — the lock-free queues (the audio lane's blocks, the control
//!   lane's requests, audio responses) plus the shared newest-sequence and
//!   sample-rate cells. Unsolicited events never cross threads: the control
//!   lane's thread buffers them itself.
//!   Payload recycling lives in [`PayloadPool`]; the batcher owns the block
//!   sequence.
//! - [`Lifecycle`] — running/crashed flags.
//!
//! The bulk audio transport (the slab) is not the bridge's: the batcher owns the
//! one it writes and reads, and the bridge only asks the server to open a new one
//! ([`AudioBridge::setup_shared_memory`]).
//!
//! The bridge's threads live in [`IpcThread`] (the audio lane's, which owns the
//! control lane's); its `Drop` shuts them down. **A block never waits behind a
//! control request**: each lane has its own thread, queue and wake-up.

mod ask;
mod channels;
mod dispatch;
mod lifecycle;
mod messages;
mod payload_pool;
mod thread;

use crate::ControlStream;
use crate::ServerWatch;
use crate::SlabHandle;
use crate::SlabLayout;
use crate::transport::control::Announced;
use crate::{
    Answered, ArmPluginMidiLearn, CloseEditor, GetCurrentPreset, GetParameter, GetParameterList,
    GetParameterNormalized, GetParameterPlain, GetParameterText, GetParameterValueFromText,
    GetPresetList, GetWiring, HostEdit, LoadPreset, LoadState, OpenEditor, PresetLoaded, SaveState,
    SetAppMidiMap, SetAutomationState, SetEditorSize, SetSampleRate, SetTrackInfo,
    SetupSharedMemory, StateLoaded, StateReady, Unanswered,
};
use dpi::Size;
use tutti_platform::Executor;
use tutti_plugin_types::Answer;
use tutti_plugin_types::{
    BlockStatus, ContentScale, Delivered, EditWriter, EditorError, IpcResult, ParamWriteError,
    PluginFailure, PresetError, StateError, StateUse, edit_slots,
};
use tutti_plugin_types::{
    ChordChanges, MidiEventVec, NoteExpressionChanges, NoteExpressionIntChanges,
    NoteExpressionTextChanges, ParameterChanges, ParameterInfo, PluginPreset, PresetId,
    ScaleChanges, TransportChanges, TransportInfo,
};
use tutti_plugin_types::{RateRefusal, RateRefusalCause};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::SampleRate;

/// VST3 sequencer-context inputs for one process block, bundled to keep
/// `AudioBridge::submit`'s signature manageable. All default to empty.
#[derive(Debug, Default, Clone)]
pub(crate) struct HarmonyInputs {
    /// Chord changes in the block.
    pub chords: ChordChanges,
    /// Scale changes in the block.
    pub scales: ScaleChanges,
    /// Text note expressions in the block.
    pub expr_texts: NoteExpressionTextChanges,
    /// Integer note expressions in the block.
    pub expr_ints: NoteExpressionIntChanges,
}

/// What `AudioBridge::submit` did with a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Submitted {
    /// Nothing went: the bridge is crashed or its queue is full, and the
    /// caller still holds its parameter changes.
    Refused,
    /// The bridge accepted the block (its output is collectable later), and
    /// the caller's parameter changes stayed with the caller: no free buffer
    /// to swap them into.
    Block,
    /// The bridge accepted the block, and it carries the caller's parameter
    /// changes (trivially, when there were none).
    BlockAndParams,
}

impl Submitted {
    /// Whether the bridge accepted the block, never that its output is ready.
    pub(crate) fn accepted(self) -> bool {
        !matches!(self, Self::Refused)
    }

    /// Whether the block carries the caller's parameter changes.
    pub(crate) fn took_params(self) -> bool {
        matches!(self, Self::BlockAndParams)
    }
}

use ask::Ask;
#[cfg(test)]
pub(crate) use channels::COMMAND_QUEUE_SIZE;
use channels::Channels;
pub use channels::Replies;
pub(crate) use channels::response_ring;
pub(crate) use channels::{BATCH_OUTCOMES, BatchOutcome};
use lifecycle::Lifecycle;
use messages::{
    AudioCommand, AudioResponse, Call, ControlCommand, Deadline, Lost, Object, Outcome, Received,
    Setting,
};
pub use payload_pool::{PARAM_BUFFERS, PayloadPool};
use std::sync::Arc;
use std::time::Duration;
use tutti_plugin_ipc_ffi::Blob;
use tutti_plugin_types::Timeouts;
use tutti_rt::Mutex;

pub use messages::IpcEvent;
pub use thread::IpcThread;

/// Waits up to `wait` for the bridge's answer to a query. The callers pass a
/// [`Timeouts`] wait (`param_wait`, `settle_wait`), which is the bridge's own
/// deadline and a margin: the bridge replies into its one-shot `Ask` at its
/// deadline, so a wait that ended there would give up as the answer arrived.
fn await_reply<T>(
    ask: ask::Ask<T>,
    wait: Duration,
) -> std::result::Result<T, std::sync::mpsc::RecvTimeoutError> {
    ask.recv_timeout(wait)
}

/// Listener for plugin-originated unsolicited events. Invoked on either lane's
/// thread (plain OS threads, so it can be called concurrently) — blocking is
/// OK, but prefer posting to a channel and returning promptly.
pub(crate) type IpcListener = Arc<dyn Fn(IpcEvent) + Send + Sync>;

pub(super) type ListenerSlot = Arc<Mutex<Option<IpcListener>>>;

/// The audio side of a served plugin: the realtime-safe command and response
/// bus to the server, and the control-thread queries answered over it. Clone
/// is cheap (Arc-based).
#[derive(Clone)]
pub struct AudioBridge {
    channels: Channels,
    payloads: PayloadPool,
    lifecycle: Lifecycle,
    listener: ListenerSlot,
    /// The count of audio responses the response ring lost to being full.
    responses_lost: tutti_rt::Counter<u64>,
    /// The writer of the host's parameter slots, built from the plugin's
    /// declared parameters by the first write; its reader is the audio lane's
    /// ([`Channels::install_host_edits`]). Control side.
    host_edits: Arc<Mutex<Option<HostEdits>>>,
    /// The host's gesture steps toward the plugin, not yet on the control
    /// queue: an `End` is kept until the queue takes it, never dropped, and the
    /// order is the order posted.
    gesture_outbox: Arc<tutti_plugin_types::HostEdits>,
}

/// The host's parameter slots, as the control side writes them.
struct HostEdits {
    writer: EditWriter,
    /// A write was made since the last flush was asked for: a flush with none
    /// asks the audio lane for nothing.
    written: bool,
}

impl std::fmt::Debug for AudioBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioBridge")
            .field("crashed", &self.is_crashed())
            .finish_non_exhaustive()
    }
}

impl Replies {
    /// Drain every reply the bridge has queued into `midi_out` (appended):
    /// block `seq`'s MIDI-out at its own frame offsets, and any earlier
    /// block's (a reply that missed its own collection) at offset 0, since it
    /// is already late. Returns whether `seq`'s reply (processed or failed)
    /// was among them.
    ///
    /// These carry only the plugin's MIDI-out: whether the *audio* is there
    /// is settled by the slab's sequence numbers, not by a reply arriving.
    /// Draining rather than taking one: a reply for an abandoned block can
    /// still be sitting here, and leaving it would put the queue one behind
    /// forever. Lock-free; `midi_out` is inline and capped (a full one drops
    /// the rest).
    ///
    /// `status` is set to what the plugin said of itself at the end of block
    /// `seq` when its reply was among them, and left as it was otherwise: an
    /// earlier block's status is about an earlier block, and is dropped.
    pub(crate) fn take(
        &mut self,
        seq: u64,
        midi_out: &mut MidiEventVec,
        status: &mut BlockStatus,
    ) -> bool {
        let mut answered = false;
        while let Some(resp) = self.pop() {
            match resp {
                AudioResponse::AudioProcessed {
                    seq: s,
                    midi_out: events,
                    status: said,
                } => {
                    answered |= s == seq;
                    if s == seq {
                        *status = said;
                    }
                    for mut e in events {
                        if s != seq {
                            e.offset = 0;
                        }
                        if midi_out.try_push(e).is_err() {
                            break;
                        }
                    }
                }
                AudioResponse::Error { seq: s } => answered |= s.is_none_or(|s| s == seq),
            }
        }
        answered
    }
}

impl AudioBridge {
    /// Starts the bridge's lanes on `executor`, which speak for the queues on the
    /// server's `audio` lane and its `control` connection; dropping the
    /// [`IpcThread`] stops them. The bridge keeps no handle to `executor`.
    ///
    /// # Errors
    ///
    /// The bridge thread cannot be spawned.
    pub(crate) fn new(
        executor: &Executor,
        audio: ControlStream,
        control: ControlStream,
        sample_rate: SampleRate,
        watch: Arc<ServerWatch>,
        timeouts: Timeouts,
    ) -> IpcResult<(Self, IpcThread, Replies)> {
        // The rate lives on `Channels`, not here: the bridge thread sizes its
        // own reply timeout from the same block period, and two copies of the
        // rate would let the two timeouts drift apart.
        let (channels, waits) = Channels::new(sample_rate, Arc::clone(watch.wake()))?;
        // An end of the server declared by anyone (the process guard, the host)
        // wakes both lanes' threads, which each wait on their own wake-up and not on the
        // watch's gate.
        watch.on_declared(channels.waker());
        let payloads = PayloadPool::new(
            sample_rate,
            channels.refusal_writer(),
            channels.leak_writer(),
        );
        let lifecycle = Lifecycle::new(watch, timeouts);
        let listener: ListenerSlot = Arc::new(Mutex::new(None));
        let (responses, replies, responses_lost) = response_ring();
        let thread = IpcThread::spawn(
            executor,
            channels.clone(),
            responses,
            waits,
            payloads.clone(),
            lifecycle.clone(),
            Arc::clone(&listener),
            audio,
            control,
        )?;
        let bridge = Self {
            channels,
            payloads,
            lifecycle,
            listener,
            responses_lost,
            host_edits: Arc::default(),
            gesture_outbox: tutti_plugin_types::HostEdits::new(),
        };
        Ok((bridge, thread, replies))
    }

    /// Reserve the buffers a block's parameter changes travel in, each with
    /// room for `queues` parameter queues (the plugin's parameter count, and
    /// some) of `points` points each (the node's per-chunk budget). Control
    /// thread: allocates. See [`submit`](Self::submit) and
    /// `PayloadPool::reserve_params`.
    pub(crate) fn reserve_param_changes(&self, queues: usize, points: usize) {
        self.payloads.reserve_params(queues, points);
    }

    /// Install a listener for plugin-originated unsolicited events. Pass
    /// `None` to clear. The callback runs on a lane's thread after each
    /// dispatched command: the control lane's for the plugin's news, either's
    /// for a crash.
    pub fn set_listener(&self, listener: Option<IpcListener>) {
        *self.listener.lock() = listener;
    }

    /// Tells the listener of a parameter edit made in the editor instance the
    /// host holds (it is not the server's to report), on the calling thread.
    /// A listener must be cheap and never block, as for the bridge thread's.
    pub(super) fn report_edit(&self, edit: tutti_plugin_types::ParamEdit) {
        let listener = self.listener.lock().clone();
        if let Some(listener) = listener {
            listener(IpcEvent::ParameterChanged { edit });
        }
    }

    /// Tells the listener of a transport request made in the editor instance
    /// the host holds (it is not the server's to report), on the calling
    /// thread, as it tells one the server sent.
    pub(super) fn report_transport(&self, request: tutti_plugin_types::TransportRequest) {
        let listener = self.listener.lock().clone();
        if let Some(listener) = listener {
            listener(IpcEvent::Transport { request });
        }
    }

    /// Whether the server died or the bridge lost it. Latched.
    pub fn is_crashed(&self) -> bool {
        self.lifecycle.is_crashed()
    }

    /// Waits, for at most `within`, until the bridge has handed the listener
    /// everything the server has sent so far (news the control lane's thread saw is read and
    /// delivered, and no byte is left unread on the connection). It sleeps on
    /// the bridge's progress, which each turn of the bridge thread and the
    /// server's end notify: call it from the control thread only. `false` when
    /// it did not settle in time (a bridge held inside its listener never does)
    /// or the bridge is dead. When nothing is pending this is an atomic load
    /// and a `poll` without a wait.
    pub(crate) fn news_settled(&self, within: std::time::Duration) -> bool {
        let deadline = crate::Wake::deadline_in(within);
        self.channels.progress().wait_until(deadline, || {
            // The bytes first, then the flag: the bridge thread sets the flag
            // before it takes bytes off the connection, so bytes gone from the
            // connection mean the flag is already set. The other order can read
            // the flag unset, then see the bytes gone.
            self.lifecycle.is_crashed()
                || !(self.channels.bytes_waiting() || self.channels.news_pending())
        });
        !self.lifecycle.is_crashed()
    }

    /// The gate a thread waiting on this bridge sleeps on: it is notified when
    /// a response is queued, when the bridge thread has finished a turn, and
    /// when the server is declared gone.
    pub(crate) fn progress(&self) -> &Arc<crate::Wake> {
        self.channels.progress()
    }

    /// Why this bridge died, or `None` while it is alive.
    ///
    /// Latched at the detection site, so it answers for a crash that happened
    /// before any listener was installed — which the connect- and
    /// handshake-failure paths routinely do.
    pub(crate) fn crash_cause(&self) -> Option<PluginFailure> {
        self.lifecycle.crash_cause()
    }

    /// The server's watch: the latch the stream's end, the process's end and
    /// the host's verdict all set.
    pub(crate) fn watch(&self) -> &Arc<ServerWatch> {
        self.lifecycle.watch()
    }

    /// The sample rate (Hz) the plugin refused and its reason, while it is
    /// refused: it is loaded, renders silence, and is retried at the next
    /// rate change. `None` once it takes a rate.
    pub fn rate_refusal(&self) -> Option<RateRefusal> {
        self.lifecycle.rate_refusal()
    }

    /// Has the bridge thread offer each rate refusal to `wait`
    /// ([`OfflineWait::refusal`](crate::OfflineWait::refusal)), so the audio thread only latches a flag.
    pub(crate) fn offer_refusals_to(&self, wait: Arc<crate::OfflineWait>) {
        self.lifecycle.offer_refusals_to(wait);
    }

    /// Whether the plugin refuses its rate, as [`rate_refusal`](Self::rate_refusal)
    /// is `Some`, for the audio thread: wait-free, no lock and no allocation.
    pub(crate) fn rate_refused(&self) -> bool {
        self.lifecycle.is_rate_refused()
    }

    /// Blocks the bridge thread abandoned, owed replies it collected late,
    /// and replies it took (on time, or owed and taken back off the socket),
    /// ever. The bridge thread writes them and any thread reads them: relaxed
    /// atomics, no lock, no allocation.
    pub fn block_counts(&self) -> tutti_plugin_types::PipelineCounts {
        self.channels.block_counts()
    }

    /// Latches `cause` as the reason the bridge is dead (the first cause
    /// wins), so a host that ends the server itself reads the reason it
    /// gave.
    pub(crate) fn mark_crashed(&self, cause: PluginFailure) {
        self.lifecycle.mark_crashed(cause);
    }

    /// How many plugin-originated parameter-change events the bridge
    /// dropped, over its life, because more arrived during one command (a
    /// preset or state load) than it holds between two listener calls.
    ///
    /// Each drop also delivers a `Resync(ParamValues)`, so a listener that
    /// re-reads on it loses nothing; this says how often that happened.
    /// Latency, tail and resync events are never dropped.
    pub fn dropped_parameter_events(&self) -> u64 {
        self.channels.dropped_param_events()
    }

    /// The count of commands the audio side could not hand to the bridge
    /// thread (a full command queue, an empty payload pool, a parameter buffer
    /// with no room to return to), shared: the plugin node's controls read it.
    pub(crate) fn commands_refused_cell(&self) -> tutti_rt::Tally<u64> {
        self.channels.commands_refused_cell()
    }

    /// The host parameter writes the plugin did not get, ever: what a flush the
    /// server refused or had no Processor for left undelivered, counted when
    /// its answer comes, late or not. A flush never answered counts nothing: it
    /// may still land, and a flush in flight when the server is lost has no
    /// answer to say whether the plugin took it (the loss is the crash, which
    /// ends the plugin with everything it held). A write refused at the call is
    /// not here ([`ParamWriteError`]).
    pub(crate) fn unapplied_edits(&self) -> u64 {
        self.channels.unapplied_edits_cell().get()
    }

    /// The count of parameter buffers the audio thread leaked rather than
    /// free, shared. See `commands_refused_cell`.
    pub(crate) fn leaked_buffers_cell(&self) -> tutti_rt::Tally<u64> {
        self.channels.leaked_buffers_cell()
    }

    /// The count of audio responses the bridge thread could not queue for the
    /// audio thread, shared. See `commands_refused_cell`.
    pub(crate) fn responses_lost_cell(&self) -> tutti_rt::Counter<u64> {
        self.responses_lost.clone()
    }

    /// The count of MIDI events the bridge thread did not send (a block it
    /// skipped as stale, and the rest of every SysEx open in it), shared:
    /// the plugin node's controls read it beside the node's own.
    pub fn dropped_midi_cell(&self) -> tutti_rt::Tally<u64> {
        self.channels.dropped_midi_cell()
    }

    /// The count of chunks the bridge thread skipped as stale, shared: the
    /// pipeline's counters read it beside the batcher's own.
    pub(crate) fn skipped_cell(&self) -> tutti_rt::Tally<tutti_units::Chunks> {
        self.channels.skipped_cell()
    }

    /// What the bridge thread learned of chunk `seq`, among the last few.
    pub(crate) fn batch_outcome(&self, seq: u64) -> Option<BatchOutcome> {
        self.channels.fate(seq)
    }

    // --- The host's parameter writes ---

    /// Stores a parameter write for the plugin, to ride the next block the audio
    /// lane sends as a change on its first frame. Never dropped for lack of
    /// room: a write replaces the value of the same parameter no block has
    /// taken, so the latest wins. **A control-side call**: it takes a lock, and
    /// the first write asks the plugin for its parameters, once.
    ///
    /// A crashed plugin, or one that does not answer the parameter list, stores
    /// nothing and is refused by name.
    ///
    /// # Errors
    ///
    /// [`ParamWriteError::Undeclared`] when the parameters the table holds do
    /// not include `id`. The table follows the plugin's own word that its list
    /// changed ([`invalidate_host_edits`](Self::invalidate_host_edits)), never a
    /// write. [`ParamWriteError::Lost`] for a crashed plugin and
    /// [`ParamWriteError::Unanswered`] for one that gave no list.
    pub(crate) fn write_parameter(
        &self,
        id: ParamId,
        value: Normalized,
    ) -> Result<(), ParamWriteError> {
        if self.lifecycle.is_crashed() {
            return Err(ParamWriteError::Lost(id));
        }
        if let Some(table) = self.host_edits.lock().as_mut() {
            table.written = true;
            return Ok(table.writer.write(id, value)?);
        }
        // No table: the plugin's list is asked, once, outside the lock (a round
        // trip), and the table is built from it.
        let Some(infos) = self.parameters() else {
            return Err(ParamWriteError::Unanswered(id));
        };
        self.declare_host_edits(infos.iter().map(ParameterInfo::id));
        match self.host_edits.lock().as_mut() {
            Some(table) => {
                table.written = true;
                Ok(table.writer.write(id, value)?)
            }
            None => Err(ParamWriteError::Unanswered(id)),
        }
    }

    /// Builds the table of the host's slots from the parameters the plugin
    /// declares (`ids`) when there is none: the list a write's policy check
    /// asked for is the table's, so a write does not ask twice. A table that
    /// stands is kept, with what its slots hold: it changes only when the plugin
    /// says its list did ([`invalidate_host_edits`](Self::invalidate_host_edits)).
    pub(crate) fn declare_host_edits(&self, ids: impl IntoIterator<Item = ParamId>) {
        let mut slot = self.host_edits.lock();
        if slot.is_none() {
            let (writer, reader) = edit_slots(ids);
            self.channels.install_host_edits(reader);
            *slot = Some(HostEdits {
                writer,
                written: false,
            });
        }
    }

    /// The plugin said its parameter list changed (or it died): the table is
    /// built again from the list at the next write. What the old slots held and
    /// no block took is dropped, and the host's mirror restores it.
    pub(crate) fn invalidate_host_edits(&self) {
        *self.host_edits.lock() = None;
    }

    /// Makes every write made so far stale: a state or preset load replaced the
    /// values they were written against, so none still in its slot lands after it,
    /// and none the lane owes again (a request the server did not hand over) is
    /// sent: the lane voids what it drained in an earlier epoch before it builds
    /// the next request.
    ///
    /// **The overlap window:** a write the audio lane has already taken into a
    /// request it is sending is past the slots and is not voided. The load rides
    /// the control lane, which waits for the blocks pushed before it, so such a
    /// write reaches the server before the load and the load replaces it; one in
    /// a block the lane sends after the load was asked for can land after it, as
    /// a block overlaps any host call (the in-process path has the same window).
    pub(crate) fn reset_host_edits(&self) {
        if let Some(table) = self.host_edits.lock().as_mut() {
            table.writer.reset();
        }
    }

    /// The host's writes the plugin has not applied, newest per parameter: from
    /// the writer's own record, so it never waits for the server
    /// ([`EditWriter::pending`]). Empty before the first write.
    pub fn pending_edits(&self) -> Vec<(ParamId, Normalized)> {
        self.host_edits
            .lock()
            .as_ref()
            .map(|table| table.writer.pending())
            .unwrap_or_default()
    }

    /// Asks the audio lane to send the writes no block has carried yet, in order
    /// with the blocks: the command carries none, and the lane takes them from the
    /// slots when it reaches it. `None` when the queue is full (the writes stay
    /// in their slots for the next block) or the plugin is dead.
    fn request_flush(&self) -> Option<ask::Ask<bool>> {
        if self.lifecycle.is_crashed() {
            return None;
        }
        // Nothing written since the last flush: nothing for the lane to send.
        if !self
            .host_edits
            .lock()
            .as_mut()
            .is_some_and(|table| std::mem::take(&mut table.written))
        {
            return None;
        }
        let (done_ask, done) = ask::ask::<bool>();
        if self.channels.push_block(AudioCommand::Params { done }) {
            Some(done_ask)
        } else {
            // The flush never queued: the writes are still unsent, so the next
            // request asks again.
            if let Some(table) = self.host_edits.lock().as_mut() {
                table.written = true;
            }
            None
        }
    }

    /// Queue an automation-state push, saying **why** if it did not go.
    ///
    /// The one member of this family whose answer a caller reads and turns into
    /// a user-visible message (`AnyThread::set_automation_mode`). The
    /// other nine are `let _ =`'d or forwarded, so they keep the bool: an enum
    /// nobody matches on is ceremony, and this family is RT-adjacent.
    ///
    /// The two causes want opposite responses — a dead plugin will answer the
    /// same forever, a full queue may take the very next call — and a `bool`
    /// merged them into one "not delivered" that a caller could only report,
    /// never act on.
    pub(crate) fn set_automation_state_rt(
        &self,
        mode: tutti_plugin_types::AutomationMode,
    ) -> Delivered {
        self.tell(SetAutomationState { mode })
    }

    /// Posts one step of a host gesture and sends what the control queue takes.
    /// A step the full queue refuses stays queued in order, for
    /// [`flush_host_edits`](Self::flush_host_edits): a begin and an end are
    /// never dropped.
    pub(crate) fn host_edit(&self, edit: tutti_plugin_types::ParamEdit) {
        self.gesture_outbox.post(edit);
        self.flush_host_edits();
    }

    /// Sends the gesture steps the control queue refused before.
    pub(crate) fn flush_host_edits(&self) {
        self.gesture_outbox.drain_while(|edit| {
            self.channels
                .push_control_back(ControlCommand::tell(HostEdit { edit }))
                .is_ok()
        });
    }

    /// Queues what the host says of the plugin's track, saying **why** if it
    /// did not go. Main thread: the value is cloned here, off the audio thread.
    pub(crate) fn set_track_info(&self, info: &tutti_plugin_types::TrackInfo) -> Delivered {
        self.tell(SetTrackInfo { info: info.clone() })
    }

    /// Asks for a rate change, ordered against the blocks. `false` only when the
    /// bridge is crashed: the request goes into a slot (the latest value), never
    /// a queue that can be full. Not for the audio thread (a lock).
    ///
    /// `true` means it is requested, not that the plugin has taken it: the audio
    /// lane holds the blocks submitted after it until the plugin has answered or
    /// its deadline passes. A change the bridge then gave up on is counted
    /// ([`unapplied_settings`](Self::unapplied_settings)) and logged.
    pub(crate) fn set_sample_rate(&self, rate: SampleRate) -> bool {
        // Publish before requesting, so both the staleness bound and
        // the bridge thread's reply timeout track the new period from here on.
        self.channels.set_sample_rate(rate);
        if self.lifecycle.is_crashed() {
            return false;
        }
        self.channels.request_setting(Setting::SampleRate(rate));
        true
    }

    /// Tells the plugin `rate` and waits for its answer, which is the caller's
    /// alone: it is not filed as a [`Told`](tutti_plugin_types::Told), so the
    /// lifecycle and the listener never hear it (a restart's own rate is
    /// answered by its activation). The standing refusal is recorded as a
    /// rate change records it. Main thread; not for a block.
    ///
    /// # Errors
    ///
    /// Why the plugin does not run at `rate`.
    pub(crate) fn set_sample_rate_now(
        &self,
        rate: SampleRate,
    ) -> std::result::Result<(), tutti_plugin_types::RateRefusal> {
        self.channels.set_sample_rate(rate);
        let cause = match self.call(SetSampleRate { rate }, Deadline::Rate, Object::None) {
            Ok(set) => set.refusal,
            Err(Lost::NoAnswer { after }) => Some(RateRefusalCause::NoAnswer { after }),
            Err(lost) => Some(RateRefusalCause::Deactivated {
                reason: format!("the rate was not delivered: {lost:?}"),
            }),
        };
        let refusal = cause.map(|cause| tutti_plugin_types::RateRefusal::new(rate, cause));
        self.lifecycle.set_rate_refusal(refusal.clone());
        refusal.map_or(Ok(()), Err)
    }

    /// Asks for a render-mode change, ordered against the blocks. `false` only
    /// when the bridge is crashed; the request goes into a slot, never a queue
    /// that can be full.
    ///
    /// `true` means it is requested, not that the plugin has applied it: the
    /// audio lane holds the blocks submitted after it until the server has
    /// answered a request sent after it, or its deadline passes. A change the
    /// bridge then gave up on is counted
    /// ([`unapplied_settings`](Self::unapplied_settings)) and logged.
    ///
    /// Not for the audio thread (a lock): a bounce sets this once from the
    /// control thread before it starts pulling. The blocks submitted before it
    /// are sent first; a mode that overtook an in-flight block would apply to
    /// audio the caller thought was already rendered.
    pub fn set_render_mode(&self, mode: tutti_plugin_types::RenderMode) -> bool {
        if self.lifecycle.is_crashed() {
            return false;
        }
        self.channels.request_setting(Setting::RenderMode(mode));
        true
    }

    /// How many render-mode and rate changes the bridge gave up on, ever: the
    /// plugin did not apply the change within the fence deadline. The change may
    /// still land; the blocks after it were not held for it.
    pub fn unapplied_settings(&self) -> u64 {
        self.channels.unapplied_cell().get()
    }

    /// Makes the bridge thread wait `wait` for each block's reply, in place of
    /// the few block periods a device's block is given, or the live bound again
    /// with `None`. An offline render has no deadline: a reply that comes after
    /// the live bound carries the block's MIDI-out all the same.
    pub(crate) fn set_reply_wait(&self, wait: Option<Duration>) {
        self.channels.set_reply_wait(wait);
    }

    /// Queues a reset of the plugin's processing state. Realtime-safe; `false`
    /// when it was not queued.
    pub(crate) fn reset_rt(&self) -> bool {
        !self.lifecycle.is_crashed() && self.channels.push_block(AudioCommand::Reset)
    }

    // --- RT submit (fire-and-forget) ---

    /// Hand block `seq` to the bridge thread and return **immediately**.
    ///
    /// Lock-free, allocation-free, and it never waits — see the module doc on
    /// `Batcher` for why waiting here would overrun the audio callback.
    ///
    /// The caller collects block `seq`'s *output* on a later call, gated on the
    /// slab's sequence number rather than on a reply (see `Batcher::collectable`).
    /// [`Submitted::accepted`] means only "the bridge accepted this block", never
    /// "the output is ready".
    ///
    /// **`param_changes` is swapped, not moved.** When it holds a queue, it
    /// goes into the block and the caller's buffer is replaced with a free
    /// one of the same reserved capacity (`PayloadPool::reserve_params`), so
    /// neither side allocates. With no free buffer (every one in flight), or
    /// when the block is refused, the changes stay where they were and
    /// the result is [`Submitted::Block`]: the caller still holds them and
    /// decides what to do (the plugin node sends each parameter's latest
    /// value on its next chunk). An empty one is never swapped.
    ///
    /// What comes back through the queue, the plugin's MIDI-out, is taken
    /// with the block's audio: [`Replies::take`].
    pub(crate) fn submit(
        &self,
        seq: u64,
        num_samples: usize,
        midi_events: MidiEventVec,
        param_changes: &mut ParameterChanges,
        note_expression: NoteExpressionChanges,
        harmony: HarmonyInputs,
        transport: TransportInfo,
        transport_changes: TransportChanges,
    ) -> Submitted {
        if self.lifecycle.is_crashed() {
            return Submitted::Refused;
        }

        // An empty pool refuses the block (counted by the pool): nothing has
        // moved out of the caller's hands.
        let Some(mut payload) = self.payloads.acquire() else {
            return Submitted::Refused;
        };
        payload.seq = seq;
        payload.num_samples = num_samples;
        payload.midi_events = midi_events;
        // A pooled payload's changes are always the empty placeholder (the
        // bridge thread takes the buffer out before recycling), so the
        // assignments below drop nothing that holds memory.
        let mut params = param_changes.queues.is_empty();
        if !params && let Some(mut fresh) = self.payloads.take_params() {
            std::mem::swap(param_changes, &mut fresh);
            payload.param_changes = fresh;
            params = true;
        }
        payload.note_expression = note_expression;
        payload.chords = harmony.chords;
        payload.scales = harmony.scales;
        payload.expr_texts = harmony.expr_texts;
        payload.expr_ints = harmony.expr_ints;
        payload.transport = transport;
        payload.transport_changes = transport_changes;

        // Publish the block number *before* queueing it, so the bridge thread
        // can never dequeue a command that looks newer than what the host admits
        // to having submitted. The reverse order would leave a window where a
        // block is judged against a stale `newest` and dropped as if it were two
        // blocks old.
        self.channels.note_submitted(seq);
        let pushed = self.channels.try_push_block(AudioCommand::Process(payload));
        if pushed.is_ok() {
            self.channels.note_pushed(seq);
        }
        match pushed {
            Ok(()) if params => Submitted::BlockAndParams,
            Ok(()) => Submitted::Block,
            Err(cmd) => {
                // Refused (the queue is full): hand the changes back to the
                // caller and the buffers back to their pools, rather than
                // dropping (freeing) them on this thread.
                if let AudioCommand::Process(mut payload) = cmd {
                    if !payload.param_changes.queues.is_empty() {
                        std::mem::swap(param_changes, &mut payload.param_changes);
                    }
                    self.payloads
                        .return_params_rt(std::mem::take(&mut payload.param_changes));
                    self.payloads.recycle(payload);
                }
                Submitted::Refused
            }
        }
    }

    // --- Main-thread sync request+response ---

    /// The plugin's state for `state_use`, read through the server, with the
    /// writes stated beside it. Blocks while
    /// the server saves it and sends it: up to [`Timeouts::state_wait`] for a
    /// request already owed, and again for the answer.
    ///
    /// Ordered one way against blocks: the request is sent after the blocks
    /// submitted before it have been answered, so it sees what they did; blocks
    /// submitted after it may be sent first.
    ///
    /// # Errors
    ///
    /// The plugin is dead, refused, or the transfer stalled.
    pub fn save_state(
        &self,
        state_use: StateUse,
    ) -> std::result::Result<tutti_plugin_types::PluginState, StateError> {
        // The writes no block has applied are not in the bytes: they are
        // stated beside them, read before the request.
        let pending = self.pending_edits();
        // A refusal to answer is not an empty state: collapsing it to `vec![]`
        // is what let an unsaved preset look like a plugin that had nothing to
        // save.
        self.call_with(
            SaveState { state_use },
            Deadline::State,
            Object::State,
            move |outcome| outcome.and_then(|received| saved(received, pending)),
        )
        .map_err(state_lost)
    }

    /// Restores a state saved for `state_use` in the plugin. Blocks while it
    /// travels.
    ///
    /// # Errors
    ///
    /// The plugin is dead, refused it, or the transfer stalled.
    pub(crate) fn load_state(
        &self,
        data: &[u8],
        state_use: StateUse,
    ) -> std::result::Result<(), StateError> {
        let (msg, object) = Object::announced(self.state_message(data, state_use)?);
        // A refusal to answer is not an acceptance.
        state_loaded(self.call(msg, Deadline::State, object))
    }

    /// The object a state travels as: the one copy of it the host makes, on the
    /// calling thread, which the bridge sends as it is. Voids the writes made so
    /// far (they were made against the values the load replaces).
    ///
    /// # Errors
    ///
    /// The plugin is dead, or the state is over [`MAX_STATE_BYTES`]
    /// (refused here, before an object is made of it), or the object could not
    /// be made.
    ///
    /// [`MAX_STATE_BYTES`]: tutti_plugin_types::MAX_STATE_BYTES
    fn state_message(
        &self,
        data: &[u8],
        state_use: StateUse,
    ) -> std::result::Result<Announced<LoadState>, StateError> {
        if self.lifecycle.is_crashed() {
            return Err(StateError::PluginCrashed);
        }
        self.reset_host_edits();
        if data.len() > tutti_plugin_types::MAX_STATE_BYTES {
            return Err(StateError::TooLarge {
                bytes: data.len(),
                limit: tutti_plugin_types::MAX_STATE_BYTES,
            });
        }
        let blob = Blob::create(data).map_err(|e| {
            StateError::Rejected(format!("the state could not be made into an object: {e}"))
        })?;
        Ok(Announced::<LoadState>::new(blob, state_use))
    }

    /// Asks the server `msg` and waits for what `finish` makes of the lane's
    /// outcome, on the lane's thread. A request made while another owes a reply
    /// **waits for it** up to `deadline`'s wait (the caller's own timeout), in
    /// place of queueing a second request behind it on the lane or failing at
    /// once: [`Lost::Busy`] (counted) when the slot is still held then, or the
    /// queue is full. The slot is the request's until the lane's thread answers
    /// it or drops it.
    fn call_with<R: Send + 'static>(
        &self,
        msg: impl Answered,
        deadline: Deadline,
        object: Object,
        finish: impl FnOnce(Outcome) -> std::result::Result<R, Lost> + Send + 'static,
    ) -> std::result::Result<R, Lost> {
        if self.lifecycle.is_crashed() {
            return Err(Lost::Crashed);
        }
        let timeouts = self.lifecycle.timeouts();
        let wait = deadline.wait(timeouts);
        let Some((waiting, reply)) = self.channels.owing().ask(wait) else {
            self.channels.note_command_refused();
            return Err(Lost::Busy);
        };
        let call = Call {
            flush: None,
            deadline,
            object,
            done: Box::new(move |outcome| reply.send(finish(outcome))),
        };
        // A refused push drops the command and its reply with it: the slot is free.
        if !self.channels.push_control(ControlCommand::ask(msg, call)) {
            return Err(Lost::Busy);
        }
        // The lane answers at the request's own deadline, so this outlasts it by
        // a margin and only ends sooner if the lane is gone.
        match waiting.recv_timeout(wait) {
            Ok(answer) => answer,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Lost::Crashed),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(Lost::NoAnswer {
                after: deadline.window(timeouts).0,
            }),
        }
    }

    /// Asks the server the answered request `M`: its reply, or why there is none.
    /// As [`call_with`](Self::call_with), with the reply read as `M::Reply` and
    /// no other kind.
    fn call<M: Answered + 'static>(
        &self,
        msg: M,
        deadline: Deadline,
        object: Object,
    ) -> std::result::Result<M::Reply, Lost>
    where
        M::Reply: Send + 'static,
    {
        self.call_with(msg, deadline, object, typed::<M>)
    }

    /// Queues `msg` and returns the [`Answer`] **the control lane's thread
    /// fills** with what `finish` makes of the lane's outcome, at the server's
    /// reply or at the request's deadline. A lane that ends drops the request
    /// unanswered, which cancels the answer, as does `finish` returning `None`
    /// (a query refused by a full queue: not an answer it can give). The request
    /// takes no slot and queues behind one in flight, and the lane waits for
    /// `flush` (the audio lane's answer to the host's writes) before it sends.
    fn answer_with<T: Send + 'static>(
        &self,
        msg: impl Answered,
        deadline: Deadline,
        flush: Option<Ask<bool>>,
        object: Object,
        finish: impl FnOnce(Outcome) -> Option<T> + Send + 'static,
    ) -> Answer<T> {
        let (answer, reply) = ask::awaited();
        if self.lifecycle.is_crashed() {
            if let Some(value) = finish(Err(Lost::Crashed)) {
                reply.send(value);
            }
            return answer;
        }
        let call = Call {
            flush,
            deadline,
            object,
            // `None` drops the reply unanswered, which cancels the answer.
            done: Box::new(move |outcome| {
                if let Some(value) = finish(outcome) {
                    reply.send(value);
                }
            }),
        };
        self.queue(ControlCommand::ask(msg, call));
        answer
    }

    /// [`answer_with`](Self::answer_with) for a reply read as `M::Reply`.
    fn call_answer<M: Answered + 'static, T: Send + 'static>(
        &self,
        msg: M,
        deadline: Deadline,
        flush: Option<Ask<bool>>,
        object: Object,
        finish: impl FnOnce(std::result::Result<M::Reply, Lost>) -> Option<T> + Send + 'static,
    ) -> Answer<T> {
        self.answer_with(msg, deadline, flush, object, move |outcome| {
            finish(typed::<M>(outcome))
        })
    }

    /// Queues a request nothing answers, saying **why** if it did not go.
    fn tell(&self, msg: impl Unanswered) -> Delivered {
        delivery(self.lifecycle.is_crashed(), || {
            self.channels.push_control(ControlCommand::tell(msg))
        })
    }

    // --- The editor in the server ---

    /// Opens the editor in the server: embedded in `parent` (the platform
    /// number of the host's window), or the plugin's own floating window for
    /// `None`. The size when it embedded, `None` for a floating one.
    ///
    /// # Errors
    ///
    /// The plugin is dead, the queue is full, the server did not answer, or the
    /// server (the plugin) refused.
    pub(crate) fn open_editor_in_server(
        &self,
        parent: Option<u64>,
        scale: ContentScale,
    ) -> std::result::Result<Option<Size>, EditorError> {
        // The server may take as long as a plugin takes to build its window; the
        // lane answers with its own refusal at `Timeouts::editor`.
        self.call(OpenEditor { parent, scale }, Deadline::Editor, Object::None)
            .map_err(|lost| editor_lost(lost, self.lifecycle.is_crashed()))?
            .result
    }

    /// Closes the server's editor. No answer: `false` when the command could
    /// not be queued.
    pub(crate) fn close_editor_in_server(&self) -> bool {
        matches!(self.tell(CloseEditor), Delivered::Yes)
    }

    /// Asks the server's editor to take `size`, and answers with the size it
    /// took.
    ///
    /// # Errors
    ///
    /// As [`open_editor_in_server`](Self::open_editor_in_server).
    pub(crate) fn set_editor_size_in_server(
        &self,
        size: Size,
    ) -> std::result::Result<Size, EditorError> {
        self.call(SetEditorSize { size }, Deadline::Editor, Object::None)
            .map_err(|lost| editor_lost(lost, self.lifecycle.is_crashed()))?
            .result
    }

    /// The size the server's editor asked its window to take since the last
    /// call.
    pub(crate) fn take_editor_wants_size(&self) -> Option<Size> {
        self.lifecycle.take_editor_wants()
    }

    /// Whether the server closed its editor on its own account since the last
    /// call.
    pub(crate) fn take_editor_closed(&self) -> bool {
        self.lifecycle.take_editor_closed()
    }

    /// The plugin's parameters, or `None` when it cannot be asked. Waits up to a
    /// query's wait for a request already owed, then up to it again for its own
    /// answer.
    pub fn parameters(&self) -> Option<Vec<ParameterInfo>> {
        let list = self.call(GetParameterList, Deadline::Query, Object::None);
        list.ok().map(|list| list.parameters)
    }

    /// The plugin's preset list, or `None` when the subprocess is gone.
    ///
    /// `None` and `Some(vec![])` are different answers: the first means the
    /// question could not be asked, the second that the plugin listed nothing.
    pub fn presets(&self) -> Option<Vec<PluginPreset>> {
        let list = self.call(GetPresetList, Deadline::Query, Object::None);
        list.ok().map(|list| list.presets)
    }

    /// Ask the plugin to load a preset. `false` when it refused, the format has
    /// no load path, or the subprocess is gone — all three mean the preset did
    /// not load, and a caller must leave its selection where it was.
    pub(crate) fn load_preset(&self, id: PresetId) -> bool {
        self.reset_host_edits();
        preset_loaded(self.call(LoadPreset { id }, Deadline::Query, Object::None)).is_ok()
    }

    // --- Requests answered into a future ---

    /// [`save_state`](Self::save_state) as an [`Answer`]: queues the request and
    /// returns, and **the control lane's thread fills the answer** when the
    /// server's reply lands. That lane owns the deadline: with no reply within
    /// [`Timeouts::state`] (renewed by the server's `Working`, up to the
    /// ceiling) it sends [`StateError::Stalled`] into the answer, so no caller
    /// waits and none polls. A lane that ends drops the request unanswered, which
    /// cancels the answer. The request takes no slot (`ControlSlot`) and
    /// queues behind one in flight.
    pub fn save_state_answer(
        &self,
        state_use: StateUse,
    ) -> Answer<std::result::Result<tutti_plugin_types::PluginState, StateError>> {
        // The lane sends the flush ahead of the save and does not wait for the
        // writes to apply: what is pending is stated beside the bytes, read
        // before the request.
        let pending = self.pending_edits();
        let flush = self.request_flush();
        self.answer_with(
            SaveState { state_use },
            Deadline::State,
            flush,
            Object::State,
            move |outcome| {
                Some(
                    outcome
                        .and_then(|received| saved(received, pending))
                        .map_err(state_lost),
                )
            },
        )
    }

    /// `load_state` as an [`Answer`], filled and bounded as
    /// [`save_state_answer`](Self::save_state_answer) is: [`StateError::Stalled`]
    /// at [`Timeouts::state`]. The server may apply a state whose answer is no
    /// longer awaited.
    pub fn load_state_answer(
        &self,
        data: &[u8],
        state_use: StateUse,
    ) -> Answer<std::result::Result<(), StateError>> {
        match self.state_message(data, state_use) {
            Ok(announced) => {
                let (msg, object) = Object::announced(announced);
                self.call_answer(msg, Deadline::State, None, object, |a| {
                    Some(state_loaded(a))
                })
            }
            Err(error) => Answer::ready(Err(error)),
        }
    }

    /// `load_preset` as an [`Answer`], filled and bounded as
    /// [`save_state_answer`](Self::save_state_answer) is:
    /// [`PresetError::Stalled`] at [`Timeouts::param`].
    pub fn load_preset_answer(&self, id: PresetId) -> Answer<std::result::Result<(), PresetError>> {
        self.reset_host_edits();
        self.call_answer(
            LoadPreset { id },
            Deadline::Query,
            None,
            Object::None,
            |a| Some(preset_loaded(a)),
        )
    }

    /// [`parameters`](Self::parameters) as an answer: queued, and **the control
    /// lane's thread fills it** with the server's reply, or with `None` at the
    /// query deadline ([`Timeouts::param`]). A lane that ends drops the request
    /// unanswered, which cancels the answer, as does a full queue. The request
    /// takes no slot and queues behind one in flight.
    pub fn parameters_answer(&self) -> Answer<Option<Vec<ParameterInfo>>> {
        self.call_answer(
            GetParameterList,
            Deadline::Query,
            None,
            Object::None,
            |list| unless_refused(list).map(|list| list.ok().map(|list| list.parameters)),
        )
    }

    /// [`parameter`](Self::parameter) as an answer, filled and bounded as
    /// [`parameters_answer`](Self::parameters_answer) is. The lane applies the
    /// host's writes no block has carried before it asks.
    pub fn parameter_answer(
        &self,
        param_id: ParamId,
    ) -> Answer<Option<tutti_plugin_types::PluginValue>> {
        let pending = self
            .pending_edits()
            .into_iter()
            .find_map(|(id, value)| (id == param_id).then_some(value));
        let flush = self.request_flush();
        self.call_answer(
            GetParameter { param_id },
            Deadline::Query,
            flush,
            Object::None,
            move |read| {
                unless_refused(read).map(|read| {
                    read.ok()
                        .and_then(|read| read.value)
                        .map(|plugin| tutti_plugin_types::PluginValue { plugin, pending })
                })
            },
        )
    }

    /// [`parameter_normalized`](Self::parameter_normalized) as an answer, filled
    /// and bounded as [`parameters_answer`](Self::parameters_answer) is.
    pub(crate) fn parameter_normalized_answer(
        &self,
        param_id: ParamId,
        plain: f64,
    ) -> Answer<Option<Normalized>> {
        self.call_answer(
            GetParameterNormalized { param_id, plain },
            Deadline::Query,
            None,
            Object::None,
            |read| unless_refused(read).map(|read| read.ok().and_then(|read| read.value)),
        )
    }

    /// Queues `command`; a full queue answers it `Busy`.
    fn queue(&self, command: ControlCommand) {
        if let Err(refused) = self.channels.push_control_back(command) {
            refused.refuse();
        }
    }

    /// Has the server open the slab `slab` names (laid out as `layout`) and
    /// render through it from its next block. `true` once it has; `false` when
    /// it refused, did not answer within [`Timeouts::param`](tutti_plugin_types::Timeouts::param) or is gone.
    /// Control thread; the old slab stays valid until the caller drops it.
    pub(crate) fn setup_shared_memory(&self, slab: SlabHandle, layout: SlabLayout) -> bool {
        let (msg, object) = Object::announced(Announced::<SetupSharedMemory>::new(slab, layout));
        self.call(msg, Deadline::Query, object).is_ok()
    }

    /// Wait until every setting queued before this call (a rate change, a render
    /// mode) has been applied, the control commands already in flight to the
    /// server have been handled, and the events the server sent because of them
    /// (a `LatencyChanged` a render-mode or rate change caused) have reached the
    /// listener. Control thread.
    ///
    /// **What the fence proves is that and no more**: a settle rides the audio
    /// lane's queue behind the settings, but settings jump the control queue, so
    /// a control command still queued (a `SetParameter`, say) is not waited for.
    /// The settle holds the blocks after it for two round trips to the control
    /// lane (see `AudioCommand::Settle` for why two) and takes no request slot.
    /// Blocks for at most two settle waits and a query's.
    pub(crate) fn settle(&self) {
        if self.lifecycle.is_crashed() {
            return;
        }
        let timeouts = self.lifecycle.timeouts();
        let (done_ask, done) = ask::ask::<()>();
        if !self.channels.push_block(AudioCommand::Settle { done }) {
            return;
        }
        let wait = timeouts
            .settle_wait()
            .saturating_mul(2)
            .saturating_add(timeouts.param_wait());
        let _ = await_reply(done_ask, wait);
    }

    /// Which preset the plugin considers current, if it will say. Waits up to
    /// a query's wait for a request already owed, then up to it again for its own
    /// answer.
    pub fn current_preset(&self) -> Option<PresetId> {
        let current = self.call(GetCurrentPreset, Deadline::Query, Object::None);
        current.ok().and_then(|current| current.id)
    }

    /// Hands the server's plugin the app's controller entries and returns who
    /// applies them. [`Host`](tutti_plugin_types::MidiMapApplied::Host) when the
    /// plugin is gone or does not answer in time: nothing was applied by it.
    pub(crate) fn set_app_midi_map(
        &self,
        app: &[tutti_plugin_types::MidiAssignment],
    ) -> tutti_plugin_types::MidiMapApplied {
        let msg = SetAppMidiMap { app: app.to_vec() };
        match self.call(msg, Deadline::Query, Object::None) {
            Ok(answer) => answer.applied,
            Err(_) => tutti_plugin_types::MidiMapApplied::Host,
        }
    }

    /// The server plugin's wiring as it reads now, or `None` when it is gone or
    /// does not answer in time.
    pub(crate) fn wiring(&self) -> Option<tutti_plugin_types::PluginWiring> {
        let read = self.call(GetWiring, Deadline::Query, Object::None);
        read.ok().map(|read| read.wiring)
    }

    /// Arms or disarms the server plugin's own MIDI learn; the plugin's refusal,
    /// or [`Lost`](tutti_plugin_types::PluginLearnError::Lost) when it is gone or
    /// does not answer in time.
    pub(crate) fn arm_plugin_midi_learn(
        &self,
        armed: bool,
    ) -> std::result::Result<(), tutti_plugin_types::PluginLearnError> {
        let msg = ArmPluginMidiLearn { armed };
        self.call(msg, Deadline::Query, Object::None)
            .map_err(|_| tutti_plugin_types::PluginLearnError::Lost)?
            .result
    }

    /// A parameter's value, or `None` when it cannot be asked. Waits up to a
    /// query's wait for a request already owed, then up to it again for its own
    /// answer.
    pub fn parameter(&self, param_id: ParamId) -> Option<Normalized> {
        // A read answers the plugin's own value: the writes no block has
        // applied are stated beside it ([`pending_edits`](Self::pending_edits)).
        let read = self.call(GetParameter { param_id }, Deadline::Query, Object::None);
        read.ok().and_then(|read| read.value)
    }

    /// The plain value the plugin's format converts `value` to, on one
    /// parameter. `None` when the subprocess is gone, the request could not be
    /// queued, or the format has no plain value for it.
    pub(crate) fn parameter_plain(&self, param_id: ParamId, value: Normalized) -> Option<f64> {
        let msg = GetParameterPlain { param_id, value };
        let read = self.call(msg, Deadline::Query, Object::None);
        read.ok().and_then(|read| read.plain)
    }

    /// The inverse of [`parameter_plain`](Self::parameter_plain).
    pub(crate) fn parameter_normalized(&self, param_id: ParamId, plain: f64) -> Option<Normalized> {
        let msg = GetParameterNormalized { param_id, plain };
        let read = self.call(msg, Deadline::Query, Object::None);
        read.ok().and_then(|read| read.value)
    }

    /// The plugin's display string for `value` on one parameter.
    ///
    /// `None` when the subprocess is gone, the request could not be queued, or
    /// the plugin declined — all three mean there is no text, and a caller
    /// renders the raw number.
    pub(crate) fn parameter_text(&self, param_id: ParamId, value: Normalized) -> Option<String> {
        let msg = GetParameterText { param_id, value };
        let read = self.call(msg, Deadline::Query, Object::None);
        read.ok().and_then(|read| read.text)
    }

    /// The value the plugin parses `text` into.
    ///
    /// `None` when it cannot parse the string, on the same three failure paths
    /// as [`parameter_text`](Self::parameter_text). A caller must leave its
    /// field unchanged rather than substituting a fallback.
    pub(crate) fn parameter_value_from_text(
        &self,
        param_id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        let msg = GetParameterValueFromText {
            param_id,
            text: text.to_string(),
        };
        let read = self.call(msg, Deadline::Query, Object::None);
        read.ok().and_then(|read| read.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state the audio lane's thread keeps, for a test that takes a flush's
    /// frame as the lane would.
    fn lane_state() -> dispatch::AudioState {
        dispatch::AudioState::new(
            tutti_plugin_types::Timeouts::default().drain,
            channels::response_ring().0,
        )
    }

    /// A bridge with no lane thread: its commands stay in the queues for a test
    /// to take, as the lane's thread would.
    fn bridge_without_a_lane() -> AudioBridge {
        let channels = Channels::new(SampleRate::SR_48K, Arc::default())
            .expect("the channels")
            .0;
        AudioBridge {
            payloads: PayloadPool::new(
                SampleRate::SR_48K,
                channels.refusal_writer(),
                channels.leak_writer(),
            ),
            channels,
            lifecycle: Lifecycle::new(Arc::new(ServerWatch::new()), Timeouts::default()),
            listener: Arc::new(Mutex::new(None)),
            responses_lost: channels::response_ring().2,
            host_edits: Arc::default(),
            gesture_outbox: tutti_plugin_types::HostEdits::new(),
        }
    }

    /// **A flush takes the writes when the lane dequeues it, not when it is
    /// asked for**: a write made after the request and before the lane reaches it
    /// is the one sent. Taking at the request left a window in which a block
    /// queued ahead could ship the newer value and the flush then overwrite it
    /// with the older.
    #[test]
    fn a_flush_takes_the_writes_when_the_lane_dequeues_it() {
        let bridge = bridge_without_a_lane();
        let id = ParamId::new(1);
        bridge.declare_host_edits([id]);
        assert_eq!(bridge.write_parameter(id, Normalized::of(0.25)), Ok(()));
        let _answer = bridge.request_flush().expect("the queue has room");
        assert_eq!(bridge.write_parameter(id, Normalized::of(0.75)), Ok(()));
        let Some(AudioCommand::Params { .. }) = bridge.channels.pop_block() else {
            panic!("the flush is queued");
        };
        let (changes, _) = lane_state().flush_frame(&bridge.channels);
        let sent: Vec<f64> = changes
            .queues
            .iter()
            .flat_map(|q| q.points.iter().map(|p| p.value.get()))
            .collect();
        assert_eq!(sent, [0.75], "the newest write, taken when dequeued");
    }

    /// **A flush the full block queue refused is asked for again**: the writes
    /// are still in their slots, so a later request sends them.
    #[test]
    fn a_flush_refused_on_a_full_queue_is_asked_for_again() {
        let bridge = bridge_without_a_lane();
        let id = ParamId::new(1);
        bridge.declare_host_edits([id]);
        assert_eq!(bridge.write_parameter(id, Normalized::of(0.5)), Ok(()));
        loop {
            let (_ask, done) = ask::ask::<bool>();
            if bridge
                .channels
                .try_push_block(AudioCommand::Params { done })
                .is_err()
            {
                break;
            }
        }
        assert!(bridge.request_flush().is_none(), "the queue is full");
        while bridge.channels.pop_block().is_some() {}
        assert!(
            bridge.request_flush().is_some(),
            "the writes still wait for a flush"
        );
    }

    /// **Writes either side of a queued flush end at the last**: W1, a flush
    /// asked for, a block queued ahead, W2, then the lane dequeues both. What the
    /// server is sent last for the parameter is W2.
    #[test]
    fn the_last_write_is_the_last_the_server_is_sent_whatever_is_queued() {
        let bridge = bridge_without_a_lane();
        let id = ParamId::new(1);
        bridge.declare_host_edits([id]);
        assert_eq!(bridge.write_parameter(id, Normalized::of(0.25)), Ok(()));
        let _answer = bridge.request_flush().expect("room");
        assert!(bridge.channels.push_block(AudioCommand::Params {
            done: ask::ask::<bool>().1
        }));
        assert_eq!(bridge.write_parameter(id, Normalized::of(0.75)), Ok(()));
        let mut last = None;
        let mut state = lane_state();
        while let Some(AudioCommand::Params { .. }) = bridge.channels.pop_block() {
            let (changes, _) = state.flush_frame(&bridge.channels);
            for point in changes.queues.iter().flat_map(|q| q.points.iter()) {
                last = Some(point.value.get());
            }
        }
        assert_eq!(last, Some(0.75));
    }

    /// **A block submitted to an empty pool is refused and counted, nothing
    /// queued and nothing allocated.** Every pooled payload is held out, as
    /// when more are out than the queue and both threads can hold.
    #[test]
    fn a_block_submitted_to_an_empty_pool_is_refused_and_counted() {
        let channels = Channels::new(SampleRate::SR_48K, Arc::default())
            .expect("the channels")
            .0;
        let bridge = AudioBridge {
            payloads: PayloadPool::new(
                SampleRate::SR_48K,
                channels.refusal_writer(),
                channels.leak_writer(),
            ),
            channels,
            lifecycle: Lifecycle::new(Arc::new(ServerWatch::new()), Timeouts::default()),
            listener: Arc::new(Mutex::new(None)),
            responses_lost: channels::response_ring().2,
            host_edits: Arc::default(),
            gesture_outbox: tutti_plugin_types::HostEdits::new(),
        };
        let mut held = Vec::new();
        while let Some(payload) = bridge.payloads.acquire() {
            held.push(payload);
        }
        let refusals = bridge.channels.commands_refused_cell();
        let before = refusals.get();
        let mut params = ParameterChanges::new();
        let submitted = assert_no_alloc::assert_no_alloc(|| {
            bridge.submit(
                1,
                64,
                MidiEventVec::new(),
                &mut params,
                Default::default(),
                Default::default(),
                TransportInfo::new(SampleRate::SR_48K),
                Default::default(),
            )
        });
        assert_eq!(submitted, Submitted::Refused);
        assert_eq!(refusals.get(), before + 1, "the refusal is counted");
        assert!(bridge.channels.pop_block().is_none(), "nothing queued");
    }

    /// **A full command queue is `Busy`, not a crash**. A bridge
    /// whose queue nothing drains (no thread) refuses a state request once
    /// the queue is full; the plugin is not dead and a retry is reasonable.
    /// Once the bridge is crashed, the same refusal is `PluginCrashed`.
    #[test]
    fn a_full_queue_is_busy_not_a_crash() {
        let channels = Channels::new(SampleRate::SR_48K, Arc::default())
            .expect("the channels")
            .0;
        let bridge = AudioBridge {
            payloads: PayloadPool::new(
                SampleRate::SR_48K,
                channels.refusal_writer(),
                channels.leak_writer(),
            ),
            channels,
            lifecycle: Lifecycle::new(Arc::new(ServerWatch::new()), Timeouts::default()),
            listener: Arc::new(Mutex::new(None)),
            responses_lost: channels::response_ring().2,
            host_edits: Arc::default(),
            gesture_outbox: tutti_plugin_types::HostEdits::new(),
        };
        while bridge
            .channels
            .push_control(ControlCommand::tell(crate::CloseEditor))
        {}

        assert_eq!(
            bridge.save_state(tutti_plugin_types::StateUse::Project),
            Err(StateError::Busy)
        );
        assert_eq!(
            bridge.load_state(&[1, 2, 3], tutti_plugin_types::StateUse::Project),
            Err(StateError::Busy)
        );
        bridge.lifecycle.mark_crashed(PluginFailure::StreamClosed);
        assert_eq!(
            bridge.load_state(&[1, 2, 3], tutti_plugin_types::StateUse::Project),
            Err(StateError::PluginCrashed)
        );
    }

    /// **A query the full queue refused is cancelled, not answered `None`**: an
    /// app must not read a request that was never asked as the plugin's "none".
    #[test]
    fn a_refused_ask_cancels_its_answer_and_is_not_read_as_none() {
        let bridge = bridge_without_a_lane();
        while bridge
            .channels
            .push_control(ControlCommand::tell(crate::CloseEditor))
        {}
        assert!(
            futures_lite::future::block_on(bridge.parameters_answer()).is_none(),
            "cancelled"
        );
        assert_eq!(
            futures_lite::future::block_on(
                bridge.load_preset_answer(PresetId::Location("/x".into()))
            ),
            Some(Err(PresetError::Busy)),
            "a preset load says Busy"
        );
    }

    /// **The host's wait outlasts the bridge's deadline by the margin, so a bridge
    /// that answers at its own deadline is still heard.** The bridge gives up on
    /// the server at `Timeouts::param` and replies into the `ask` then (a late
    /// answer or its `None`); the host's wait is `param_wait`, a margin longer.
    /// The rule is the pure `param_wait`, and an answer already in the `ask`
    /// is heard whatever the wait.
    #[test]
    fn the_hosts_wait_outlasts_the_bridges_deadline_by_the_margin() {
        let timeouts = {
            let mut t = Timeouts::default();
            t.param = Duration::from_millis(100);
            t.reply_margin = Duration::from_millis(200);
            t
        };
        assert_eq!(
            timeouts.param_wait(),
            timeouts.param + timeouts.reply_margin
        );
        let (waiting, reply) = ask::ask::<u8>();
        reply.send(7);
        assert_eq!(await_reply(waiting, timeouts.param_wait()), Ok(7));
    }

    /// The wait is still bounded: a bridge that never answers is given up on.
    #[test]
    fn a_bridge_that_never_answers_is_given_up_on() {
        let timeouts = {
            let mut t = Timeouts::default();
            t.param = Duration::from_millis(50);
            t.reply_margin = Duration::from_millis(50);
            t
        };
        let (waiting, _reply) = ask::ask::<u8>();
        assert_eq!(
            await_reply(waiting, timeouts.param_wait()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
    }

    /// The negotiated rate is stored once and read live.
    ///
    /// The audio thread never waits, so it sizes nothing from the period. The
    /// *bridge* thread does — its reply timeout in `dispatch` reads exactly this
    /// cell — so the storage has to keep working. The test goes through the real
    /// `Channels` rather than a local atomic, so it fails if that plumbing is
    /// disconnected.
    #[test]
    fn the_sample_rate_is_stored_and_updated_live() {
        let channels = Channels::new(SampleRate::SR_48K, Arc::default())
            .expect("the channels")
            .0;
        assert_eq!(channels.sample_rate(), SampleRate::SR_48K);

        channels.set_sample_rate(SampleRate::SR_192K);
        assert_eq!(channels.sample_rate(), SampleRate::SR_192K);
    }
}

/// What a push of a command comes to: nothing is queued for a plugin that is
/// gone, and a full queue refuses it. `push` runs only for a live plugin.
/// A lane's outcome as the reply of the request `M` and no other kind: the one
/// place a caller reads a reply's variant is `M::Reply`'s [`TryFrom`].
fn typed<M: Answered>(outcome: Outcome) -> std::result::Result<M::Reply, Lost> {
    M::Reply::try_from(outcome?.reply).map_err(Lost::of)
}

/// An answer to a query, or `None` when the queue refused it: a query that was
/// never asked has no answer, and must not read as the plugin's own "none".
fn unless_refused<R>(answer: std::result::Result<R, Lost>) -> Option<std::result::Result<R, Lost>> {
    match answer {
        Err(Lost::Busy) => None,
        other => Some(other),
    }
}

/// The state a save answered with: its bytes, and the writes pending on both
/// sides, the host's (`host`, read before the request) over the server's own
/// control's.
fn saved(
    received: Received,
    host: Vec<(ParamId, Normalized)>,
) -> std::result::Result<tutti_plugin_types::PluginState, Lost> {
    let ready = StateReady::try_from(received.reply).map_err(Lost::of)?;
    Ok(tutti_plugin_types::PluginState {
        bytes: received.object,
        pending: tutti_plugin_types::overlay_writes(ready.pending, host),
    })
}

/// A state request's failure as the error its caller reads.
fn state_lost(lost: Lost) -> StateError {
    match lost {
        Lost::Busy => StateError::Busy,
        Lost::Crashed => StateError::PluginCrashed,
        Lost::NoAnswer { after } => StateError::Stalled { after },
        Lost::Object(error) => error,
        Lost::Server(error) => StateError::Rejected(error.to_string()),
        Lost::Wrong => StateError::UnexpectedReply,
    }
}

/// A [`LoadState`]'s answer: the state was taken, or the plugin's refusal.
fn state_loaded(
    answer: std::result::Result<StateLoaded, Lost>,
) -> std::result::Result<(), StateError> {
    answer.map_err(state_lost)?.error.map_or(Ok(()), Err)
}

/// An editor request's failure as the error its caller reads.
fn editor_lost(lost: Lost, crashed: bool) -> EditorError {
    match lost {
        Lost::Busy => EditorError::Busy,
        // A lane that dropped the request without a crash is a request not served.
        Lost::Crashed if !crashed => EditorError::Busy,
        Lost::Crashed => EditorError::PluginCrashed,
        Lost::NoAnswer { after } => EditorError::NoAnswer { after },
        Lost::Server(error) => EditorError::Server(error),
        Lost::Wrong | Lost::Object(_) => EditorError::UnexpectedReply,
    }
}

/// A [`LoadPreset`]'s answer: the preset loaded, or why it did not. No answer
/// within the window is not a refusal: the plugin was asked and may have loaded
/// it.
fn preset_loaded(
    answer: std::result::Result<PresetLoaded, Lost>,
) -> std::result::Result<(), PresetError> {
    match answer {
        Ok(PresetLoaded { ok: true }) => Ok(()),
        Err(Lost::Busy) => Err(PresetError::Busy),
        Err(Lost::Crashed) => Err(PresetError::Lost),
        Err(Lost::NoAnswer { after }) => Err(PresetError::Stalled { after }),
        Ok(_) | Err(_) => Err(PresetError::Refused),
    }
}

fn delivery(crashed: bool, push: impl FnOnce() -> bool) -> Delivered {
    if crashed {
        Delivered::PluginDead
    } else if push() {
        Delivered::Yes
    } else {
        Delivered::Dropped
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    /// A command that was queued is `Yes`; one the full queue refused is
    /// `Dropped`, which a later call may cure; a dead plugin is `PluginDead`,
    /// and its queue is not even tried.
    #[test]
    fn a_push_is_delivered_dropped_or_to_a_dead_plugin_each_by_itself() {
        assert_eq!(delivery(false, || true), Delivered::Yes);
        assert_eq!(delivery(false, || false), Delivered::Dropped);
        assert_eq!(
            delivery(true, || panic!("a dead plugin's queue is not tried")),
            Delivered::PluginDead
        );
    }
}
