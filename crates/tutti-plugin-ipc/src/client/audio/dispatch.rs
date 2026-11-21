//! Per-command IPC choreography for the bridge's two lane threads: the audio
//! lane's ([`handle_audio`]) and the control lane's ([`handle_control`]).

use super::ask::Reply;
#[cfg(test)]
use super::channels::response_ring;
use super::channels::{BatchOutcome, Channels, ResponseWriter};
use super::lifecycle::Lifecycle;
use super::messages::{AudioCommand, AudioResponse, ControlCommand, Setting, Unsolicited};
use super::payload_pool::PayloadPool;
use crate::ProcessAudioData;
use crate::conversation::{Conversation, Fault, Heard};
use crate::error::unexpected_message;
use crate::transport::control::{self as ipc, ControlStream, PartialFrame};
use crate::transport::shm::RING_SLOTS;
use crate::{
    AudioReply, AudioRequest, ControlFrame, ControlReply, ControlRequest, News, ReqId, Request,
};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use tutti_platform::Instant;
use tutti_plugin_types::{
    EditorError, IpcError, IpcResult, PresetError, ResyncKind, SlabError, StateError,
};
use tutti_plugin_types::{MidiIntake, RateRefusal, RateRefusalCause};
use tutti_plugin_types::{ParameterChanges, ParameterPoint, ParameterQueue};
use tutti_units::SampleRate;
use tutti_units::{Normalized, ParamId};

/// How many block periods the bridge thread will wait for a `ProcessAudio`
/// reply before abandoning the block.
///
/// Bounded by the *block period*, not an absolute duration. A bridge thread
/// blocked in `recv_reply` dequeues no further commands — a lane's loop is a single
/// loop — while the audio thread keeps pushing one command per block, so a single
/// slow reply overflows the 128-slot queue and produces sustained silence long
/// after the server recovers. A fixed 500 ms against a 667 µs block (64 frames
/// @ 48 kHz) is a ~750x mismatch, some 750 queued blocks.
///
/// A few periods rather than exactly one: the bridge may still be waiting after
/// the audio thread has moved on, and that reply is simply unmatched by the
/// slab's sequence check. Beyond a few periods it is worthless anyway.
const PROCESS_TIMEOUT_PERIODS: u32 = 4;

/// Floor for the `ProcessAudio` reply timeout. At very short periods the
/// computed timeout drops under the scheduler's own wake latency (~0.7-1 ms
/// measured on this machine), and a bridge thread that times out before the
/// server could realistically have been scheduled would abandon every block on
/// a busy machine. Unlike the audio thread — which genuinely cannot spend 1 ms
/// at 64 frames / 192 kHz — the bridge thread is *not* on a deadline, so a floor
/// is safe here and merely stops it giving up prematurely.
const MIN_PROCESS_TIMEOUT: Duration = Duration::from_millis(2);

/// Ceiling, for the degenerate case of a very large block: past this the reply
/// cannot help any live block and continuing to wait only starves the queue.
const MAX_PROCESS_TIMEOUT: Duration = Duration::from_millis(50);

/// How long [`read_news`] waits for the control connection to say more
/// once it has gone quiet: news the server sent is already in the socket.
const DRAIN_UNSOLICITED: Duration = Duration::from_millis(1);

/// The slice [`Timeouts::rate`](tutti_plugin_types::Timeouts::rate) is waited in, between checks that the bridge is
/// still running.
const RATE_SLICE: Duration = Duration::from_millis(100);

/// How many blocks behind the newest submitted block a queued `Process` may be
/// and still be worth sending.
///
/// **This is the ring depth, not an independent tunable**, but note the `- 1`:
/// a block exactly [`RING_SLOTS`] behind occupies the *same slot* as the newest
/// one, because `slot_for` is `seq % RING_SLOTS`. So the last still-live block
/// is `RING_SLOTS - 1` behind, not `RING_SLOTS`.
///
/// This was `RING_SLOTS`, and being one too lax was not merely wasteful. The
/// server writes and publishes its output *unconditionally* — its `has_input`
/// check gates only the read — so a block admitted here at `RING_SLOTS` behind
/// would write into the very slot the host is reading for the newest block and
/// then stamp that slot's `output_seq` with its own sequence, either tearing
/// the block being read or destroying the evidence for it. Deriving the
/// constant is right; deriving it with the wrong offset still shipped the bug
/// the derivation was meant to prevent.
///
/// It answers a *different* question from [`process_timeout`]: that asks "is this
/// plugin still responding?", this asks "is this reply still wanted?". Capping the
/// timeout low enough to force a catch-up would answer the second by breaking the
/// first, abandoning slow-but-working plugins on every block.
///
/// Cost of dropping: a stateful plugin (delay line, reverb tail) that skips input
/// blocks has its state diverge from a continuous signal, so it glitches on
/// recovery rather than cleanly silencing. Unavoidable without stalling the audio
/// thread, and only when the plugin is already failing to keep up.
const MAX_BEHIND: u64 = RING_SLOTS as u64 - 1;

/// The bridge thread's reply timeout for one block: [`PROCESS_TIMEOUT_PERIODS`]
/// of that block's own period, clamped to
/// [`MIN_PROCESS_TIMEOUT`]..=[`MAX_PROCESS_TIMEOUT`].
///
/// `num_samples` stays raw: it comes off the IPC wire, where the unit mandate
/// stops.
fn process_timeout(num_samples: usize, rate: SampleRate) -> Duration {
    let period = Duration::from_secs_f64(num_samples as f64 / rate.get());
    (period * PROCESS_TIMEOUT_PERIODS).clamp(MIN_PROCESS_TIMEOUT, MAX_PROCESS_TIMEOUT)
}

/// Whether block `seq` is far enough behind the newest submitted block that its
/// reply is provably unwanted — its slab slot has been recycled.
///
/// `newest` is the highest sequence the audio thread has submitted. Saturating
/// rather than plain subtraction only to be total: `newest < seq` cannot happen
/// (the bridge cannot dequeue a block that was never pushed), and treating that
/// impossible case as "not stale" is the safe direction — it sends a block
/// rather than silently dropping a live one.
///
/// No wrap handling, unlike the `u32` `buffer_id` this replaced: a `u64`
/// sequence at ~750 blocks/s takes on the order of 780,000 years to exhaust.
fn is_stale(seq: u64, newest: u64) -> bool {
    newest.saturating_sub(seq) > MAX_BEHIND
}

/// What identifies a request on the audio connection: a block by the sequence
/// it carries, and the `ApplyParameters` that rides beside the blocks by its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioKey {
    Block(u64),
    Params(ReqId),
}

/// What an audio-lane request the lane stopped waiting for still has coming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AbandonedAudio {
    /// A block's `AudioProcessed`: its MIDI is forwarded.
    Block,
    /// The one `ParametersApplied` of an `ApplyParameters`.
    Params,
}

/// The key of the request an audio reply answers: `None` for a reply that
/// answers none (the handshake's `Ready`).
fn audio_key(reply: &AudioReply) -> Option<AudioKey> {
    match reply {
        AudioReply::AudioProcessed { seq, .. } => Some(AudioKey::Block(*seq)),
        AudioReply::ParametersApplied { id } => Some(AudioKey::Params(*id)),
        AudioReply::Ready { .. } => None,
    }
}

/// A server's fault, as the connection-level error it ends the lane with.
fn fault(fault: Fault) -> IpcError {
    IpcError::ProtocolError(match fault {
        Fault::Unknown => "a reply to a request this lane never made",
        Fault::Flooded { .. } => "the server left too many requests unanswered",
    })
}

/// The audio lane's conversation with the server, the cursor of any frame a
/// timeout left half-read, and what the lane carries from one block to the
/// next.
///
/// Owned by the audio lane's thread and passed in: only that thread ever reads
/// or writes it. The [`PartialFrame`] has exactly this scope for a different
/// reason: it describes a position in *this* connection's stream, so it lives
/// and dies with the stream.
pub(super) struct AudioState {
    /// How long a reply that has already started arriving gets to finish when
    /// its request's own budget runs out
    /// ([`Timeouts::drain`](tutti_plugin_types::Timeouts::drain)).
    finish: Duration,
    /// The requests sent and not yet heard the end of, by key.
    conv: Conversation<AudioKey, AbandonedAudio>,
    /// The id of the last request that carried one.
    last_id: u64,
    /// Where the last read stopped, so a timeout resumes rather than restarts.
    partial: PartialFrame,
    /// The parameter changes of blocks skipped as stale, as each parameter's
    /// last value, on frame 0: sent on the first frame of the next block that
    /// goes. A plain list, cleared (its capacity kept) once merged.
    carried: Vec<(ParamId, ParameterPoint)>,
    /// The MIDI of the blocks that go, filtered: once a stale block is
    /// skipped, the rest of every SysEx open in it is not sent, so the
    /// server's reassembler never completes one with a gap.
    intake: MidiIntake,
    /// The audio-response ring's writer: the ring is single-producer by
    /// ownership, and this lane's thread is its producer.
    responses: ResponseWriter,
}

