//! Per-command IPC choreography for the bridge's two lane threads: the audio
//! lane's ([`handle_audio`]) and the control lane's ([`handle_control`]).

use super::ask::Reply;
#[cfg(test)]
use super::channels::response_ring;
use super::channels::{Channels, Fate, ResponseWriter};
use super::lifecycle::Lifecycle;
use super::messages::{AudioCommand, AudioResponse, ControlCommand, Setting, Unsolicited};
use super::payload_pool::PayloadPool;
use super::progress::StateProgress;
use crate::ProcessAudioData;
use crate::error::unexpected_message;
use crate::transport::control::{self as ipc, ControlStream, PartialFrame};
use crate::transport::shm::RING_SLOTS;
use crate::transport::state_chunk::{self, ChunkError, Reassembler};
use crate::{AudioReply, AudioRequest, ControlFrame, ControlReply, ControlRequest, News};
use crossbeam_channel::RecvTimeoutError;
use std::collections::VecDeque;
use std::time::Duration;
use tutti_platform::Instant;
use tutti_plugin_types::{BridgeError, BridgeResult, ResyncKind, StateError};
use tutti_plugin_types::{Deadline, MidiIntake};
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

/// What the audio lane's server still owes for requests this thread stopped
/// waiting on, the cursor of any frame a timeout left half-read, and what the
/// lane carries from one block to the next.
///
/// **The server answers strictly in order**, one request at a time, so every
/// frame owed for an earlier request arrives before the answer to the next
/// one. Taking exactly the owed frames off the stream first
/// ([`AudioOwed::take`], which every read goes through) keeps each answer
/// paired with its own request, whatever kind of request gave up.
///
/// Owned by the audio lane's thread and passed in: only that thread ever reads
/// or writes it. The [`PartialFrame`] has exactly this scope for a different
/// reason: it describes a position in *this* connection's stream, so it lives
/// and dies with the stream.
pub(super) struct AudioOwed {
    /// How long a reply that has already started arriving gets to finish when
    /// its request's own budget runs out
    /// ([`Timeouts::drain`](tutti_plugin_types::Timeouts::drain)).
    finish: Duration,
    /// What is still owed, oldest first: the order the frames arrive in.
    late: VecDeque<AudioLate>,
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

impl AudioOwed {
    /// Nothing owed on the audio connection, whose begun replies get `finish`
    /// to end and whose thread alone answers the blocks into `responses`.
    pub(super) fn new(finish: Duration, responses: ResponseWriter) -> Self {
        Self {
            finish,
            late: VecDeque::new(),
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

    /// Note that a request's reply was abandoned before it arrived.
    fn owe(&mut self, late: AudioLate) {
        self.late.push_back(late);
    }

    /// Whether `msg` is a frame owed for an earlier request, taking it if so:
    /// `None` when it was owed (and has been dealt with), `Some(msg)` when it
    /// is the answer the caller is waiting for.
    ///
    /// Each owed item knows what answers it (and the server's `Error` answers
    /// anything). A frame of another kind means that answer is not coming: the
    /// item is let go and the frame is weighed against the next one, so a lost
    /// answer cannot swallow every reply after it.
    ///
    /// A block's owed reply is **forwarded, not discarded**. Its audio is
    /// moot (the slab slot it names has been recycled, and the sequence check
    /// rejects it) but its MIDI-out is the plugin's real output and the socket
    /// is its only path. Only a block's counts as late
    /// ([`Channels::note_late`](super::channels::Channels::note_late)).
    ///
    /// A block's reply naming a block never sent is no owed reply: it is
    /// handed back as the answer, for the caller to refuse.
    fn take(&mut self, msg: AudioReply, channels: &Channels) -> Option<AudioReply> {
        if matches!(msg, AudioReply::AudioProcessed { seq, .. } if !channels.sent(seq)) {
            return Some(msg);
        }
        while let Some(&late) = self.late.front() {
            let error = matches!(msg, AudioReply::Error { .. });
            match late {
                AudioLate::Block => {
                    if let AudioReply::AudioProcessed {
                        seq,
                        midi_out,
                        status,
                        refused,
                        ..
                    } = msg
                    {
                        channels.note_fate(
                            seq,
                            if refused {
                                Fate::Refused
                            } else {
                                Fate::Published
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
                        self.late.pop_front();
                        return None;
                    }
                    if error {
                        // The block fell back to silence when its budget
                        // expired; nothing to forward. The plugin answered,
                        // though not with a reply to collect.
                        channels.note_replied();
                        self.late.pop_front();
                        return None;
                    }
                }
                AudioLate::Params => {
                    if error || matches!(msg, AudioReply::ParametersApplied) {
                        self.late.pop_front();
                        return None;
                    }
                }
            }
            self.late.pop_front();
        }
        Some(msg)
    }
}

/// What a request the audio lane stopped waiting for still has coming.
#[derive(Clone, Copy, Debug)]
pub(super) enum AudioLate {
    /// A block's `AudioProcessed` (or `Error`): its MIDI is forwarded.
    Block,
    /// The one `ParametersApplied` (or `Error`) of an `ApplyParameters`.
    Params,
}

/// The next reply to the request just sent on the audio connection, within
/// `budget`, after the frames owed for earlier requests: `Ok(None)` when it
/// did not come in time.
///
/// One deadline for the whole read, and a budget that expires part-way through
/// a frame gets [`Timeouts::drain`](tutti_plugin_types::Timeouts::drain) to
/// complete that frame: see [`next_reply`], which reads the control
/// connection the same way.
fn next_audio_reply(
    stream: &mut ControlStream,
    owed: &mut AudioOwed,
    channels: &Channels,
    budget: Duration,
) -> BridgeResult<Option<AudioReply>> {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match ipc::recv_resumable::<AudioReply>(stream, left, &mut owed.partial) {
            Ok(msg) => {
                if let Some(msg) = owed.take(msg, channels) {
                    return Ok(Some(msg));
                }
            }
            Err(BridgeError::Timeout { partial: true, .. }) => {
                let frame =
                    ipc::recv_resumable::<AudioReply>(stream, owed.finish, &mut owed.partial)?;
                return Ok(owed.take(frame, channels));
            }
            Err(BridgeError::Timeout { partial: false, .. }) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

/// [`next_audio_reply`], owing the reply as `late` when it does not come in
/// time, so it is taken off the stream when it does.
fn audio_answer(
    stream: &mut ControlStream,
    owed: &mut AudioOwed,
    channels: &Channels,
    budget: Duration,
    late: AudioLate,
) -> BridgeResult<Option<AudioReply>> {
    let reply = next_audio_reply(stream, owed, channels, budget)?;
    if reply.is_none() {
        owed.owe(late);
    }
    Ok(reply)
}

/// Replies the control lane's server still owes for requests this thread
/// stopped waiting on, plus the cursor of any frame a timeout left half-read.
///
/// **The server answers strictly in order**, one request at a time, so every
/// frame owed for an earlier request arrives before the answer to the next
/// one. Taking exactly the owed frames off the stream first ([`Owed::take`],
/// which every read goes through, [`next_reply`]) therefore keeps each answer
/// paired with its own request, whatever kind of request gave up.
///
/// Owned by the control lane's thread and passed in: only that thread ever
/// reads or writes it. The [`PartialFrame`] must have exactly this scope for a
/// different reason: it describes a position in *this* connection's stream, so
/// it lives and dies with the stream.
pub(super) struct Owed {
    /// How long a reply that has already started arriving gets to finish when
    /// its request's own budget runs out ([`Timeouts::drain`](tutti_plugin_types::Timeouts::drain)).
    ///
    /// Larger than any request's budget, and the split is the point: the
    /// decision to *wait* for a reply is the request's, and is short for a
    /// query, but once bytes of a frame are moving the reply is arriving and
    /// finishing it costs little. A few hundred bytes over a local socket
    /// cannot plausibly take this long without the peer being dead, which the
    /// read reports as EOF rather than as a timeout.
    finish: Duration,
    /// What is still owed, oldest first: the order the frames arrive in.
    late: VecDeque<Late>,
    /// Where the last read stopped, so a timeout resumes rather than restarts.
    partial: PartialFrame,
}

impl Owed {
    /// Nothing owed on the control connection, whose begun replies get `finish`
    /// to end.
    pub(super) fn new(finish: Duration) -> Self {
        Self {
            finish,
            late: VecDeque::new(),
            partial: PartialFrame::default(),
        }
    }

    /// Note that a request's reply was abandoned before it arrived.
    fn owe(&mut self, late: Late) {
        self.late.push_back(late);
    }

    /// Whether `msg` is a frame owed for an earlier request, taking it if so:
    /// `None` when it was owed (and has been dealt with), `Some(msg)` when it
    /// is the answer the caller is waiting for.
    ///
    /// Each owed item knows what answers it (and the server's `Error` answers
    /// anything). A frame of another kind means that answer is not coming
    /// (a peer that dropped it): the item is let go and the frame is weighed
    /// against the next one, so a lost answer cannot swallow every reply
    /// after it.
    fn take(&mut self, msg: ControlReply) -> Option<ControlReply> {
        while let Some(&late) = self.late.front() {
            let error = matches!(msg, ControlReply::Error { .. });
            match late {
                Late::Answer(answers) if error || answers(&msg) => {
                    self.late.pop_front();
                    return None;
                }
                Late::Answer(_) => {}
                // A state keeps arriving until its `last` chunk; an `Error`
                // instead of the chunks ends it.
                Late::StateRest => match msg {
                    ControlReply::StateChunk { last: false, .. } => return None,
                    ControlReply::StateChunk { last: true, .. } | ControlReply::Error { .. } => {
                        self.late.pop_front();
                        return None;
                    }
                    _ => {}
                },
            }
            self.late.pop_front();
        }
        Some(msg)
    }
}

/// What a request the control lane stopped waiting for still has coming.
#[derive(Clone, Copy, Debug)]
pub(super) enum Late {
    /// A control request's one reply, of the kind this accepts (or `Error`).
    Answer(fn(&ControlReply) -> bool),
    /// The rest of a `SaveState` answer: chunks up to `last`, or one `Error`.
    StateRest,
}

/// The next reply to the request just sent, within `budget`, after the frames
/// owed for earlier requests: `Ok(None)` when it did not come in time.
///
/// **One deadline for the whole read.** The owed frames and the unsolicited
/// events read on the way are inside it; a chatty plugin or a
/// backlog does not extend it.
///
/// A timeout on a frame boundary is not an error: the stream is synchronised
/// and the session healthy, so the caller answers its own caller and the
/// bridge goes on. A budget that expires **part-way through a frame** gets
/// [`Timeouts::drain`](tutti_plugin_types::Timeouts::drain) to complete *that frame*, and the read ends there (with
/// the frame, when it is the answer): extending the whole read instead let a
/// chatty peer start a new frame at the extended deadline, and a live peer was
/// declared unable to frame. A frame still unfinished after `Timeouts::drain` is
/// a peer that cannot frame (a few hundred bytes over a local socket do not
/// take that long from a live one), and that timeout propagates with the
/// transport's errors (EOF, a frame over the cap), which the lane's loop crashes on.
fn next_reply(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    owed: &mut Owed,
    budget: Duration,
) -> BridgeResult<Option<ControlReply>> {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match recv_reply(stream, inbox, left, &mut owed.partial) {
            Ok(msg) => {
                if let Some(msg) = owed.take(msg) {
                    return Ok(Some(msg));
                }
            }
            Err(BridgeError::Timeout { partial: true, .. }) => {
                let frame = ipc::recv_resumable(stream, owed.finish, &mut owed.partial)?;
                let answer = absorb(inbox, frame).and_then(|msg| owed.take(msg));
                return Ok(answer);
            }
            Err(BridgeError::Timeout { partial: false, .. }) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}

/// [`next_reply`], owing the reply as `late` when it does not come in time,
/// so it is taken off the stream when it does.
fn answer(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    owed: &mut Owed,
    budget: Duration,
    late: Late,
) -> BridgeResult<Option<ControlReply>> {
    let reply = next_reply(stream, inbox, owed, budget)?;
    if reply.is_none() {
        owed.owe(late);
    }
    Ok(reply)
}

/// One connection of the bridge: its stream, and the replies it still owes
/// (`O`: [`AudioOwed`] on the audio lane, [`Owed`] on the control lane).
pub(super) struct Lane<O> {
    pub(super) stream: ControlStream,
    pub(super) owed: O,
}

/// The audio lane's command, run on its thread: **the lane carries `ProcessAudio`
/// and `Reset`** (the server's render thread answers them), in order and one
/// at a time. A block is never queued behind the control lane (the server's
/// main thread, which may be busy with an editor), and what a block's reply
/// owes ([`Owed`]) is paired on its own stream.
pub(super) fn handle_audio(
    cmd: AudioCommand,
    lane: &mut Lane<AudioOwed>,
    channels: &Channels,
    payloads: &PayloadPool,
    lifecycle: &Lifecycle,
) -> BridgeResult<()> {
    let Lane { stream, owed } = lane;
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
                channels.note_fate(seq, Fate::Skipped);
                // Its MIDI is lost, counted, and so is the rest of every
                // SysEx open in it: the server's reassembler would otherwise
                // complete it from the next block's packets, with these
                // missing. (No wire change: a gap is never sent.)
                for e in &payload.midi_events {
                    owed.intake.admit(e);
                }
                owed.intake.lost();
                channels.note_dropped_midi(payload.midi_events.len() as u64);
                owed.carry(&payload.param_changes);
                payloads.return_params(core::mem::take(&mut payload.param_changes));
                payloads.recycle(payload);
                channels.note_handled(seq);
                return Ok(());
            }

            // What the host wrote since the last block, on its first frame.
            channels.drain_host_edits(|id, value| owed.carry_write(id, value));
            if !owed.carried.is_empty() {
                owed.merge_carried(&mut payload.param_changes);
            }
            let before = payload.midi_events.len();
            payload.midi_events.retain(|e| owed.intake.admit(e));
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
            let sent = ipc::send(stream, &msg);
            // The parameter-change buffer goes back to the audio thread's free
            // list once its contents are on the wire (sent or not: a failed
            // send crashes the bridge below, and the buffer is no use to it).
            if let AudioRequest::ProcessAudio(data) = msg {
                payloads.return_params(data.param_changes);
            }
            sent?;

            // Replies owed from earlier blocks come first and are forwarded
            // (`Owed::take`); then this block's own, within its budget.
            match audio_answer(stream, owed, channels, timeout, AudioLate::Block)? {
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
                // The reply is late, not absent: `answer` owes it, so it is
                // taken off the stream before a later request reads its own.
                // The host needs no notification to fall back to silence (the
                // server has not published, so the slab's sequence check fails
                // for this block on its own); the response keeps the
                // abandonment visible and stops the queue running a block
                // behind.
                None => {
                    channels.note_abandoned();
                    owed.respond(channels, AudioResponse::Error { seq: Some(seq) });
                }
                Some(reply) => {
                    match reply {
                        // A reply to a block not yet sent is a peer making
                        // fates up, not an answer.
                        AudioReply::AudioProcessed { seq: replied, .. } if replied > seq => {
                            return Err(BridgeError::ProtocolError(
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
                            channels.note_fate(
                                seq,
                                if refused {
                                    Fate::Refused
                                } else {
                                    Fate::Published
                                },
                            );
                            owed.respond(
                                channels,
                                AudioResponse::AudioProcessed {
                                    seq,
                                    midi_out,
                                    status,
                                },
                            );
                        }
                        // Attributable to this request: the server answered it
                        // and never published, so the block is silence; the
                        // variant keeps the failure visible.
                        AudioReply::Error { .. } => {
                            owed.respond(channels, AudioResponse::Error { seq: Some(seq) });
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
            ipc::send(
                stream,
                &AudioRequest::ApplyParameters {
                    param_changes: changes,
                },
            )?;
            let applied = audio_answer(
                stream,
                owed,
                channels,
                lifecycle.timeouts().param_wait(),
                AudioLate::Params,
            )?;
            done.send(matches!(applied, Some(AudioReply::ParametersApplied)));
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
/// The wait ends after a rate change's deadline and a query's, or when the
/// bridge stops or crashes. A change the lane gives up on at the deadline is
/// counted ([`Channels::note_unapplied`]) and logged, and the blocks go on: a
/// lane that cannot answer in that time is the control lane's to report.
pub(super) fn apply_setting(channels: &Channels, lifecycle: &Lifecycle, setting: Setting) {
    let what = match setting {
        Setting::SampleRate(_) => "a rate change",
        Setting::RenderMode(_) => "a render mode",
        Setting::Fence => "a settle",
    };
    let (applied, reply) = super::ask::ask::<()>();
    let deadline = Deadline::after(Instant::now(), lifecycle.timeouts().settle_wait());
    channels.put_setting(setting, reply);
    loop {
        match applied.recv_timeout_borrowed(RATE_SLICE) {
            Err(RecvTimeoutError::Timeout) => {
                if !lifecycle.is_running() || lifecycle.is_crashed() {
                    return;
                }
                if deadline.passed(Instant::now()) {
                    channels.note_unapplied();
                    tracing::warn!(
                        "the plugin bridge gave up holding blocks for {what}: \
                         the plugin did not apply it in time"
                    );
                    return;
                }
            }
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A round trip that proves the server has handled every request sent before
/// it: it answers this lane in order, and `GetCurrentPreset` is a cheap
/// request every server answers. A reply that does not come in time is owed
/// and does not hold the caller longer.
fn fence(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    owed: &mut Owed,
    lifecycle: &Lifecycle,
) -> BridgeResult<()> {
    ipc::send(stream, &ControlRequest::GetCurrentPreset)?;
    ask(stream, inbox, owed, lifecycle, |m| {
        matches!(m, ControlReply::CurrentPreset { .. })
    })?;
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
    lane: &mut Lane<Owed>,
    inbox: &mut Unsolicited,
    lifecycle: &Lifecycle,
) -> BridgeResult<()> {
    let Lane { stream, owed } = lane;
    match cmd {
        ControlCommand::SetAutomationState { mode } => {
            ipc::send(stream, &ControlRequest::SetAutomationState { mode })?;
        }
        ControlCommand::SetTrackInfo { info } => {
            ipc::send(stream, &ControlRequest::SetTrackInfo { info: *info })?;
        }
        ControlCommand::SaveState {
            state_use,
            progress,
            reply,
        } => {
            ipc::send(stream, &ControlRequest::SaveState { state_use })?;
            reply.send(recv_state(stream, inbox, owed, &progress)?);
        }
        ControlCommand::LoadState {
            state_use,
            data,
            progress,
            reply,
        } => {
            // Refuse over-limit state *here*, before a byte is written, and
            // answer the caller rather than propagating.
            //
            // Propagating would be wrong twice over: `?` returns before
            // `reply.send`, so the `Reply` drops and the caller waits out
            // the progress deadline to learn a length comparison the host made
            // instantly; and a lane's loop treats every handler error as
            // connection-level, so one oversized preset would `crash()` a
            // healthy session. Nothing was sent, so the stream is still
            // synchronised and there is nothing to crash about. Contrast
            // `recv_by`, where an over-cap length genuinely *has*
            // desynchronised the stream.
            if data.len() > tutti_plugin_types::MAX_STATE_BYTES {
                reply.send(Err(StateError::TooLarge {
                    bytes: data.len(),
                    limit: tutti_plugin_types::MAX_STATE_BYTES,
                }));
                return Ok(());
            }
            for (seq, last, bytes) in state_chunk::split(&data) {
                let n = bytes.len();
                ipc::send(
                    stream,
                    &ControlRequest::LoadStateChunk {
                        seq,
                        last: last.then_some(state_use),
                        bytes: bytes.to_vec(),
                    },
                )?;
                // Report the write before waiting on the acknowledgement. A
                // socket that accepts chunks is a transfer that is moving, and
                // it is the only progress this direction has to show: the
                // server answers once, on `last`, so a caller watching for a
                // reply alone cannot distinguish a slow write from a stall.
                progress.advance(n);
            }
            // Wait for the answer, as `SaveState` above does. Answering the
            // caller straight after the write would report that the request had
            // been *sent*, never whether the plugin accepted it.
            //
            // A timeout here is **not** connection-level, for the same reason
            // the over-limit branch above is not: the socket is synchronised and
            // the session is healthy — the plugin simply has not answered yet.
            // Propagating would `crash()` it, and the caller would receive
            // `Stalled` (which says "the session is fine, retrying is
            // reasonable") against a session that had just been torn down.
            // The answer is owed, and taken off the stream when it comes.
            let budget = progress.deadline();
            let value = match answer(
                stream,
                inbox,
                owed,
                budget,
                Late::Answer(|m| matches!(m, ControlReply::StateLoaded { .. })),
            )? {
                Some(ControlReply::StateLoaded { error }) => {
                    error.map(StateError::Rejected).map_or(Ok(()), Err)
                }
                Some(other) => Err(StateError::Rejected(format!(
                    "unexpected reply to LoadState: {other:?}"
                ))),
                None => Err(progress.stalled()),
            };
            reply.send(value);
        }
        ControlCommand::GetParameterList { reply } => {
            ipc::send(stream, &ControlRequest::GetParameterList)?;
            let value = match ask(stream, inbox, owed, lifecycle, |m| {
                matches!(m, ControlReply::ParameterList { .. })
            })? {
                Some(ControlReply::ParameterList { parameters }) => Some(parameters),
                _ => None,
            };
            reply.send(value);
        }
        ControlCommand::GetPresetList { reply } => {
            ipc::send(stream, &ControlRequest::GetPresetList)?;
            let value = match ask(stream, inbox, owed, lifecycle, |m| {
                matches!(m, ControlReply::PresetList { .. })
            })? {
                Some(ControlReply::PresetList { presets }) => Some(presets),
                _ => None,
            };
            reply.send(value);
        }
        ControlCommand::LoadPreset { id, reply } => {
            ipc::send(stream, &ControlRequest::LoadPreset { id })?;
            let ok = matches!(
                ask(stream, inbox, owed, lifecycle, |m| matches!(
                    m,
                    ControlReply::PresetLoaded { .. }
                ))?,
                Some(ControlReply::PresetLoaded { ok: true })
            );
            reply.send(ok);
        }
        ControlCommand::SetupSharedMemory {
            shm_name,
            layout,
            reply,
        } => {
            ipc::send(
                stream,
                &ControlRequest::SetupSharedMemory { shm_name, layout },
            )?;
            // The server's refusal (it could not open the slab) is an `Error`:
            // no reason to wait out the deadline for a `Ready` that is not coming.
            let ok = matches!(
                ask(stream, inbox, owed, lifecycle, |m| matches!(
                    m,
                    ControlReply::SharedMemoryReady | ControlReply::Error { .. }
                ))?,
                Some(ControlReply::SharedMemoryReady)
            );
            reply.send(ok);
        }
        ControlCommand::GetCurrentPreset { reply } => {
            ipc::send(stream, &ControlRequest::GetCurrentPreset)?;
            let value = match ask(stream, inbox, owed, lifecycle, |m| {
                matches!(m, ControlReply::CurrentPreset { .. })
            })? {
                Some(ControlReply::CurrentPreset { id }) => id,
                _ => None,
            };
            reply.send(value);
        }
        ControlCommand::GetParameter { param_id, reply } => {
            ipc::send(stream, &ControlRequest::GetParameter { param_id })?;
            let value = match ask(stream, inbox, owed, lifecycle, |m| {
                matches!(m, ControlReply::ParameterValue { .. })
            })? {
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
            ipc::send(
                stream,
                &ControlRequest::GetParameterText { param_id, value },
            )?;
            let text = match ask(stream, inbox, owed, lifecycle, |m| {
                matches!(m, ControlReply::ParameterText { .. })
            })? {
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
            ipc::send(
                stream,
                &ControlRequest::GetParameterValueFromText { param_id, text },
            )?;
            let value = match ask(stream, inbox, owed, lifecycle, |m| {
                matches!(m, ControlReply::ParameterValueFromText { .. })
            })? {
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
            ipc::send(
                stream,
                &ControlRequest::GetParameterPlain { param_id, value },
            )?;
            let plain = match ask(stream, inbox, owed, lifecycle, |m| {
                matches!(m, ControlReply::ParameterPlain { .. })
            })? {
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
            ipc::send(stream, &ControlRequest::OpenEditor { parent, scale })?;
            let value = match answer(
                stream,
                inbox,
                owed,
                lifecycle.timeouts().editor,
                Late::Answer(|m| matches!(m, ControlReply::EditorOpened { .. })),
            )? {
                Some(ControlReply::EditorOpened { result }) => result,
                Some(ControlReply::Error { message }) => Err(message),
                Some(other) => Err(format!("unexpected reply to OpenEditor: {other:?}")),
                None => Err(format!(
                    "the server did not open the editor within {:?}",
                    lifecycle.timeouts().editor
                )),
            };
            reply.send(value);
        }
        ControlCommand::CloseEditor => {
            ipc::send(stream, &ControlRequest::CloseEditor)?;
        }
        ControlCommand::SetEditorSize { size, reply } => {
            ipc::send(stream, &ControlRequest::SetEditorSize { size })?;
            let value = match answer(
                stream,
                inbox,
                owed,
                lifecycle.timeouts().editor,
                Late::Answer(|m| matches!(m, ControlReply::EditorSized { .. })),
            )? {
                Some(ControlReply::EditorSized { result }) => result,
                Some(ControlReply::Error { message }) => Err(message),
                Some(other) => Err(format!("unexpected reply to SetEditorSize: {other:?}")),
                None => Err(format!(
                    "the server did not resize the editor within {:?}",
                    lifecycle.timeouts().editor
                )),
            };
            reply.send(value);
        }
        ControlCommand::GetParameterNormalized {
            param_id,
            plain,
            reply,
        } => {
            ipc::send(
                stream,
                &ControlRequest::GetParameterNormalized { param_id, plain },
            )?;
            let value = match ask(stream, inbox, owed, lifecycle, |m| {
                matches!(m, ControlReply::ParameterNormalized { .. })
            })? {
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
    lane: &mut Lane<Owed>,
    inbox: &mut Unsolicited,
    lifecycle: &Lifecycle,
) -> BridgeResult<()> {
    let Lane { stream, owed } = lane;
    match setting {
        Setting::SampleRate(rate) => {
            ipc::send(stream, &ControlRequest::SetSampleRate { rate })?;
            // Wait for the answer: the plugin's re-activation is the slowest
            // thing the bridge asks of it, and the requests queued behind it
            // on this lane wait for it. The audio lane does not: its blocks
            // are the server's render thread's.
            //
            // Replies owed from abandoned requests can still be ahead of the
            // answer on the stream; `next_reply` takes them first.
            //
            // Waited in slices, so a shutdown (dropping the node joins this
            // thread) ends the wait within one slice rather than after
            // `Timeouts::rate`. A slice that expires part-way through a frame
            // resumes it (`owed.partial`), so slicing never unframes the stream.
            let budget = lifecycle.timeouts().rate;
            let deadline = tutti_platform::Instant::now() + budget;
            loop {
                let reply = match next_reply(stream, inbox, owed, RATE_SLICE)? {
                    None => {
                        if !lifecycle.is_running() {
                            return Ok(());
                        }
                        if tutti_platform::Instant::now() >= deadline {
                            // No answer is a refusal, not a dead connection: the
                            // plugin may still be re-activating. Its late reply
                            // is taken off the stream when it arrives
                            // (`recv_reply`) and settles the state.
                            inbox.expect_late_rate_reply();
                            apply_rate_reply(
                                lifecycle,
                                inbox,
                                rate,
                                Some(format!(
                                    "the plugin did not answer the rate change within {budget:?}"
                                )),
                            );
                            break;
                        }
                        continue;
                    }
                    Some(reply) => reply,
                };
                match reply {
                    // A refusal is not a failure of the connection: the plugin
                    // stays loaded, the server renders silence for it, and the
                    // next rate change retries it.
                    ControlReply::SampleRateSet { rate, error } => {
                        // Recorded before the blocks go on, a refusal and its
                        // clearing alike: a block that finds the old refusal
                        // would latch it (an offline fork's wait does).
                        apply_rate_reply(lifecycle, inbox, rate, error);
                        break;
                    }
                    other => {
                        return Err(BridgeError::UnexpectedMessage {
                            expected: "SampleRateSet",
                            got: format!("{other:?}").into(),
                        });
                    }
                }
            }
            applied.send(());
        }
        Setting::RenderMode(mode) => {
            ipc::send(stream, &ControlRequest::SetRenderMode { mode })?;
            fence(stream, inbox, owed, lifecycle)?;
            applied.send(());
        }
        Setting::Fence => {
            fence(stream, inbox, owed, lifecycle)?;
            applied.send(());
        }
    }
    Ok(())
}

/// Reads what the plugin said unprompted off the control connection: each
/// frame is filed into `inbox` as `next_reply` reads it, and what is left over
/// is an answer owed to nobody.
pub(super) fn read_news(lane: &mut Lane<Owed>, inbox: &mut Unsolicited) -> BridgeResult<()> {
    let Lane { stream, owed } = lane;
    while next_reply(stream, inbox, owed, DRAIN_UNSOLICITED)?.is_some() {}
    Ok(())
}

/// A query's reply, within [`Timeouts::param`](tutti_plugin_types::Timeouts::param): `None` when it does not come
/// in time, which the query answers as "cannot be asked" (and the reply is
/// owed). A timeout here was a `?` that crashed a healthy bridge.
fn ask(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    owed: &mut Owed,
    lifecycle: &Lifecycle,
    answers: fn(&ControlReply) -> bool,
) -> BridgeResult<Option<ControlReply>> {
    answer(
        stream,
        inbox,
        owed,
        lifecycle.timeouts().param,
        Late::Answer(answers),
    )
}

/// Collect a chunked `StateChunk` sequence into one state.
///
/// Returns the caller's `BridgeResult` rather than propagating, because none of the
/// ways this fails is a connection failure: an over-limit state, a misordered
/// sequence and a stalled one are all *this plugin's* problem, and crashing the
/// bridge for any of them would take down a session whose socket is fine
/// ([`StateError::Stalled`] promises the caller a healthy session to retry
/// against). Only a genuine transport error (`?`) still propagates, and that
/// is the one case where the stream really is unusable.
///
/// **Whenever this stops before the answer has ended, the rest is owed**
/// ([`Late::StateRest`]): the server sends the whole sequence whatever this end
/// made of it, and a later request must not read its chunks as its answer.
///
/// Each chunk gets the full progress deadline, not a share of it: the budget is
/// per reply, and a plugin serialising a large state can legitimately pause
/// between chunks. The overall wait is still bounded, because a sequence that
/// stops arriving fails on the next chunk's own deadline. The deadline comes
/// from `progress`, so it is the same figure the waiting caller uses, and
/// `progress` is advanced per chunk so the *caller's* wait is bounded the same
/// way: this bounds the bridge thread's read, that the main thread's block on
/// the reply, and a fix to only one leaves the other as the ceiling.
fn recv_state(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    owed: &mut Owed,
    progress: &StateProgress,
) -> BridgeResult<std::result::Result<Vec<u8>, StateError>> {
    let mut acc = Reassembler::default();
    loop {
        let Some(msg) = next_reply(stream, inbox, owed, progress.deadline())? else {
            owed.owe(Late::StateRest);
            return Ok(Err(progress.stalled()));
        };
        let ControlReply::StateChunk { seq, last, bytes } = msg else {
            // A reply of another type is the whole answer (the server's
            // `Error` when the plugin cannot save); the state is not coming.
            return Ok(Err(StateError::Rejected(format!(
                "unexpected reply while reading plugin state: {msg:?}"
            ))));
        };
        progress.advance(bytes.len());
        let refused = match acc.push(seq, last, &bytes) {
            Ok(true) => return Ok(Ok(acc.into_inner())),
            Ok(false) => continue,
            Err(ChunkError::TooLarge { bytes, limit }) => StateError::TooLarge { bytes, limit },
            Err(ChunkError::OutOfOrder { got, expected }) => StateError::Rejected(format!(
                "plugin sent state chunk {got} where {expected} was expected"
            )),
            Err(ChunkError::Empty { seq }) => StateError::Rejected(format!(
                "plugin sent an empty state chunk {seq} before the last"
            )),
        };
        if !last {
            owed.owe(Late::StateRest);
        }
        return Ok(Err(refused));
    }
}

/// Record the plugin's answer to a rate change: the refusal while it refuses,
/// cleared (and heard, if there was one) when it takes the rate.
pub(super) fn apply_rate_reply(
    lifecycle: &Lifecycle,
    inbox: &mut Unsolicited,
    rate: SampleRate,
    error: Option<String>,
) {
    match error {
        Some(reason) => {
            lifecycle.set_rate_refusal(Some((rate, reason.clone())));
            inbox.rate_refusal(Some((rate, reason)));
        }
        None => {
            if lifecycle.rate_refusal().is_some() {
                lifecycle.set_rate_refusal(None);
                inbox.rate_refusal(None);
            }
        }
    }
}

/// Drain unsolicited events into `inbox`, returning the next reply-type message
/// within `timeout`.
///
/// The timeout is one deadline for the whole call: each read gets what is left
/// of it. Restarting it per read let a plugin that keeps sending events (a GUI
/// drag, a meter) hold the bridge past every budget.
fn recv_reply(
    stream: &mut ControlStream,
    inbox: &mut Unsolicited,
    timeout: Duration,
    partial: &mut PartialFrame,
) -> BridgeResult<ControlReply> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if let Some(reply) = absorb(
            inbox,
            ipc::recv_resumable::<ControlFrame>(stream, left, partial)?,
        ) {
            return Ok(reply);
        }
    }
}

/// Put an unsolicited event into `inbox`; anything else is a reply, returned.
fn absorb(inbox: &mut Unsolicited, frame: ControlFrame) -> Option<ControlReply> {
    let msg = match frame {
        ControlFrame::Reply(reply) => return absorb_reply(inbox, reply),
        ControlFrame::News(news) => news,
    };
    match msg {
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
    None
}

/// Files the one reply that is no request's answer once its request gave up: a
/// rate change's, held for the thread to apply and never read as another
/// request's reply.
fn absorb_reply(inbox: &mut Unsolicited, reply: ControlReply) -> Option<ControlReply> {
    match reply {
        ControlReply::SampleRateSet { rate, error } => {
            if inbox.late_rate_reply(rate, error.clone()) {
                None
            } else {
                Some(ControlReply::SampleRateSet { rate, error })
            }
        }
        reply => Some(reply),
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

    fn chunk(seq: u32, last: bool) -> ControlReply {
        ControlReply::StateChunk {
            seq,
            last,
            bytes: vec![1; 4],
        }
    }

    fn is_value(m: &ControlReply) -> bool {
        matches!(m, ControlReply::ParameterValue { .. })
    }

    fn audio_owed() -> (AudioOwed, super::super::channels::Replies, Channels) {
        let (writer, replies, _lost) = response_ring();
        let channels = Channels::new(SampleRate::SR_48K, std::sync::Arc::default())
            .expect("the channels")
            .0;
        (
            AudioOwed::new(Timeouts::default().drain, writer),
            replies,
            channels,
        )
    }

    /// **What the audio lane owes is taken first, in order, and the next
    /// request's answer is its own**. An abandoned block's reply is forwarded
    /// and counted late; an abandoned `ApplyParameters`' acknowledgement is
    /// taken without answering anything; the frame after them is the answer.
    #[test]
    fn owed_audio_frames_are_taken_in_order_before_the_answer() {
        let (mut owed, mut replies, channels) = audio_owed();
        channels.note_submitted(3);
        owed.owe(AudioLate::Block);
        owed.owe(AudioLate::Params);

        assert!(owed.take(processed(3), &channels).is_none());
        assert!(matches!(
            replies.pop(),
            Some(AudioResponse::AudioProcessed { seq: 3, .. })
        ));
        assert_eq!(channels.block_counts().late, 1);
        assert!(
            owed.take(AudioReply::ParametersApplied, &channels)
                .is_none()
        );
        assert!(owed.late.is_empty());

        channels.note_submitted(4);
        assert!(matches!(
            owed.take(processed(4), &channels),
            Some(AudioReply::AudioProcessed { seq: 4, .. })
        ));
        assert_eq!(channels.block_counts().late, 1, "an answer is not late");
    }

    /// **A frame of another kind lets an owed audio answer go**, and the
    /// server's `Error` answers whatever is owed.
    #[test]
    fn a_frame_of_another_kind_lets_an_owed_audio_answer_go() {
        let (mut owed, _replies, channels) = audio_owed();
        channels.note_submitted(5);
        owed.owe(AudioLate::Params);
        assert!(matches!(
            owed.take(processed(5), &channels),
            Some(AudioReply::AudioProcessed { .. })
        ));
        assert!(owed.late.is_empty());
        owed.owe(AudioLate::Block);
        let error = AudioReply::Error {
            message: "refused".into(),
        };
        assert!(owed.take(error, &channels).is_none());
        assert!(owed.late.is_empty());
    }

    /// **What the control lane owes is taken first, in order**: a query's late
    /// reply and the rest of a stalled state are taken without answering
    /// anything; the frame after them is the answer.
    #[test]
    fn owed_control_frames_are_taken_in_order_before_the_answer() {
        let mut owed = Owed::new(Timeouts::default().drain);
        owed.owe(Late::Answer(is_value));
        owed.owe(Late::StateRest);
        let value = ControlReply::ParameterValue {
            value: Some(tutti_units::Normalized::of(0.25)),
        };
        assert!(owed.take(value).is_none());
        assert!(owed.take(chunk(0, false)).is_none());
        assert!(owed.take(chunk(1, true)).is_none());
        assert!(owed.late.is_empty());

        let list = ControlReply::ParameterList {
            parameters: Vec::new(),
        };
        assert!(matches!(
            owed.take(list),
            Some(ControlReply::ParameterList { .. })
        ));
    }

    /// **An owed answer that is not coming does not swallow the replies after
    /// it.** A frame of another kind lets the owed item go and is weighed
    /// against the next; the server's `Error` answers any request.
    #[test]
    fn a_frame_of_another_kind_lets_an_owed_answer_go() {
        let list = || ControlReply::ParameterList {
            parameters: Vec::new(),
        };

        let mut owed = Owed::new(Timeouts::default().drain);
        owed.owe(Late::Answer(is_value));
        owed.owe(Late::StateRest);
        assert!(matches!(
            owed.take(list()),
            Some(ControlReply::ParameterList { .. })
        ));
        assert!(owed.late.is_empty());

        owed.owe(Late::Answer(is_value));
        let error = ControlReply::Error {
            message: "refused".into(),
        };
        assert!(owed.take(error).is_none());
        assert!(owed.late.is_empty());
    }

    /// **A carried value goes into a reserved queue of the block's buffer**,
    /// so the buffer returns to the pool with its point room intact. A
    /// queue pushed past the spares (the carry's own, with the inline room)
    /// later took a reserved queue's place on `clear` and left the buffer
    /// with a 10-point queue among its reserved ones.
    #[test]
    fn a_carried_value_fills_a_reserved_queue() {
        let (mut owed, _replies, _channels) = audio_owed();
        let mut skipped = ParameterChanges::reserved(1, 1);
        skipped.add_change(ParamId::new(7), 3, 0.25);
        owed.carry(&skipped);

        let mut block = ParameterChanges::reserved(4, 40);
        assert!(block.open_queue(ParamId::new(1)));
        if let Some(q) = block.queues.last_mut() {
            q.add_point(5, 0.5);
        }
        owed.merge_carried(&mut block);

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

        let mut owed = audio_owed().0;
        owed.carry(&skipped);
        let mut block = ParameterChanges::reserved_bridged(1, 40);
        assert!(block.add_change(ParamId::new(1), 5, 0.5));
        assert_no_alloc::assert_no_alloc(|| owed.merge_carried(&mut block));
        assert_eq!(block.queues.len(), 1);
        assert_eq!(block.points_overflowed(), 2);

        let mut owed = audio_owed().0;
        owed.carry(&skipped);
        let mut placeholder = ParameterChanges::new();
        owed.merge_carried(&mut placeholder);
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
    /// left a value owed for every parameter. Inside the no-alloc gate,
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
        let mut owed = audio_owed().0;
        owed.carry(&skipped);

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

        assert_no_alloc::assert_no_alloc(|| owed.merge_carried(&mut block));

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