impl AudioState {
    /// Nothing sent on the audio connection, whose begun replies get `finish`
    /// to end and whose thread alone answers the blocks into `responses`.
    pub(super) fn new(finish: Duration, responses: ResponseWriter) -> Self {
        Self {
            finish,
            conv: Conversation::new(),
            last_id: 0,
            partial: PartialFrame::default(),
            carried: Vec::new(),
            intake: MidiIntake::default(),
            responses,
        }
    }

    /// Queues `resp` for the audio thread.
    pub(super) fn respond(&mut self, channels: &Channels, resp: AudioResponse) {
        self.responses.push(resp, channels);
    }

    /// The next id a request of this lane carries.
    fn fresh(&mut self) -> ReqId {
        self.last_id += 1;
        ReqId::new(self.last_id)
    }

    /// Keep a skipped block's parameter changes: each parameter's last value.
    ///
    /// The plugin node sends a parameter only when its value changes, so a
    /// change dropped with its block would never be sent again: the plugin
    /// would hold the value before it until the host's value moved once more.
    /// Bridge thread: may allocate.
    fn carry(&mut self, changes: &ParameterChanges) {
        for queue in &changes.queues {
            let Some(last) = queue.points.last() else {
                continue;
            };
            let point = ParameterPoint {
                sample_offset: 0,
                value: last.value,
            };
            match self.carried.iter_mut().find(|(a, _)| *a == queue.param_id) {
                Some((_, kept)) => *kept = point,
                None => self.carried.push((queue.param_id, point)),
            }
        }
    }

    /// Keep a host write for the block about to be sent, on its first frame:
    /// the parameter's latest value, replacing one kept already.
    /// Bridge thread: may allocate.
    fn carry_write(&mut self, id: ParamId, value: Normalized) {
        let point = ParameterPoint {
            sample_offset: 0,
            value,
        };
        match self.carried.iter_mut().find(|(a, _)| *a == id) {
            Some((_, kept)) => *kept = point,
            None => self.carried.push((id, point)),
        }
    }

    /// Put what [`carry`](Self::carry) and [`carry_write`](Self::carry_write) kept into the block about to be sent,
    /// on its first frame: a parameter the block moves itself keeps its own
    /// points, preceded by the carried value when its first point is later
    /// than frame 0.
    ///
    /// Never grows the block's buffer: it is built
    /// `ParameterChanges::reserved_bridged`, which keeps a point of every
    /// queue (and of every pool block) for the carried value, so the value
    /// takes a real slot (`ParameterChanges::prepend_carried`, counted in the
    /// buffer's `points_overflowed` when it cannot: a buffer built some other
    /// way, full, or one with every queue open). A parameter the block does
    /// not move opens one of the buffer's spare queues. Only the empty
    /// placeholder of a block that had no changes (no queue room at all)
    /// gets queues built here (bridge thread: may allocate; it is dropped on
    /// return).
    fn merge_carried(&mut self, changes: &mut ParameterChanges) {
        let placeholder = changes.capacity() == 0;
        for &(param_id, carried) in &self.carried {
            let i = match changes.queues.iter().position(|q| q.param_id == param_id) {
                Some(i)
                    if changes.queues[i]
                        .points
                        .first()
                        .is_some_and(|p| p.sample_offset > 0) =>
                {
                    i
                }
                Some(_) => continue,
                None if placeholder => {
                    changes
                        .queues
                        .push(ParameterQueue::from_points(param_id, vec![carried]));
                    continue;
                }
                None if changes.open_queue(param_id) => changes.queues.len() - 1,
                // No spare queue: past the queues, so dropped and counted.
                // Here the carried value is the parameter's latest, and it
                // is lost. Unreachable from the delivery: every buffer holds
                // its slot count (the parameters + `SPARE_QUEUES`), and each
                // id a block holds or the bridge carries is one of those
                // slots. No `debug_assert`: a buffer built smaller reaches
                // it, and the test below builds one.
                None => changes.queues.len(),
            };
            let _ = changes.prepend_carried(i, carried);
        }
        self.carried.clear();
    }

    /// What `reply` is, heard at `now`: `Some` when it answers `awaited`,
    /// `None` when it was the late reply of a request this lane gave up on (a
    /// block's is **forwarded, not discarded**: its audio is moot, the slab
    /// slot it names was recycled and the sequence check rejects it, but its
    /// MIDI-out is the plugin's real output and the socket is its only path;
    /// only a block's counts as late).
    ///
    /// # Errors
    ///
    /// A reply to a request this lane never made (a block never sent, a reply
    /// of the other kind) is the server's fault, which ends the connection.
    fn hear(
        &mut self,
        reply: AudioReply,
        awaited: AudioKey,
        channels: &Channels,
    ) -> IpcResult<Option<AudioReply>> {
        let key = audio_key(&reply).ok_or_else(|| fault(Fault::Unknown))?;
        match self.conv.hear(key).map_err(fault)? {
            Heard::Answer if key == awaited => Ok(Some(reply)),
            Heard::Answer => Err(IpcError::ProtocolError(
                "an answer to a request this lane is not waiting for",
            )),
            Heard::Abandoned(AbandonedAudio::Block) => {
                if let AudioReply::AudioProcessed {
                    seq,
                    midi_out,
                    status,
                    refused,
                    ..
                } = reply
                {
                    channels.note_outcome(
                        seq,
                        if refused {
                            BatchOutcome::Refused
                        } else {
                            BatchOutcome::Published
                        },
                    );
                    self.respond(
                        channels,
                        AudioResponse::AudioProcessed {
                            seq,
                            midi_out,
                            status,
                        },
                    );
                    channels.note_late();
                }
                Ok(None)
            }
            Heard::Abandoned(AbandonedAudio::Params) => Ok(None),
        }
    }

    /// Stops waiting for `key`: its reply is late when it comes.
    fn give_up(&mut self, key: AudioKey) -> IpcResult<()> {
        self.conv.abandon(key).map_err(fault)
    }
}

/// The audio reply to the request `key`, waited for within the window it was
/// opened with: `Ok(None)` when it did not come in time, in which case the
/// request is given up on and its reply, when it comes, is late.
///
/// **One deadline for the whole read**: the frames read on the way are inside
/// it. A timeout on a frame boundary is not an error: the stream is
/// synchronised and the session healthy, so the caller answers its own caller
/// and the bridge goes on. A budget that expires **part-way through a frame**
/// gets [`Timeouts::drain`](tutti_plugin_types::Timeouts::drain) to complete
/// *that frame*, and the read ends there: extending the whole read instead let
/// a chatty peer start a new frame at the extended deadline, and a live peer
/// was declared unable to frame. A frame still unfinished after the drain is a
/// peer that cannot frame, and that timeout propagates with the transport's
/// errors (EOF, a frame over the cap), which the lane's loop crashes on.
fn wait_audio(
    stream: &mut ControlStream,
    state: &mut AudioState,
    channels: &Channels,
    key: AudioKey,
) -> IpcResult<Option<AudioReply>> {
    loop {
        let Some(left) = state.conv.left(key, Instant::now()) else {
            return Ok(None);
        };
        match ipc::recv_resumable::<AudioReply>(stream, left, &mut state.partial) {
            Ok(reply) => {
                if let Some(answer) = state.hear(reply, key, channels)? {
                    return Ok(Some(answer));
                }
            }
            Err(IpcError::Timeout { partial: true, .. }) => {
                let frame =
                    ipc::recv_resumable::<AudioReply>(stream, state.finish, &mut state.partial)?;
                let answer = state.hear(frame, key, channels)?;
                if answer.is_none() {
                    state.give_up(key)?;
                }
                return Ok(answer);
            }
            Err(IpcError::Timeout { partial: false, .. }) => {
                state.give_up(key)?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
    }
}

/// What a control request the lane stopped waiting for still has coming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Abandoned {
    /// Nothing is done with it: a query's reply, or the rest of a state.
    Discard,
    /// A rate change's answer, which settles the plugin's rate refusal.
    Rate,
}

/// The control lane's conversation with the server, and the cursor of any frame
/// a timeout left half-read.
///
/// Owned by the control lane's thread and passed in: only that thread ever
/// reads or writes it.
pub(super) struct ControlState {
    /// How long a reply that has already started arriving gets to finish when
    /// its request's own budget runs out
    /// ([`Timeouts::drain`](tutti_plugin_types::Timeouts::drain)).
    ///
    /// Larger than any request's budget, and the split is the point: the
    /// decision to *wait* for a reply is the request's, and is short for a
    /// query, but once bytes of a frame are moving the reply is arriving and
    /// finishing it costs little. A few hundred bytes over a local socket
    /// cannot plausibly take this long without the peer being dead, which the
    /// read reports as EOF rather than as a timeout.
    finish: Duration,
    /// The requests sent and not yet heard the end of, by id.
    conv: Conversation<ReqId, Abandoned>,
    /// The id of the last request sent.
    last_id: u64,
    /// Where the last read stopped, so a timeout resumes rather than restarts.
    partial: PartialFrame,
}

/// What waiting for a control request's answer came to.
enum Waited {
    /// A frame of the answer.
    Reply(ControlReply),
    /// A slice of the wait passed with the request still within its window.
    Pending,
    /// The request is past its window and was given up on.
    GaveUp,
}

impl ControlState {
    /// Nothing sent on the control connection, whose begun replies get `finish`
    /// to end.
    pub(super) fn new(finish: Duration) -> Self {
        Self {
            finish,
            conv: Conversation::new(),
            last_id: 0,
            partial: PartialFrame::default(),
        }
    }

    /// The next id a request of this lane carries.
    fn fresh(&mut self) -> ReqId {
        self.last_id += 1;
        ReqId::new(self.last_id)
    }

    /// Files what `frame` is, heard at `now`: `Some` when it is a frame of the
    /// answer to `awaited`; `None` for news (filed into `inbox`), a sign of
    /// life (the request's window restarts), and the late reply of a request
    /// this lane gave up on.
    ///
    /// # Errors
    ///
    /// A reply to a request this lane never made, or to one that is not the one
    /// waited for, is the server's fault, which ends the connection.
    fn hear(
        &mut self,
        frame: ControlFrame,
        awaited: Option<ReqId>,
        now: Instant,
        inbox: &mut Unsolicited,
    ) -> IpcResult<Option<ControlReply>> {
        match frame {
            ControlFrame::News(news) => {
                file_news(inbox, news);
                Ok(None)
            }
            ControlFrame::Working { id, .. } => {
                // A sign of life for a request given up on, or answered, is a
                // frame in flight when the lane moved on: nothing waits on it.
                self.conv.extend(id, now);
                Ok(None)
            }
            ControlFrame::Ready { .. } => Err(IpcError::ProtocolError(
                "a second handshake on a connection that is in use",
            )),
            ControlFrame::Reply { id, reply } => match self.conv.hear(id).map_err(fault)? {
                Heard::Answer if awaited == Some(id) => Ok(Some(reply)),
                Heard::Answer => Err(IpcError::ProtocolError(
                    "an answer to a request this lane is not waiting for",
                )),
                Heard::Abandoned(Abandoned::Discard) => Ok(None),
                Heard::Abandoned(Abandoned::Rate) => {
                    if let ControlReply::SampleRateSet { rate, refusal } = reply {
                        inbox.late_rate_answer(rate, refusal);
                    }
                    Ok(None)
                }
            },
        }
    }

    /// Stops waiting for `id`: its frames are late when they come.
    fn give_up(&mut self, id: ReqId) -> IpcResult<Waited> {
        self.conv.abandon(id).map_err(fault)?;
        Ok(Waited::GaveUp)
    }
}

/// Sends `body` as a request that expects an answer: the server has `window`
/// to give it (restarting at each sign of life, up to `ceiling` from now).
fn ask(
    stream: &mut ControlStream,
    state: &mut ControlState,
    body: ControlRequest,
    late: Abandoned,
    window: Duration,
    ceiling: Duration,
) -> IpcResult<ReqId> {
    ask_by(stream, state, late, window, ceiling, |stream, id| {
        ipc::send(stream, &Request { id, body })
    })
}

/// [`ask`] with the request written by `send`, for one that travels with more
/// than a frame (a slab's descriptor).
fn ask_by(
    stream: &mut ControlStream,
    state: &mut ControlState,
    late: Abandoned,
    window: Duration,
    ceiling: Duration,
    send: impl FnOnce(&mut ControlStream, ReqId) -> IpcResult<()>,
) -> IpcResult<ReqId> {
    let id = state.fresh();
    state.conv.open(id, late, Instant::now(), window, ceiling);
    send(stream, id)?;
    Ok(id)
}

/// Sends `body` as a request nothing answers.
pub(super) fn tell(
    stream: &mut ControlStream,
    state: &mut ControlState,
    body: ControlRequest,
) -> IpcResult<()> {
    let id = state.fresh();
    ipc::send(stream, &Request { id, body })
}

/// The next frame of the answer to request `id`, within the window it was
/// opened with: the news read on the way is filed into `inbox`.
///
/// With a `slice` the wait returns [`Waited::Pending`] each time a slice passes
/// with the request still inside its window, so the caller can look at whether
/// to go on; a slice that expires part-way through a frame resumes it, so
/// slicing never unframes the stream. See [`wait_audio`] for the rule on a
/// budget that expires part-way through a frame.
fn wait_for(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    state: &mut ControlState,
    id: ReqId,
    slice: Option<Duration>,
) -> IpcResult<Waited> {
    loop {
        let Some(left) = state.conv.left(id, Instant::now()) else {
            return Ok(Waited::GaveUp);
        };
        let wait = slice.map_or(left, |slice| slice.min(left));
        match ipc::recv_resumable::<ControlFrame>(stream, wait, &mut state.partial) {
            Ok(frame) => {
                if let Some(reply) = state.hear(frame, Some(id), Instant::now(), inbox)? {
                    return Ok(Waited::Reply(reply));
                }
            }
            Err(IpcError::Timeout { partial: true, .. }) => {
                let frame =
                    ipc::recv_resumable::<ControlFrame>(stream, state.finish, &mut state.partial)?;
                return match state.hear(frame, Some(id), Instant::now(), inbox)? {
                    Some(reply) => Ok(Waited::Reply(reply)),
                    None => state.give_up(id),
                };
            }
            Err(IpcError::Timeout { partial: false, .. }) => {
                if slice.is_some() && !state.conv.is_due(id, Instant::now()) {
                    return Ok(Waited::Pending);
                }
                return state.give_up(id);
            }
            Err(e) => return Err(e),
        }
    }
}

/// A query: `body` sent, and its one answer waited for within the lifecycle's
/// query window; `None` when it did not come in time, which the query answers
/// as "cannot be asked" (and the reply is late when it comes).
fn query(
    lane: &mut Lane<ControlState>,
    inbox: &mut Unsolicited,
    lifecycle: &Lifecycle,
    body: ControlRequest,
) -> IpcResult<Option<ControlReply>> {
    query_by(lane, inbox, lifecycle, |stream, id| {
        ipc::send(stream, &Request { id, body })
    })
}

/// [`query`] with the request written by `send`.
fn query_by(
    lane: &mut Lane<ControlState>,
    inbox: &mut Unsolicited,
    lifecycle: &Lifecycle,
    send: impl FnOnce(&mut ControlStream, ReqId) -> IpcResult<()>,
) -> IpcResult<Option<ControlReply>> {
    let window = lifecycle.timeouts().param;
    let Lane { stream, state } = lane;
    let id = ask_by(stream, state, Abandoned::Discard, window, window, send)?;
    match wait_for(stream, inbox, state, id, None)? {
        Waited::Reply(reply) => Ok(Some(reply)),
        Waited::Pending | Waited::GaveUp => Ok(None),
    }
}

/// One connection of the bridge: its stream and its lane's state
/// (`O`: [`AudioState`] on the audio lane, [`ControlState`] on the control lane).
pub(super) struct Lane<O> {
    pub(super) stream: ControlStream,
    pub(super) state: O,
}

/// The audio lane's command, run on its thread: **the lane carries `ProcessAudio`
/// and `Reset`** (the server's render thread answers them), in order and one
/// at a time. A block is never queued behind the control lane (the server's
/// main thread, which may be busy with an editor), and what a block's reply
/// owes ([`AudioState`]) is paired on its own stream.
pub(super) fn handle_audio(
    cmd: AudioCommand,
    lane: &mut Lane<AudioState>,
    channels: &Channels,
    payloads: &PayloadPool,
    lifecycle: &Lifecycle,
) -> IpcResult<()> {
    let Lane { stream, state } = lane;
    match cmd {
        AudioCommand::Process(mut payload) => {
            let seq = payload.seq;
            let num_samples = payload.num_samples;

            // Skip a block whose slab slot the host has already recycled: no
            // reply to it can be consumed (see `MAX_BEHIND`). Recycle the
            // payload so the pool doesn't leak, and push nothing — the audio
            // thread stopped expecting this block two blocks ago.
            if is_stale(seq, channels.newest_submitted()) {
                channels.note_skipped();
                channels.note_outcome(seq, BatchOutcome::Skipped);
                // Its MIDI is lost, counted, and so is the rest of every
                // SysEx open in it: the server's reassembler would otherwise
                // complete it from the next block's packets, with these
                // missing. (No wire change: a gap is never sent.)
                for e in &payload.midi_events {
                    state.intake.admit(e);
                }
                state.intake.lost();
                channels.note_dropped_midi(payload.midi_events.len() as u64);
                state.carry(&payload.param_changes);
                payloads.return_params(core::mem::take(&mut payload.param_changes));
                payloads.recycle(payload);
                channels.note_handled(seq);
                return Ok(());
            }

            // What the host wrote since the last block, on its first frame.
            channels.drain_host_edits(|id, value| state.carry_write(id, value));
            if !state.carried.is_empty() {
                state.merge_carried(&mut payload.param_changes);
            }
            let before = payload.midi_events.len();
            payload.midi_events.retain(|e| state.intake.admit(e));
            channels.note_dropped_midi((before - payload.midi_events.len()) as u64);
            let msg = AudioRequest::ProcessAudio(ProcessAudioData {
                seq: payload.seq,
                num_samples: payload.num_samples,
                midi_events: payload.midi_events.clone(),
                param_changes: core::mem::take(&mut payload.param_changes),
                note_expression: core::mem::take(&mut payload.note_expression),
                chords: core::mem::take(&mut payload.chords),
                scales: core::mem::take(&mut payload.scales),
                expr_texts: core::mem::take(&mut payload.expr_texts),
                expr_ints: core::mem::take(&mut payload.expr_ints),
                transport: payload.transport,
                transport_changes: core::mem::take(&mut payload.transport_changes),
            });

            payloads.recycle(payload);

            let timeout = channels
                .reply_wait()
                .unwrap_or_else(|| process_timeout(num_samples, channels.sample_rate()));
            let key = AudioKey::Block(seq);
            state
                .conv
                .open(key, AbandonedAudio::Block, Instant::now(), timeout, timeout);
            let sent = ipc::send(stream, &msg);
            // The parameter-change buffer goes back to the audio thread's free
            // list once its contents are on the wire (sent or not: a failed
            // send crashes the bridge below, and the buffer is no use to it).
            if let AudioRequest::ProcessAudio(data) = msg {
                payloads.return_params(data.param_changes);
            }
            sent?;

            // Replies of earlier blocks the lane gave up on are forwarded as they
            // are read (`AudioState::hear`); then this block's own, within its
            // budget.
            match wait_audio(stream, state, channels, key)? {
                // **A missed block budget costs the block, not the connection.**
                //
                // [`process_timeout`] is a few block periods with a 2 ms floor,
                // and it exists to stop the bridge thread blocking while the
                // audio thread keeps queueing. When the timeout was an error
                // out of the handler, the loop treated it as connection-level and
                // `crash()`ed: one reply that lost a 2 ms race against the
                // scheduler latched a permanent crash for a server still
                // running and publishing correct audio. Whether a working
                // plugin was declared dead was a property of the machine.
                //
                // The reply is late, not absent: the lane gave up on the block's
                // key, so the reply is recognised as that block's when it comes.
                // The host needs no notification to fall back to silence (the
                // server has not published, so the slab's sequence check fails
                // for this block on its own); the response keeps the
                // abandonment visible and stops the queue running a block
                // behind.
                None => {
                    channels.note_abandoned();
                    state.respond(channels, AudioResponse::Error { seq: Some(seq) });
                }
                Some(reply) => {
                    match reply {
                        // A reply to a block not yet sent is a peer making
                        // fates up, not an answer.
                        AudioReply::AudioProcessed { seq: replied, .. } if replied > seq => {
                            return Err(IpcError::ProtocolError(
                                "an AudioProcessed for a block that was never sent",
                            ));
                        }
                        // The wire adapter already decoded the list on the
                        // bridge thread (off-RT) and refused one past the cap;
                        // the RT thread only drains it. Whether the *audio* is
                        // there is settled by the slab's sequence numbers,
                        // which the server published before sending this; the
                        // echoed `seq` and `refused` are the chunk's fate for
                        // the batcher's accounting (late, refused, skipped or
                        // unanswered) and for ordering.
                        AudioReply::AudioProcessed {
                            seq,
                            midi_out,
                            status,
                            refused,
                            ..
                        } => {
                            channels.note_outcome(
                                seq,
                                if refused {
                                    BatchOutcome::Refused
                                } else {
                                    BatchOutcome::Published
                                },
                            );
                            state.respond(
                                channels,
                                AudioResponse::AudioProcessed {
                                    seq,
                                    midi_out,
                                    status,
                                },
                            );
                        }
                        // Any other kind is no answer to a block: counting it
                        // as one would keep a hung plugin looking alive.
                        other => return Err(unexpected_message("AudioProcessed", &other)),
                    }
                    // Counted after the push: a reader that sees the count
                    // finds the reply queued.
                    channels.note_replied();
                }
            }
            // Dealt with once the server has answered it (or its budget has
            // passed): a control command stamped after this block is sent
            // after the server has had the block, as it was when one thread
            // read the reply before it took the next command.
            channels.note_handled(seq);
        }
        AudioCommand::Reset => {
            ipc::send(stream, &AudioRequest::Reset)?;
        }
        AudioCommand::Params { done } => {
            // Taken now, as this lane dequeues it: every block queued before it
            // has taken what was written before that block, and what is taken
            // here is newer than all of it.
            let changes = host_edit_changes(channels);
            if changes.is_empty() {
                done.send(true);
                return Ok(());
            }
            // On the block connection, behind every block pushed before it, so
            // the server has those blocks' writes before these.
            let id = state.fresh();
            let key = AudioKey::Params(id);
            let window = lifecycle.timeouts().param_wait();
            state
                .conv
                .open(key, AbandonedAudio::Params, Instant::now(), window, window);
            ipc::send(
                stream,
                &AudioRequest::ApplyParameters {
                    id,
                    param_changes: changes,
                },
            )?;
            let applied = wait_audio(stream, state, channels, key)?;
            done.send(matches!(
                applied,
                Some(AudioReply::ParametersApplied { .. })
            ));
        }
        AudioCommand::Settle { done } => {
            // Two fences, not one. A fence's reply wakes its waiter before the
            // control lane hands the events it read meanwhile to the listener
            // (after the turn); the second fence's turn starts after that
            // hand-off, so its reply means the first's events were delivered.
            // The settings requested before it first.
            while let Some(setting) = channels.take_request(None) {
                apply_setting(channels, lifecycle, setting);
            }
            for _ in 0..2 {
                apply_setting(channels, lifecycle, Setting::Fence);
            }
            done.send(());
        }
    }
    Ok(())
}

/// Applies a setting a caller requested ([`Channels::request_setting`]), ahead
/// of the blocks submitted after the request: the blocks before it are sent,
/// and the ones after wait until the control lane has had the server apply it. The setting goes into the control
/// lane's latest-value slot, not its queue, so a full queue cannot lose it; the
/// control lane answers `applied` (the server answers that lane, not the audio
/// one).
///
/// The wait ends when the control lane answers, when the bridge stops or
/// crashes (the lane's end drops the setting's reply, so the wait sees it
/// disconnect at once), or at `settle_wait()`. A change the lane gives up on at
/// the deadline is counted ([`Channels::note_unapplied`]) and logged, and the
/// blocks go on: a lane that cannot answer in that time is the control lane's
/// to report.
pub(super) fn apply_setting(channels: &Channels, lifecycle: &Lifecycle, setting: Setting) {
    let what = match setting {
        Setting::SampleRate(_) => "a rate change",
        Setting::RenderMode(_) => "a render mode",
        Setting::Fence => "a settle",
    };
    let (applied, reply) = super::ask::ask::<()>();
    channels.put_setting(setting, reply);
    if let Err(RecvTimeoutError::Timeout) =
        applied.recv_timeout_borrowed(lifecycle.timeouts().settle_wait())
    {
        channels.note_unapplied();
        tracing::warn!(
            "the plugin bridge gave up holding blocks for {what}: \
             the plugin did not apply it in time"
        );
    }
}

/// A round trip that proves the server has handled every request sent before
/// it: it answers this lane in order, and `GetCurrentPreset` is a cheap
/// request every server answers. A reply that does not come in time is late
/// when it comes and does not hold the caller longer.
fn fence(
    lane: &mut Lane<ControlState>,
    inbox: &mut Unsolicited,
    lifecycle: &Lifecycle,
) -> IpcResult<()> {
    query(lane, inbox, lifecycle, ControlRequest::GetCurrentPreset)?;
    Ok(())
}

/// The host's writes no block has taken, as one change per parameter on the first
/// frame: what a flush sends. Taken by the audio lane's thread.
pub(super) fn host_edit_changes(channels: &Channels) -> ParameterChanges {
    let taken = channels.take_host_edits();
    let mut changes = ParameterChanges::reserved(taken.len(), 1);
    for (id, value) in &taken {
        let _ = changes.add_change(*id, 0, value.get());
    }
    changes
}

/// The control lane's command, run on its thread: **every request but the
/// blocks'**, and the plugin's unsolicited news (the server's main thread).
pub(super) fn handle_control(
    cmd: ControlCommand,
    lane: &mut Lane<ControlState>,
    inbox: &mut Unsolicited,
    lifecycle: &Lifecycle,
) -> IpcResult<()> {
    match cmd {
        ControlCommand::SetAutomationState { mode } => {
            tell(
                &mut lane.stream,
                &mut lane.state,
                ControlRequest::SetAutomationState { mode },
            )?;
        }
        ControlCommand::SetTrackInfo { info } => {
            tell(
                &mut lane.stream,
                &mut lane.state,
                ControlRequest::SetTrackInfo { info: *info },
            )?;
        }
        ControlCommand::SaveState {
            state_use,
            flush,
            reply,
        } => {
            // The host's writes no block carried are applied first, so the state
            // holds them: this lane waits for the audio lane, not the asker.
            if let Some(applied) = flush {
                let _ = super::await_reply(applied, lifecycle.timeouts().param_wait());
            }
            let Lane { stream, state } = lane;
            let window = lifecycle.timeouts().state;
            // The server's `Working` while it saves restarts the window, up to the
            // ceiling.
            let id = ask(
                stream,
                state,
                ControlRequest::SaveState { state_use },
                Abandoned::Discard,
                window,
                lifecycle.timeouts().state_ceiling(),
            )?;
            reply.send(recv_state(stream, inbox, state, id, window)?);
        }
        ControlCommand::LoadState {
            state_use,
            len,
            blob,
            reply,
        } => {
            let body = ControlRequest::LoadState {
                state_use,
                len,
                blob: blob.wire(),
            };
            let Lane { stream, state } = lane;
            let window = lifecycle.timeouts().state;
            let id = state.fresh();
            state.conv.open(
                id,
                Abandoned::Discard,
                Instant::now(),
                window,
                lifecycle.timeouts().state_ceiling(),
            );
            // Held until the answer: where the object is read by name, it stays
            // until then.
            let _lease = ipc::send_blob(stream, id, body, blob)?;
            // Wait for the answer. Answering the caller straight after the write
            // would report that the request had been *sent*, never whether the
            // plugin accepted it.
            //
            // A timeout here is **not** connection-level: the socket is
            // synchronised and the session is healthy, the plugin simply has not
            // answered yet.
            // Propagating would `crash()` it, and the caller would receive
            // `Stalled` (which says "the session is fine, retrying is
            // reasonable") against a session that had just been torn down.
            // The answer is late when it comes, and recognised by its id.
            let value = match wait_for(stream, inbox, state, id, None)? {
                Waited::Reply(ControlReply::StateLoaded { error }) => error.map_or(Ok(()), Err),
                Waited::Reply(_) => Err(StateError::UnexpectedReply),
                Waited::Pending | Waited::GaveUp => Err(StateError::Stalled { after: window }),
            };
            reply.send(value);
        }
        ControlCommand::GetParameterList { reply } => {
            let value = match query(lane, inbox, lifecycle, ControlRequest::GetParameterList)? {
                Some(ControlReply::ParameterList { parameters }) => Some(parameters),
                _ => None,
            };
            reply.send(value);
        }
        ControlCommand::GetPresetList { reply } => {
            let value = match query(lane, inbox, lifecycle, ControlRequest::GetPresetList)? {
                Some(ControlReply::PresetList { presets }) => Some(presets),
                _ => None,
            };
            reply.send(value);
        }
        ControlCommand::LoadPreset { id, reply } => {
            // No answer within the window: the plugin was asked and may have
            // loaded it, which is not a refusal.
            let loaded = match query(lane, inbox, lifecycle, ControlRequest::LoadPreset { id })? {
                Some(ControlReply::PresetLoaded { ok: true }) => Ok(()),
                Some(_) => Err(PresetError::Refused),
                None => Err(PresetError::Stalled {
                    after: lifecycle.timeouts().param,
                }),
            };
            reply.send(loaded);
        }
        ControlCommand::SetupSharedMemory {
            slab,
            layout,
            reply,
        } => {
            // The server's refusal (it could not open the slab) is an `Error`
            // answer: no reason to wait out the deadline for a `Ready` that is
            // not coming.
            let ok = matches!(
                query_by(lane, inbox, lifecycle, |stream, id| {
                    ipc::send_setup(stream, id, layout, &slab)
                })?,
                Some(ControlReply::SharedMemoryReady)
            );
            reply.send(ok);
        }
        ControlCommand::GetCurrentPreset { reply } => {
            let value = match query(lane, inbox, lifecycle, ControlRequest::GetCurrentPreset)? {
                Some(ControlReply::CurrentPreset { id }) => id,
                _ => None,
            };
            reply.send(value);
        }
        ControlCommand::GetParameter { param_id, reply } => {
            let value = match query(
                lane,
                inbox,
                lifecycle,
                ControlRequest::GetParameter { param_id },
            )? {
                Some(ControlReply::ParameterValue { value }) => value,
                _ => None,
            };
            reply.send(value);
        }
        ControlCommand::GetParameterText {
            param_id,
            value,
            reply,
        } => {
            let text = match query(
                lane,
                inbox,
                lifecycle,
                ControlRequest::GetParameterText { param_id, value },
            )? {
                Some(ControlReply::ParameterText { text }) => text,
                _ => None,
            };
            reply.send(text);
        }
        ControlCommand::GetParameterValueFromText {
            param_id,
            text,
            reply,
        } => {
            let value = match query(
                lane,
                inbox,
                lifecycle,
                ControlRequest::GetParameterValueFromText { param_id, text },
            )? {
                Some(ControlReply::ParameterValueFromText { value }) => value,
                _ => None,
            };
            reply.send(value);
        }
        ControlCommand::GetParameterPlain {
            param_id,
            value,
            reply,
        } => {
            let plain = match query(
                lane,
                inbox,
                lifecycle,
                ControlRequest::GetParameterPlain { param_id, value },
            )? {
                Some(ControlReply::ParameterPlain { plain }) => plain,
                _ => None,
            };
            reply.send(plain);
        }
        ControlCommand::OpenEditor {
            parent,
            scale,
            reply,
        } => {
            let window = lifecycle.timeouts().editor;
            let Lane { stream, state } = lane;
            let id = ask(
                stream,
                state,
                ControlRequest::OpenEditor { parent, scale },
                Abandoned::Discard,
                window,
                window,
            )?;
            let value = match wait_for(stream, inbox, state, id, None)? {
                Waited::Reply(ControlReply::EditorOpened { result }) => result,
                Waited::Reply(ControlReply::Error { error }) => Err(EditorError::Server(error)),
                Waited::Reply(_) => Err(EditorError::UnexpectedReply),
                Waited::Pending | Waited::GaveUp => Err(EditorError::NoAnswer { after: window }),
            };
            reply.send(value);
        }
        ControlCommand::CloseEditor => {
            tell(
                &mut lane.stream,
                &mut lane.state,
                ControlRequest::CloseEditor,
            )?;
        }
        ControlCommand::SetEditorSize { size, reply } => {
            let window = lifecycle.timeouts().editor;
            let Lane { stream, state } = lane;
            let id = ask(
                stream,
                state,
                ControlRequest::SetEditorSize { size },
                Abandoned::Discard,
                window,
                window,
            )?;
            let value = match wait_for(stream, inbox, state, id, None)? {
                Waited::Reply(ControlReply::EditorSized { result }) => result,
                Waited::Reply(ControlReply::Error { error }) => Err(EditorError::Server(error)),
                Waited::Reply(_) => Err(EditorError::UnexpectedReply),
                Waited::Pending | Waited::GaveUp => Err(EditorError::NoAnswer { after: window }),
            };
            reply.send(value);
        }
        ControlCommand::GetParameterNormalized {
            param_id,
            plain,
            reply,
        } => {
            let value = match query(
                lane,
                inbox,
                lifecycle,
                ControlRequest::GetParameterNormalized { param_id, plain },
            )? {
                Some(ControlReply::ParameterNormalized { value }) => value,
                _ => None,
            };
            reply.send(value);
        }
    }
    Ok(())
}

/// A setting the audio lane holds its blocks for, applied on the control
/// lane's thread: sent, and answered `applied` once the server has taken it.
pub(super) fn handle_setting(
    setting: Setting,
    applied: Reply<()>,
    lane: &mut Lane<ControlState>,
    inbox: &mut Unsolicited,
    lifecycle: &Lifecycle,
) -> IpcResult<()> {
    match setting {
        Setting::SampleRate(rate) => {
            // Wait for the answer: the plugin's re-activation is the slowest
            // thing the bridge asks of it, and the requests queued behind it
            // on this lane wait for it. The audio lane does not: its blocks
            // are the server's render thread's.
            //
            // Waited in slices, so a shutdown (dropping the node joins this
            // thread) ends the wait within one slice rather than after
            // `Timeouts::rate`.
            let budget = lifecycle.timeouts().rate;
            let Lane { stream, state } = lane;
            let id = ask(
                stream,
                state,
                ControlRequest::SetSampleRate { rate },
                Abandoned::Rate,
                budget,
                budget,
            )?;
            loop {
                match wait_for(stream, inbox, state, id, Some(RATE_SLICE))? {
                    Waited::Pending => {
                        if !lifecycle.is_running() {
                            return Ok(());
                        }
                    }
                    // No answer is a refusal, not a dead connection: the plugin
                    // may still be re-activating. Its late answer is recognised
                    // by its id when it arrives (`Abandoned::Rate`) and settles the
                    // state.
                    Waited::GaveUp => {
                        apply_rate_reply(
                            lifecycle,
                            inbox,
                            rate,
                            Some(RateRefusalCause::NoAnswer { after: budget }),
                        );
                        break;
                    }
                    // A refusal is not a failure of the connection: the plugin
                    // stays loaded, the server renders silence for it, and the
                    // next rate change retries it.
                    Waited::Reply(ControlReply::SampleRateSet { rate, refusal }) => {
                        // Recorded before the blocks go on, a refusal and its
                        // clearing alike: a block that finds the old refusal
                        // would latch it (an offline fork's wait does).
                        apply_rate_reply(lifecycle, inbox, rate, refusal);
                        break;
                    }
                    Waited::Reply(other) => {
                        return Err(IpcError::UnexpectedMessage {
                            expected: "SampleRateSet",
                            got: format!("{other:?}").into(),
                        });
                    }
                }
            }
            applied.send(());
        }
        Setting::RenderMode(mode) => {
            tell(
                &mut lane.stream,
                &mut lane.state,
                ControlRequest::SetRenderMode { mode },
            )?;
            fence(lane, inbox, lifecycle)?;
            applied.send(());
        }
        Setting::Fence => {
            fence(lane, inbox, lifecycle)?;
            applied.send(());
        }
    }
    Ok(())
}

/// Reads what the plugin said unprompted off the control connection: each
/// frame is filed into `inbox` as it is read, and a frame that answers nothing
/// the lane is waiting for is the server's fault.
pub(super) fn read_news(lane: &mut Lane<ControlState>, inbox: &mut Unsolicited) -> IpcResult<()> {
    let Lane { stream, state } = lane;
    loop {
        match ipc::recv_resumable::<ControlFrame>(stream, DRAIN_UNSOLICITED, &mut state.partial) {
            Ok(frame) => {
                state.hear(frame, None, Instant::now(), inbox)?;
            }
            Err(IpcError::Timeout { partial: true, .. }) => {
                let frame =
                    ipc::recv_resumable::<ControlFrame>(stream, state.finish, &mut state.partial)?;
                state.hear(frame, None, Instant::now(), inbox)?;
            }
            Err(IpcError::Timeout { partial: false, .. }) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// Takes the plugin's state from the answer to request `id`: the
/// `StateReady` frame and the object that followed it.
///
/// Returns the caller's `IpcResult` rather than propagating, because none of
/// the ways this fails is a connection failure: an over-limit state, an object
/// of another length than declared and a late answer are all *this plugin's*
/// problem, and crashing the bridge for any of them would take down a session
/// whose socket is fine ([`StateError::Stalled`] promises the caller a healthy
/// session to retry against). Only a genuine transport error (`?`) still
/// propagates, and that is the one case where the stream really is unusable.
///
/// The object was read off the stream with its frame, so a refusal here leaves
/// the stream in step. An answer that arrives after the lane gave up is
/// recognised by its id and dropped ([`Abandoned::Discard`]).
fn recv_state(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    state: &mut ControlState,
    id: ReqId,
    window: Duration,
) -> IpcResult<std::result::Result<Vec<u8>, StateError>> {
    let msg = match wait_for(stream, inbox, state, id, None)? {
        Waited::Reply(msg) => msg,
        Waited::Pending | Waited::GaveUp => return Ok(Err(StateError::Stalled { after: window })),
    };
    let ControlReply::StateReady { len, .. } = msg else {
        // A reply of another type is the whole answer (the server's `Error`
        // when the plugin cannot save); the state is not coming.
        return Ok(Err(StateError::Rejected(format!(
            "unexpected reply while reading plugin state: {msg:?}"
        ))));
    };
    let Some(object) = state.partial.take_blob() else {
        return Ok(Err(StateError::Rejected(
            "the plugin's state object did not arrive".to_owned(),
        )));
    };
    Ok(object
        .read(len, tutti_plugin_types::MAX_STATE_BYTES)
        .map_err(|e| match e {
            SlabError::BlobTooLarge { declared, limit } => StateError::TooLarge {
                bytes: usize::try_from(declared).unwrap_or(usize::MAX),
                limit,
            },
            other => StateError::Rejected(other.to_string()),
        }))
}

/// Record the plugin's answer to a rate change: the refusal while it refuses,
/// cleared (and heard, if there was one) when it takes the rate.
pub(super) fn apply_rate_reply(
    lifecycle: &Lifecycle,
    inbox: &mut Unsolicited,
    rate: SampleRate,
    refusal: Option<RateRefusalCause>,
) {
    match refusal {
        Some(cause) => {
            let refusal = RateRefusal::new(rate, cause);
            lifecycle.set_rate_refusal(Some(refusal.clone()));
            inbox.rate_refusal(Some(refusal));
        }
        None => {
            if lifecycle.is_rate_refused() {
                lifecycle.set_rate_refusal(None);
                inbox.rate_refusal(None);
            }
        }
    }
}

/// Puts an unsolicited event into `inbox`.
fn file_news(inbox: &mut Unsolicited, news: News) {
    match news {
        News::LatencyChanged { samples } => inbox.latency(samples),
        News::TailChanged { tail } => inbox.tail(tail),
        News::ParameterChanged { edit } => inbox.param(edit),
        News::PluginParamValuesChanged => inbox.resync(ResyncKind::ParamValues),
        News::PluginParamTitlesChanged => inbox.resync(ResyncKind::ParamTitles),
        News::PluginIoChanged { layout } => {
            if let Some(layout) = layout {
                inbox.io_layout(layout);
            }
            inbox.resync(ResyncKind::Io);
        }
        News::PluginReloaded => inbox.resync(ResyncKind::Reloaded),
        News::ProcessRequested => inbox.process_requested(),
        News::TransportRequested { request } => inbox.transport(request),
        // The server's editor is the remote editor's to hear of: no event of
        // the plugin's, so the lifecycle holds them.
        News::EditorWantsSize { size } => inbox.editor_wants_size(size),
        News::EditorClosed => inbox.editor_closed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::Timeouts;
    use tutti_units::ParamId;

    fn processed(seq: u64) -> AudioReply {
        AudioReply::AudioProcessed {
            latency_us: 0,
            seq,
            midi_out: Default::default(),
            status: Default::default(),
            refused: false,
        }
    }

    fn state_ready(id: u64) -> ControlFrame {
        ControlFrame::Reply {
            id: ReqId::new(id),
            reply: ControlReply::StateReady {
                len: 0,
                blob: tutti_plugin_ipc_ffi::Blob::create(&[])
                    .expect("an empty blob")
                    .wire(),
            },
        }
    }

    fn value(id: u64) -> ControlFrame {
        ControlFrame::Reply {
            id: ReqId::new(id),
            reply: ControlReply::ParameterValue {
                value: Some(tutti_units::Normalized::of(0.25)),
            },
        }
    }

    const WINDOW: Duration = Duration::from_secs(5);

    fn audio_state() -> (AudioState, super::super::channels::Replies, Channels) {
        let (writer, replies, _lost) = response_ring();
        let channels = Channels::new(SampleRate::SR_48K, std::sync::Arc::default())
            .expect("the channels")
            .0;
        (
            AudioState::new(Timeouts::default().drain, writer),
            replies,
            channels,
        )
    }

    /// Opens `key` on `state` and gives up on it: its reply is late.
    fn abandoned(state: &mut AudioState, key: AudioKey, late: AbandonedAudio) {
        state.conv.open(key, late, Instant::now(), WINDOW, WINDOW);
        state.give_up(key).expect("room to give up");
    }

    /// **An abandoned block's reply is forwarded and counted late, and the next
    /// block's answer is its own**: the reply is recognised by its sequence, so
    /// it is neither taken as the answer nor dropped, whatever order the two
    /// arrive in.
    #[test]
    fn a_late_block_reply_is_forwarded_and_counted_late() {
        let (mut state, mut replies, channels) = audio_state();
        abandoned(&mut state, AudioKey::Block(3), AbandonedAudio::Block);
        state.conv.open(
            AudioKey::Block(4),
            AbandonedAudio::Block,
            Instant::now(),
            WINDOW,
            WINDOW,
        );

        let late = state.hear(processed(3), AudioKey::Block(4), &channels);
        assert!(matches!(late, Ok(None)), "late, not the answer");
        assert!(matches!(
            replies.pop(),
            Some(AudioResponse::AudioProcessed { seq: 3, .. })
        ));
        assert_eq!(channels.block_counts().late, 1);

        let answer = state.hear(processed(4), AudioKey::Block(4), &channels);
        assert!(matches!(
            answer,
            Ok(Some(AudioReply::AudioProcessed { seq: 4, .. }))
        ));
        assert_eq!(channels.block_counts().late, 1, "an answer is not late");
    }

    /// **A late acknowledgement of `ApplyParameters` is dropped without
    /// answering anything**, and is not counted as a late block.
    #[test]
    fn a_late_apply_parameters_acknowledgement_is_dropped() {
        let (mut state, mut replies, channels) = audio_state();
        let id = state.fresh();
        abandoned(&mut state, AudioKey::Params(id), AbandonedAudio::Params);
        let heard = state.hear(
            AudioReply::ParametersApplied { id },
            AudioKey::Block(1),
            &channels,
        );
        assert!(matches!(heard, Ok(None)));
        assert!(replies.pop().is_none());
        assert_eq!(channels.block_counts().late, 0);
    }

    /// **A reply to a request this lane never made is the server's fault**: a
    /// block never sent, an acknowledgement nobody asked for.
    #[test]
    fn a_reply_to_a_request_never_made_is_a_fault() {
        let (mut state, _replies, channels) = audio_state();
        state.conv.open(
            AudioKey::Block(1),
            AbandonedAudio::Block,
            Instant::now(),
            WINDOW,
            WINDOW,
        );
        for reply in [
            processed(9),
            AudioReply::ParametersApplied { id: ReqId::new(77) },
        ] {
            assert!(
                matches!(
                    state.hear(reply, AudioKey::Block(1), &channels),
                    Err(IpcError::ProtocolError(_))
                ),
                "a reply to nothing this lane sent was accepted"
            );
        }
    }

    fn control_state() -> ControlState {
        ControlState::new(Timeouts::default().drain)
    }

    fn open(state: &mut ControlState, id: u64, late: Abandoned) -> ReqId {
        let id = ReqId::new(id);
        state.conv.open(id, late, Instant::now(), WINDOW, WINDOW);
        id
    }

    /// **A query's late reply and a stalled state's late answer are dropped by
    /// their ids, and the answer to the next request is its own**, however the
    /// frames interleave.
    #[test]
    fn late_control_frames_are_dropped_by_id_and_the_answer_is_its_own() {
        let mut state = control_state();
        let mut inbox = Unsolicited::new();
        let query = open(&mut state, 1, Abandoned::Discard);
        let save = open(&mut state, 2, Abandoned::Discard);
        state.give_up(query).unwrap();
        state.give_up(save).unwrap();
        let next = open(&mut state, 3, Abandoned::Discard);

        assert!(matches!(
            state.hear(value(1), Some(next), Instant::now(), &mut inbox),
            Ok(None)
        ));
        assert!(matches!(
            state.hear(state_ready(2), Some(next), Instant::now(), &mut inbox),
            Ok(None)
        ));
        let list = ControlFrame::Reply {
            id: next,
            reply: ControlReply::ParameterList {
                parameters: Vec::new(),
            },
        };
        assert!(matches!(
            state.hear(list, Some(next), Instant::now(), &mut inbox),
            Ok(Some(ControlReply::ParameterList { .. }))
        ));
        assert!(
            state
                .hear(state_ready(2), None, Instant::now(), &mut inbox)
                .is_err(),
            "the state's answer had ended"
        );
    }

    /// **A state request slower than its window lives on the server's beats, under
    /// the ceiling, and without them is due at the window**: constructed instants,
    /// no clock. The window and the ceiling are the ones the state handlers open
    /// a request with.
    #[test]
    fn a_state_request_slower_than_its_window_lives_on_beats_under_the_ceiling() {
        let timeouts = Timeouts::default();
        let (window, ceiling) = (timeouts.state, timeouts.state_ceiling());
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let working = |id| ControlFrame::Working {
            id,
            stage: crate::Working::Opening,
        };
        let open = |state: &mut ControlState| {
            let id = ReqId::new(1);
            state
                .conv
                .open(id, Abandoned::Discard, start, window, ceiling);
            id
        };
        let mut inbox = Unsolicited::new();

        // Without beats: due at the window.
        let mut silent = control_state();
        let id = open(&mut silent);
        assert!(!silent.conv.is_due(id, at(window.as_secs() - 1)));
        assert!(silent.conv.is_due(id, at(window.as_secs())));

        // With a beat every few seconds: alive well past the window, due a window
        // after the last beat, and never past the ceiling.
        let mut beaten = control_state();
        let id = open(&mut beaten);
        for step in 1..=5u64 {
            let now = at(step * (window.as_secs() - 2));
            assert!(!beaten.conv.is_due(id, now), "beat {step} came in time");
            assert!(matches!(
                beaten.hear(working(id), Some(id), now, &mut inbox),
                Ok(None)
            ));
        }
        assert!(window.as_secs() * 3 < ceiling.as_secs());
        let last = at(5 * (window.as_secs() - 2));
        assert!(
            !beaten
                .conv
                .is_due(id, last + window - Duration::from_secs(1))
        );
        assert!(beaten.conv.is_due(id, last + window));
        assert!(
            beaten
                .hear(working(id), Some(id), at(ceiling.as_secs() - 1), &mut inbox)
                .is_ok()
        );
        assert!(
            beaten.conv.is_due(id, at(ceiling.as_secs())),
            "the ceiling stands"
        );
    }

    /// **The server's `Error` answers the request it names**, like any reply,
    /// and no other.
    #[test]
    fn an_error_answers_only_its_own_request() {
        let mut state = control_state();
        let mut inbox = Unsolicited::new();
        let first = open(&mut state, 1, Abandoned::Discard);
        let error = |id| ControlFrame::Reply {
            id,
            reply: ControlReply::Error {
                error: tutti_plugin_types::ServerError::StateWithoutObject,
            },
        };
        assert!(matches!(
            state.hear(error(first), Some(first), Instant::now(), &mut inbox),
            Ok(Some(ControlReply::Error { .. }))
        ));
        assert!(
            state
                .hear(
                    error(ReqId::new(9)),
                    Some(first),
                    Instant::now(),
                    &mut inbox
                )
                .is_err(),
            "an error for a request nobody made"
        );
    }

    /// **A sign of life restarts the request's wait, and news does not**: the
    /// frames of a chatty plugin do not hold a request open. Driven with
    /// constructed instants.
    #[test]
    fn a_sign_of_life_restarts_the_wait_and_news_does_not() {
        let mut state = control_state();
        let mut inbox = Unsolicited::new();
        let t = Instant::now();
        let id = ReqId::new(1);
        state
            .conv
            .open(id, Abandoned::Discard, t, WINDOW, Duration::from_secs(600));
        let at = |secs| t + Duration::from_secs(secs);

        let news = ControlFrame::News(News::PluginReloaded);
        assert!(matches!(
            state.hear(news, Some(id), at(3), &mut inbox),
            Ok(None)
        ));
        assert_eq!(state.conv.left(id, at(3)), Some(Duration::from_secs(2)));

        let working = ControlFrame::Working {
            id,
            stage: crate::Working::Opening,
        };
        assert!(matches!(
            state.hear(working, Some(id), at(3), &mut inbox),
            Ok(None)
        ));
        assert_eq!(state.conv.left(id, at(3)), Some(WINDOW));
    }

    /// **A carried value goes into a reserved queue of the block's buffer**,
    /// so the buffer returns to the pool with its point room intact. A
    /// queue pushed past the spares (the carry's own, with the inline room)
    /// later took a reserved queue's place on `clear` and left the buffer
    /// with a 10-point queue among its reserved ones.
    #[test]
    fn a_carried_value_fills_a_reserved_queue() {
        let (mut state, _replies, _channels) = audio_state();
        let mut skipped = ParameterChanges::reserved(1, 1);
        skipped.add_change(ParamId::new(7), 3, 0.25);
        state.carry(&skipped);

        let mut block = ParameterChanges::reserved(4, 40);
        assert!(block.open_queue(ParamId::new(1)));
        if let Some(q) = block.queues.last_mut() {
            q.add_point(5, 0.5);
        }
        state.merge_carried(&mut block);

        let carried = block.queue(ParamId::new(7)).expect("carried");
        assert_eq!(carried.points.len(), 1);
        assert_eq!(carried.points[0].sample_offset, 0);
        assert!((carried.points[0].value.get() - 0.25).abs() < 1e-12);
        assert!(
            carried.points.capacity() >= 40,
            "the carried queue is a reserved one: {}",
            carried.points.capacity()
        );
        block.clear();
        assert_eq!(block.point_storage(), 4 * 41, "the reserved storage");
    }

    /// **A reserved buffer with no spare queue drops a carried value,
    /// counted, rather than growing; the empty placeholder takes every one.**
    /// A one-queue buffer with its queue open for another parameter: the
    /// carried value is dropped inside the no-alloc gate and counted, and the
    /// buffer still has one queue. The placeholder of a block with no
    /// changes (`ParameterChanges::new`) gets a queue for each of two
    /// carried values, though the first gives it queue room.
    #[test]
    fn a_carried_value_past_the_queues_is_dropped_but_the_placeholder_takes_all() {
        let mut skipped = ParameterChanges::reserved(2, 1);
        skipped.add_change(ParamId::new(7), 3, 0.25);
        skipped.add_change(ParamId::new(8), 3, 0.5);

        let mut state = audio_state().0;
        state.carry(&skipped);
        let mut block = ParameterChanges::reserved_bridged(1, 40);
        assert!(block.add_change(ParamId::new(1), 5, 0.5));
        assert_no_alloc::assert_no_alloc(|| state.merge_carried(&mut block));
        assert_eq!(block.queues.len(), 1);
        assert_eq!(block.points_overflowed(), 2);

        let mut state = audio_state().0;
        state.carry(&skipped);
        let mut placeholder = ParameterChanges::new();
        state.merge_carried(&mut placeholder);
        assert_eq!(placeholder.queues.len(), 2);
        assert_eq!(placeholder.points_overflowed(), 0);
        for (id, value) in [(7, 0.25), (8, 0.5)] {
            let q = placeholder.queue(ParamId::new(id as u32)).expect("queued");
            assert_eq!(q.points.len(), 1);
            assert_eq!(
                (q.points[0].sample_offset, q.points[0].value.get()),
                (0, value)
            );
        }
    }

    /// **The carried values reach a chunk at the pool budget whole, in real
    /// slots, without allocating or growing it.** 2 000 parameters at a
    /// 512-frame block, the block's buffer built `reserved_bridged` as the
    /// buffer pool builds it. The block moves all but the last ten
    /// parameters, every one at its floor (first point after frame 0) and
    /// the last `POOLED_QUEUES` of them at their full room; a skipped block
    /// left a value state for every parameter. Inside the no-alloc gate,
    /// `merge_carried` puts each on frame 0 (the ten unmoved ones in spare
    /// queues): nothing is counted, every queue gained exactly its carried
    /// point ahead of its own, and the buffer's storage is what it was.
    #[test]
    fn carried_values_take_real_slots_at_the_pool_budget_without_allocating() {
        const PARAMS: usize = 2_000;
        const UNMOVED: usize = 10;
        let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
        let moved = PARAMS - UNMOVED;
        let full = moved - ParameterChanges::POOLED_QUEUES..moved;
        let addr = |i: usize| ParamId::new((i as i32) as u32);

        let mut skipped = ParameterChanges::reserved(PARAMS, 1);
        for i in 0..PARAMS {
            assert!(skipped.add_change(addr(i), 7, 0.25));
        }
        let mut state = audio_state().0;
        state.carry(&skipped);

        // Filled as the node's delivery fills it: a full queue takes points
        // while `make_room` gives it room (the producers' room, `room`).
        let mut block = ParameterChanges::reserved_bridged(PARAMS, room);
        for i in 0..moved {
            assert!(block.open_queue(addr(i)));
            if full.contains(&i) {
                let mut k = 0;
                while block.make_room(i) {
                    assert!(block.push_point(i, 1 + k, 0.5));
                    k += 1;
                }
            } else {
                for k in 0..ParameterChanges::FLOOR_POINTS {
                    assert!(block.push_point(i, 1 + k as i32, 0.5));
                }
            }
        }
        let before: Vec<usize> = block.queues.iter().map(|q| q.points.len()).collect();
        let storage = block.point_storage();

        assert_no_alloc::assert_no_alloc(|| state.merge_carried(&mut block));

        assert_eq!(block.points_overflowed(), 0);
        assert_eq!(block.point_storage(), storage, "nothing grew");
        assert_eq!(block.queues.len(), PARAMS);
        for (i, q) in block.queues.iter().enumerate() {
            let own = before.get(i).copied().unwrap_or(0);
            assert_eq!(q.points.len(), own + 1, "queue {i}");
            assert_eq!(q.points[0].sample_offset, 0, "queue {i}");
            assert!((q.points[0].value.get() - 0.25).abs() < 1e-12);
        }
    }

    /// Every production block size / rate combination the engine can present.
    /// A chunk is one device callback (`MAX_CHUNK` at most), so the timeout
    /// must hold at every size.
    const RATES: [SampleRate; 4] = [
        SampleRate::SR_44K1,
        SampleRate::SR_48K,
        SampleRate::SR_96K,
        SampleRate::SR_192K,
    ];
    const BLOCKS: [usize; 4] = [64, 128, 256, 512];

    fn period(num_samples: usize, rate: SampleRate) -> Duration {
        Duration::from_secs_f64(num_samples as f64 / rate.get())
    }

    /// A fixed 500 ms timeout sits ~750x above the audio thread's 667 us budget
    /// at 64 frames / 48 kHz. While the bridge thread blocks in `recv_reply` it
    /// dequeues nothing, so the audio thread's one-command-per-block fills the
    /// 128-slot queue and one slow reply becomes a sustained run of silence.
    /// The timeout is therefore bound to a few block periods.
    #[test]
    fn timeout_is_a_small_multiple_of_the_block_period() {
        for rate in RATES {
            for n in BLOCKS {
                let t = process_timeout(n, rate);
                let p = period(n, rate);
                let limit = p * PROCESS_TIMEOUT_PERIODS;
                assert!(
                    t <= limit.max(MIN_PROCESS_TIMEOUT),
                    "n={n} rate={rate}: timeout {t:?} exceeds {PROCESS_TIMEOUT_PERIODS} periods ({limit:?})"
                );
                assert!(
                    t <= MAX_PROCESS_TIMEOUT,
                    "n={n} rate={rate}: timeout {t:?} over the ceiling"
                );
            }
        }
    }

    /// The bridge's patience must outlast the ring, or it would abandon blocks
    /// whose slots are still live — declaring a plugin unresponsive while its
    /// reply was still wanted.
    ///
    /// Bounded against the ring depth rather than the audio thread's wait
    /// budget: the audio thread never waits, so there is nothing to outlast on
    /// that side.
    #[test]
    fn timeout_keeps_the_bridge_within_the_ring_depth() {
        for rate in RATES {
            for n in BLOCKS {
                let t = process_timeout(n, rate);
                let ring_lifetime = period(n, rate) * MAX_BEHIND as u32;
                assert!(
                    t >= ring_lifetime.min(MAX_PROCESS_TIMEOUT),
                    "n={n} rate={rate}: timeout {t:?} gives up before the slot \
                     is recycled ({ring_lifetime:?})"
                );
            }
        }
    }

    /// The block just submitted is never stale — dropping it would turn every
    /// block into silence, the opposite of the fix.
    #[test]
    fn the_newest_block_is_never_stale() {
        assert!(!is_stale(10, 10));
    }

    /// Anything still inside the ring is sent: its slot has not been recycled,
    /// so its output can still be collected.
    #[test]
    fn blocks_still_inside_the_ring_are_sent() {
        for behind in 0..=MAX_BEHIND {
            assert!(
                !is_stale(10 - behind, 10),
                "{behind} block(s) behind is still within the ring"
            );
        }
    }

    /// Past the ring depth the slot has been overwritten, so no reply can land
    /// anywhere useful.
    #[test]
    fn blocks_past_the_ring_are_dropped() {
        assert!(is_stale(10 - MAX_BEHIND - 1, 10));
        assert!(is_stale(0, 10));
        assert!(is_stale(0, 1_000));
    }

    /// A backlog collapses to the live blocks in one pass rather than being
    /// ground through. Sending all ten, each a socket round-trip, loses ground
    /// while the audio thread keeps adding more.
    #[test]
    fn a_backlog_collapses_to_the_live_blocks() {
        let newest = 10u64;
        let survivors: Vec<u64> = (0..=newest).filter(|&s| !is_stale(s, newest)).collect();
        assert_eq!(survivors, vec![9, 10]);
    }

    /// **The invariant, stated as a property rather than as a number.**
    ///
    /// No block this admits may share a ring slot with the newest one. If it
    /// did, the server would write that block's output into the slot the host
    /// is reading for `newest` and stamp the slot's sequence with its own —
    /// tearing the block being read, or destroying the evidence for it.
    ///
    /// Asserting specific numbers (`MAX_BEHIND == RING_SLOTS`, `survivors ==
    /// [8, 9, 10]`) would pin an off-by-one as intended behaviour: at depth 2,
    /// `slot_for(8) == slot_for(10)`. Asserting the property makes the tests
    /// independent of the ring depth, so raising `RING_SLOTS` cannot silently
    /// introduce it.
    #[test]
    fn no_admitted_block_shares_a_slot_with_the_newest() {
        // Mirrors `shm::header::slot_for`, which is private to that module.
        // Duplicated deliberately: importing it would couple this test to the
        // slab's internals, and the mapping (`seq % RING_SLOTS`) is the *wire*
        // contract both sides implement, not an implementation detail.
        let slot_of = |seq: u64| seq % RING_SLOTS as u64;

        let newest = 64u64;
        for seq in 0..=newest {
            if is_stale(seq, newest) {
                continue;
            }
            assert!(
                seq == newest || slot_of(seq) != slot_of(newest),
                "block {seq} is {} behind and admitted, but shares slot {} with \
                 the newest block {newest} — the server would overwrite the slot \
                 the host is reading",
                newest - seq,
                slot_of(seq),
            );
        }
    }

    /// `newest < seq` cannot occur — the bridge cannot dequeue a block that was
    /// never pushed — but the comparison must be total, and "send it" is the
    /// safe direction if it somehow did.
    #[test]
    fn an_impossible_future_block_is_not_dropped() {
        assert!(!is_stale(20, 10));
    }
}
