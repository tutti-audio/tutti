//! The queues between the host's threads and the bridge's two lane threads.
//!
//! Three queues live at this level, one per lane and the response ring. The
//! block queue (the audio thread → the audio lane's thread) and the control
//! queue (any caller → the control lane's thread) each have their own wake-up,
//! so a block is never queued behind a control request. The audio-response ring
//! (the audio lane's thread → the audio thread, RT) has owned ends:
//! [`ResponseWriter`] belongs to the audio lane's thread alone, whatever the
//! control lane learns, and [`Replies`] to the pipeline that collects chunks.
//! Reply-bearing commands carry their own per-request `Reply<T>` so each
//! response is routed back to its specific caller — no shared
//! control-response queue, no risk of mismatched responses.
//!
//! `ProcessPayload` recycling lives in [`PayloadPool`](super::payload_pool::PayloadPool),
//! a separate primitive whose `acquire`/`recycle` names document the RT
//! contract.

use super::ask::ControlSlot;
use super::ask::Reply;
use super::messages::{AudioCommand, AudioResponse, ControlCommand, Setting};
use crate::transport::control::{ControlUnparker, ControlWait, control_unparker};
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tutti_plugin_types::EditReader;
use tutti_plugin_types::PipelineCounts;
use tutti_rt::AtomicSampleRate;
use tutti_rt::AudioThreadCell;
use tutti_rt::{
    Counter, Mutex, Overflow, PackedAtomic, Parker, RingReader, RingWriter, Tally, TallyWriter,
    Unparker, parker, ring, tally,
};
use tutti_units::{Chunks, Normalized, ParamId, SampleRate};

/// How many commands each lane's queue holds before a push fails. The block
/// queue's size also bounds how many `Process` payloads can be outstanding at
/// once, which is why [`super::payload_pool::PayloadPool`] sizes itself from it.
pub(crate) const COMMAND_QUEUE_SIZE: usize = 128;
const RESPONSE_QUEUE_SIZE: usize = 128;

/// A control command with the newest block sequence submitted when it was
/// pushed: the control lane sends it once the audio lane has dealt with that
/// block. Two lanes' threads order nothing by themselves, and a request that
/// reached the server before the blocks pushed ahead of it (a `save_state`
/// that misses a block's parameter writes) would read a state the host has
/// already moved past.
pub(super) struct Sequenced<T> {
    pub(super) after: u64,
    pub(super) cmd: T,
}

#[derive(Clone)]
pub(super) struct Channels {
    /// What rides the audio lane: blocks, resets, render modes.
    blocks: Arc<ArrayQueue<AudioCommand>>,
    /// What rides the control lane: every other request.
    controls: Arc<ArrayQueue<Sequenced<ControlCommand>>>,
    /// The newest block sequence **pushed** (queued, not merely submitted):
    /// stored after a push succeeds, so a refused push never advances it and a
    /// block counted here is already in the queue. A control command is stamped
    /// with it and the idle lane publishes it; `newest_seq`, which
    /// `note_submitted` sets before the push, only judges staleness.
    pushed: Arc<AtomicU64>,
    /// The newest block sequence the audio lane has dealt with: answered by the
    /// server (or abandoned at its budget), or skipped as stale. A control command stamped with a sequence
    /// waits for it ([`Sequenced`]), so it never reaches the server ahead of a
    /// block submitted before it.
    handled: Arc<AtomicU64>,
    /// Whether a control request owes a reply: one at a time.
    owing: ControlSlot,
    /// The settings the audio lane has written and the control lane has not
    /// taken: one slot per kind, the latest value, never a queue.
    settings: Arc<Mutex<Settings>>,
    /// What callers asked for, before the audio lane takes it up: a slot, never
    /// a queue, so a full block queue (a producer racing a held lane) cannot
    /// lose one.
    requests: Arc<Mutex<Requests>>,
    /// The newest block sequence the audio thread has submitted. Owned here
    /// rather than on the batcher because the *bridge* thread is what reads it,
    /// to decide whether a dequeued block is still worth sending.
    newest_seq: Arc<AtomicU64>,
    /// The audio lane's unparker: its thread parks on the matching [`Parker`]
    /// when the block queue is empty, and the audio thread unparks it after a
    /// push. The unpark is wait-free (no lock, no allocation) and cannot be
    /// missed ([`Unparker`]'s protocol), so the thread needs no polling
    /// timeout.
    unparker: Unparker,
    /// The control lane's bell: its thread waits on the control stream and this
    /// together, so a request or the server's news ends the wait at once.
    control_unparker: ControlUnparker,
    /// The negotiated sample rate, as `f64::to_bits`. Lives here rather than
    /// only on `AudioBridge` because *both* threads size a timeout from the
    /// block period — the bridge thread its reply timeout, and the staleness
    /// bound its notion of how far behind a block may be. One shared source
    /// keeps the two from drifting apart: two independently-derived periods let
    /// a constant sit orders of magnitude off the real block period, which
    /// starves the command queue.
    sample_rate: Arc<AtomicSampleRate>,
    /// How long the bridge thread waits for a block's reply, in nanoseconds,
    /// when it is not the live bound (the block period's few multiples, from
    /// the dispatcher): `0` is the live bound, so a set wait is never stored as
    /// `0`. An offline render has no deadline, so a reply the server is slow
    /// to send is the server's, not an abandoned block's.
    reply_wait_ns: Arc<AtomicU64>,
    /// The host's parameter writes the audio lane drains into the next block it
    /// sends ([`AudioBridge::write_parameter`](super::AudioBridge::write_parameter)):
    /// the one reader of the slots the host's writer fills, `None` until the
    /// first write names the plugin's parameters. Claimed, never locked: the
    /// audio lane's thread claims it without waiting at each block (a refused
    /// claim drains at the next), and a control thread that installs a reader or
    /// flushes claims it waiting.
    host_edits: Arc<AudioThreadCell<Option<EditReader>>>,
    /// How many chunks the bridge thread abandoned on a missed budget, ever:
    /// a loss, and with [`replies`](Self::replies) the hang verdict's input.
    abandoned: Tally<Chunks>,
    /// The bridge thread's writer of `abandoned`, minted in `new`.
    abandoned_writer: TallyWriter<Chunks>,
    /// How many owed replies the bridge thread collected after abandoning the
    /// chunks that asked for them, ever: the other half of `abandoned`.
    late: Tally<Chunks>,
    /// The bridge thread's writer of `late`, minted in `new`.
    late_writer: TallyWriter<Chunks>,
    /// Commands the audio side could not hand to the bridge thread, ever: a
    /// full command queue refused one, the payload pool had no payload for a
    /// block, or a parameter buffer had no room to come back to. Written
    /// through any clone of the channels, read by the plugin node's controls.
    commands_refused: Tally<u64>,
    /// The writer of `commands_refused`, minted in `new`.
    commands_refused_writer: TallyWriter<u64>,
    /// Render-mode and rate changes the audio lane gave up on, ever: the plugin
    /// did not apply the change within the fence deadline. The change may still
    /// land; the blocks after it were not held for it.
    unapplied: Tally<u64>,
    /// The audio lane's writer of `unapplied`, minted in `new`.
    unapplied_writer: TallyWriter<u64>,
    /// Parameter buffers the audio thread leaked rather than free, ever: the
    /// free list had no room for one it returned (a broken sizing bound).
    leaked_buffers: Tally<u64>,
    /// The writer of `leaked_buffers`, minted in `new`.
    leaked_writer: TallyWriter<u64>,
    /// How many replies the bridge thread has taken from the plugin, ever,
    /// on time or owed. With [`abandoned`](Self::abandoned) the hang
    /// verdict's input (`PluginHandle::block_counts`): a hang is abandoned
    /// blocks growing while this does not. Written by the bridge thread only.
    /// A block's reply is counted after it is queued, `Release` against the
    /// `Acquire` read in [`block_counts`](Self::block_counts): a reader that
    /// sees the count finds the reply queued.
    replies: Arc<AtomicU64>,
    /// How many unsolicited parameter events the bridge thread dropped
    /// because more arrived during one command than its inbox holds, ever.
    /// Each drop also raises a `Resync(ParamValues)`, so the host re-reads.
    dropped_param_events: Tally<u64>,
    /// The bridge thread's writer of `dropped_param_events`, minted in `new`.
    param_events_writer: TallyWriter<u64>,
    /// MIDI events the bridge thread did not send, ever: a stale block's,
    /// and the rest of a SysEx that lost packets with it. Shared with the
    /// plugin node's controls, which add it to the node's own count.
    dropped_midi: Tally<u64>,
    /// The bridge thread's writer of `dropped_midi`, minted in `new`.
    midi_writer: TallyWriter<u64>,
    /// Chunks the bridge thread skipped as stale, ever: more than
    /// `MAX_BEHIND` behind the newest submitted, so their slab slot is the
    /// host's again. Shared with the pipeline's counters
    /// (`ChunkCounters::attach_skipped`).
    skipped: Tally<Chunks>,
    /// The bridge thread's writer of `skipped`, minted in `new`.
    skipped_writer: TallyWriter<Chunks>,
    /// What became of each of the last [`BATCH_OUTCOMES`] chunks the bridge thread
    /// dealt with, for the batcher to look up.
    fates: Arc<BatchOutcomes>,
    /// The control thread is reading the control connection for news: set when
    /// its wait ended on the stream, cleared after the lane is read dry and
    /// the events are delivered.
    news: Arc<std::sync::atomic::AtomicBool>,
    /// A watch on the control connection, kept while the control thread runs:
    /// [`bytes_waiting`](Self::bytes_waiting) asks it whether bytes the thread
    /// has not read are there, which `news` alone misses before the thread's
    /// wait has returned.
    probe: Arc<Mutex<Option<Arc<crate::transport::control::Watch>>>>,
    /// Notified on a lane's thread when a response is queued and when it has
    /// finished a turn (a lane read dry, a command answered): what a control or
    /// worker thread waiting on the bridge sleeps on. The server's watch
    /// notifies it too, so a waiter wakes at the server's end.
    progress: Arc<crate::Wake>,
}

/// How many chunks back [`BatchOutcomes`] remembers.
pub(crate) const BATCH_OUTCOMES: usize = 16;

/// What the bridge thread learned of a chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatchOutcome {
    /// The server published its audio: the reply said so.
    Published,
    /// The server did not publish it: its input slot had been recycled.
    Refused,
    /// The bridge never sent it: more than a chunk behind.
    Skipped,
}

/// The last [`BATCH_OUTCOMES`] chunks' fates, one word each: the sequence above two
/// bits of fate, so a lookup of a sequence the entry no longer holds finds
/// nothing. Written by the bridge thread alone, read on the audio thread.
///
/// Each word stands alone and nothing is published through it (the audio is
/// gated by the slab's sequence words), so a reader sees a fate or not yet,
/// and tries again at its next look. The cell's `Acquire`/`Release` is its
/// own contract and nothing here relies on it.
struct BatchOutcomes([PackedAtomic<u64>; BATCH_OUTCOMES]);

impl BatchOutcomes {
    fn new() -> Self {
        Self(std::array::from_fn(|_| PackedAtomic::new(0)))
    }

    /// Stores `seq`'s fate, unless the word already holds a newer chunk's: a
    /// late reply to an old chunk (a server that stalled for more than
    /// [`BATCH_OUTCOMES`] chunks) must not erase the newer fate. A plain load decides:
    /// only the bridge thread writes.
    fn record(&self, seq: u64, fate: BatchOutcome) {
        let word = &self.0[(seq % BATCH_OUTCOMES as u64) as usize];
        if unpack(word.load()).0 > seq {
            return;
        }
        word.store(pack(seq, fate.code()));
    }

    fn get(&self, seq: u64) -> Option<BatchOutcome> {
        let (held, code) = unpack(self.0[(seq % BATCH_OUTCOMES as u64) as usize].load());
        if held != seq {
            return None;
        }
        BatchOutcome::from_code(code)
    }
}

/// Bits of a [`BatchOutcomes`] word that hold the fate's code; the sequence is above.
const CODE_BITS: u32 = 2;

/// The word for `seq` with fate `code`: the code in the low [`CODE_BITS`] bits,
/// the sequence above them.
fn pack(seq: u64, code: u64) -> u64 {
    seq << CODE_BITS | code
}

/// The sequence and the fate code a [`pack`]ed word holds; code 0 is an empty
/// word's.
fn unpack(word: u64) -> (u64, u64) {
    (word >> CODE_BITS, word & ((1 << CODE_BITS) - 1))
}

impl BatchOutcome {
    /// The fate's code in a word: 1, 2 or 3, never 0 (an empty word's).
    fn code(self) -> u64 {
        match self {
            Self::Published => 1,
            Self::Refused => 2,
            Self::Skipped => 3,
        }
    }

    /// The fate a [`code`](Self::code) stands for; `None` for 0.
    fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::Published),
            2 => Some(Self::Refused),
            3 => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// The settings written and not yet taken, each with the reply that tells the
/// audio lane it was applied. A kind written again replaces the one waiting (the
/// replaced reply drops, which ends its waiter): the latest value stands.
#[derive(Default)]
struct Settings {
    rate: Option<(tutti_units::SampleRate, Reply<()>)>,
    mode: Option<(tutti_plugin_types::RenderMode, Reply<()>)>,
    fence: Option<Reply<()>>,
    /// The control lane has ended: a setting put now is dropped at once.
    closed: bool,
}

/// The settings a caller has requested and the audio lane has not taken, each
/// with the newest block sequence submitted when it was requested: the blocks
/// after it (a greater sequence) wait for it. A kind requested again replaces
/// the one waiting: the latest value stands, **and so does its stamp** (the
/// blocks between the two requests are held for the later value too).
#[derive(Default)]
struct Requests {
    rate: Option<(tutti_units::SampleRate, u64)>,
    mode: Option<(tutti_plugin_types::RenderMode, u64)>,
}

/// The waits of the two lane threads, each moved to the thread that waits.
pub(super) struct Waits {
    /// The audio lane's parker.
    pub(super) audio: Parker,
    /// The control lane's wait on its stream and bell.
    pub(super) control: ControlWait,
}

impl Channels {
    /// The channels and the [`Waits`] of the two lanes' threads, which each
    /// wait alone on theirs.
    ///
    /// # Errors
    ///
    /// The OS refused the control lane's bell.
    pub(super) fn new(
        sample_rate: SampleRate,
        progress: Arc<crate::Wake>,
    ) -> std::io::Result<(Self, Waits)> {
        let (unparker, parker) = parker();
        let (control_unparker, control_wait) = control_unparker()?;
        let (dropped_param_events, dropped_midi, skipped) = (tally(), tally(), tally());
        let (abandoned, late, commands_refused, leaked_buffers, unapplied) =
            (tally(), tally(), tally(), tally(), tally());
        let channels = Self {
            blocks: Arc::new(ArrayQueue::new(COMMAND_QUEUE_SIZE)),
            controls: Arc::new(ArrayQueue::new(COMMAND_QUEUE_SIZE)),
            handled: Arc::new(AtomicU64::new(0)),
            pushed: Arc::new(AtomicU64::new(0)),
            owing: ControlSlot::default(),
            settings: Arc::default(),
            requests: Arc::default(),
            newest_seq: Arc::new(AtomicU64::new(0)),
            unparker,
            control_unparker,
            sample_rate: Arc::new(AtomicSampleRate::new(sample_rate)),
            reply_wait_ns: Arc::new(AtomicU64::new(0)),
            host_edits: Arc::new(AudioThreadCell::new(None)),
            abandoned_writer: abandoned.writer(),
            abandoned,
            late_writer: late.writer(),
            late,
            commands_refused_writer: commands_refused.writer(),
            commands_refused,
            unapplied_writer: unapplied.writer(),
            unapplied,
            leaked_writer: leaked_buffers.writer(),
            leaked_buffers,
            replies: Arc::new(AtomicU64::new(0)),
            param_events_writer: dropped_param_events.writer(),
            dropped_param_events,
            midi_writer: dropped_midi.writer(),
            dropped_midi,
            skipped_writer: skipped.writer(),
            skipped,
            fates: Arc::new(BatchOutcomes::new()),
            news: Arc::default(),
            probe: Arc::default(),
            progress,
        };
        Ok((
            channels,
            Waits {
                audio: parker,
                control: control_wait,
            },
        ))
    }

    /// Installs the reader of the slots the host's writer fills, replacing the
    /// previous one: what it had not yielded goes with it. Control side: waits
    /// for a drain that holds the cell.
    pub(super) fn install_host_edits(&self, reader: EditReader) {
        *self.claim_host_edits() = Some(reader);
    }

    /// Takes the cell, waiting while the audio lane drains. Control side.
    fn claim_host_edits(&self) -> tutti_rt::BorrowGuard<'_, Option<EditReader>> {
        loop {
            if let Some(guard) = self.host_edits.try_borrow_mut() {
                return guard;
            }
            std::thread::yield_now();
        }
    }

    /// Hands `apply` each parameter the host wrote since the last drain, with
    /// its latest value. The audio lane's thread, once per block: a refused
    /// claim (a control thread flushes) drains nothing, and the slots keep what
    /// they hold.
    pub(super) fn drain_host_edits(&self, apply: impl FnMut(ParamId, Normalized)) {
        if let Some(mut cell) = self.host_edits.try_borrow_mut()
            && let Some(reader) = cell.as_mut()
        {
            reader.drain(apply);
        }
    }

    /// What no block has taken, claiming the cell: a flush's, which sends it
    /// ahead of whatever it must precede. Control side.
    pub(super) fn take_host_edits(&self) -> Vec<(ParamId, Normalized)> {
        let mut taken = Vec::new();
        if let Some(reader) = self.claim_host_edits().as_mut() {
            reader.drain(|id, value| taken.push((id, value)));
        }
        taken
    }

    /// The current sample rate. `Relaxed` suffices: it only sizes a timeout, so
    /// reading a value one block stale is harmless.
    pub(super) fn sample_rate(&self) -> SampleRate {
        self.sample_rate.load(Ordering::Relaxed)
    }

    pub(super) fn set_sample_rate(&self, rate: SampleRate) {
        self.sample_rate.store(rate, Ordering::Relaxed);
    }

    /// The bridge thread's wait for a block's reply, when it is not the live
    /// one.
    pub(super) fn reply_wait(&self) -> Option<Duration> {
        match self.reply_wait_ns.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(Duration::from_nanos(ns)),
        }
    }

    /// Sets the wait, `None` for the live bound. `None` is stored as the `0`
    /// sentinel, and a wait is raised to 1 ns so a zero wait is not read back
    /// as the live bound; one past `u64::MAX` ns saturates.
    pub(super) fn set_reply_wait(&self, wait: Option<Duration>) {
        let ns = wait.map_or(0, |w| {
            u64::try_from(w.as_nanos()).unwrap_or(u64::MAX).max(1)
        });
        self.reply_wait_ns.store(ns, Ordering::Relaxed);
    }

    /// Park the audio lane's thread until a block-lane command is queued or
    /// `stop` says to leave, whichever is already true or comes first. May
    /// return with neither (a spurious wake-up): the caller rechecks. `stop`
    /// reads what whoever ends the loop publishes before calling
    /// [`wake`](Self::wake). `parker` is the one [`new`](Self::new) made.
    pub(super) fn wait_for_block(&self, parker: &mut Parker, stop: impl FnOnce() -> bool) {
        parker.park(|| !self.blocks.is_empty() || self.requested() || stop());
    }

    /// Asks for `setting` to be applied, ordered against the blocks: the blocks
    /// submitted after this call are held for it. Latest value per kind; never
    /// refused. Wakes the audio lane's thread. Not for the audio thread (a lock).
    pub(super) fn request_setting(&self, setting: Setting) {
        let after = self.newest_submitted();
        {
            let mut slots = self.requests.lock();
            match setting {
                Setting::SampleRate(rate) => slots.rate = Some((rate, after)),
                Setting::RenderMode(mode) => slots.mode = Some((mode, after)),
                // A fence rides the audio queue (`AudioCommand::Settle`), never
                // a request.
                Setting::Fence => {}
            }
        }
        self.unparker.unpark();
    }

    /// Whether a requested setting waits for the audio lane.
    fn requested(&self) -> bool {
        let slots = self.requests.lock();
        slots.rate.is_some() || slots.mode.is_some()
    }

    /// The requested setting the block `before` must wait for (one requested
    /// while a lesser sequence was the newest), or any with `None`; a rate
    /// before a render mode.
    pub(super) fn take_request(&self, before: Option<u64>) -> Option<Setting> {
        let mut slots = self.requests.lock();
        let due = |after: u64| before.is_none_or(|seq| seq > after);
        if let Some((rate, after)) = slots.rate
            && due(after)
        {
            slots.rate = None;
            return Some(Setting::SampleRate(rate));
        }
        if let Some((mode, after)) = slots.mode
            && due(after)
        {
            slots.mode = None;
            return Some(Setting::RenderMode(mode));
        }
        None
    }

    /// The control thread is reading the control connection for news.
    pub(super) fn set_news(&self) {
        self.news.store(true, Ordering::Release);
        // Pairs with the fence after `bytes_waiting`'s poll: this flag is
        // visible before the bytes it covers are taken off the connection, so
        // a checker that saw the bytes gone and then fenced reads it set.
        std::sync::atomic::fence(Ordering::SeqCst);
    }

    /// Whether [`set_news`](Self::set_news) was called since the last
    /// [`clear_news`](Self::clear_news).
    pub(super) fn news_pending(&self) -> bool {
        self.news.load(Ordering::Acquire)
    }

    /// Keeps `watch` as the probe [`bytes_waiting`](Self::bytes_waiting) asks.
    pub(super) fn set_probe(&self, watch: Arc<crate::transport::control::Watch>) {
        *self.probe.lock() = Some(watch);
    }

    /// Lets the connection's duplicate go once nothing watches it.
    pub(super) fn clear_probe(&self) {
        *self.probe.lock() = None;
    }

    /// Whether the control connection has bytes the control thread has not read
    /// (one `poll`, no wait). `false` with no probe. A fence follows the poll,
    /// so a [`news_pending`](Self::news_pending) read after this sees a flag
    /// the control thread set before it took the bytes.
    pub(super) fn bytes_waiting(&self) -> bool {
        let waiting = self
            .probe
            .lock()
            .as_ref()
            .is_some_and(|watch| watch.wait_readable(Duration::ZERO));
        std::sync::atomic::fence(Ordering::SeqCst);
        waiting
    }

    /// The control thread has read the control connection dry and delivered
    /// what it said.
    pub(super) fn clear_news(&self) {
        self.news.store(false, Ordering::Release);
    }

    /// Wake both lanes' threads to re-check their stop condition (shutdown, a
    /// crash). Publish the condition first. Control and bridge side only, not
    /// the audio thread: the progress notify takes a lock.
    pub(super) fn wake(&self) {
        self.unparker.unpark();
        self.control_unparker.unpark();
        self.progress.notify();
    }

    /// A call that wakes both lanes' threads, for what has no `Channels` to
    /// hold: it holds the two bells and no queue.
    pub(super) fn waker(&self) -> impl Fn() + Send + Sync + use<> {
        let (unparker, control_unparker) = (self.unparker.clone(), self.control_unparker.clone());
        move || {
            unparker.unpark();
            control_unparker.unpark();
        }
    }

    /// Record that the audio thread has submitted block `seq`. Called from the
    /// RT path, so it is a single relaxed store and nothing else.
    pub(super) fn note_submitted(&self, seq: u64) {
        self.newest_seq.store(seq, Ordering::Relaxed);
    }

    /// The newest block the audio thread has submitted.
    ///
    /// The bridge thread uses this to tell how far behind a command it just
    /// dequeued is, without draining the queue to look. `Relaxed` suffices: it
    /// only decides whether to skip work that is already provably useless, so
    /// reading a value one block stale costs at most one extra dead block.
    pub(super) fn newest_submitted(&self) -> u64 {
        self.newest_seq.load(Ordering::Relaxed)
    }

    /// Whether block `seq` has been submitted: a reply naming a later one is
    /// the peer's invention.
    pub(super) fn sent(&self, seq: u64) -> bool {
        seq <= self.newest_submitted()
    }

    /// Record that the drain collected an owed reply off the socket: a block's
    /// reply that came after its abandonment, which is also a reply the plugin
    /// gave. Called by the bridge thread only.
    pub(super) fn note_late(&self) {
        self.late_writer.add(Chunks::new(1));
        self.replies.fetch_add(1, Ordering::Release);
    }

    /// Record that a block's reply missed its budget and is now owed. Called
    /// by the bridge thread only, before any settle of that reply.
    pub(super) fn note_abandoned(&self) {
        self.abandoned_writer.add(Chunks::new(1));
    }

    /// Record that the plugin answered (a reply for a block, on time or
    /// owed): it is not hung. Bridge thread only, after the reply is queued.
    pub(super) fn note_replied(&self) {
        self.replies.fetch_add(1, Ordering::Release);
    }

    /// The block counts over the life of the bridge. Separate reads: a reply
    /// landing between them shows up on the next call, which is all a poller
    /// needs. A counted reply is queued.
    pub(super) fn block_counts(&self) -> PipelineCounts {
        PipelineCounts {
            abandoned: self.abandoned.get(),
            late: self.late.get(),
            replies: Chunks::new(self.replies.load(Ordering::Acquire)),
        }
    }

    /// Record one command the audio side could not hand over. Wait-free, any
    /// thread.
    pub(super) fn note_command_refused(&self) {
        self.commands_refused_writer.add(1);
    }

    /// A writer of the cell [`note_command_refused`](Self::note_command_refused)
    /// counts into, for the pools that refuse beside the queue.
    pub(super) fn refusal_writer(&self) -> TallyWriter<u64> {
        self.commands_refused_writer.clone()
    }

    /// The cell [`note_command_refused`](Self::note_command_refused) counts into.
    pub(super) fn commands_refused_cell(&self) -> Tally<u64> {
        self.commands_refused.clone()
    }

    /// A writer of the cell the payload pool counts leaked buffers into.
    pub(super) fn leak_writer(&self) -> TallyWriter<u64> {
        self.leaked_writer.clone()
    }

    /// The cell of parameter buffers the audio thread leaked.
    pub(super) fn leaked_buffers_cell(&self) -> Tally<u64> {
        self.leaked_buffers.clone()
    }

    /// Record `n` dropped unsolicited parameter events. Bridge thread only; a
    /// count nothing branches on.
    pub(super) fn note_dropped_param_events(&self, n: u64) {
        if n > 0 {
            self.param_events_writer.add(n);
        }
    }

    /// Record `n` MIDI events not sent. Bridge thread only; a count nothing
    /// branches on.
    pub(super) fn note_dropped_midi(&self, n: u64) {
        if n > 0 {
            self.midi_writer.add(n);
        }
    }

    /// Record a chunk skipped as stale. Bridge thread only; a count nothing
    /// branches on.
    pub(super) fn note_skipped(&self) {
        self.skipped_writer.add(Chunks::new(1));
    }

    /// Record what became of chunk `seq`. Bridge thread only. A chunk not
    /// yet submitted has no fate: a peer-supplied `seq` far ahead would
    /// otherwise claim a word of the ring and erase the fates behind it.
    pub(super) fn note_outcome(&self, seq: u64, fate: BatchOutcome) {
        if self.sent(seq) {
            self.fates.record(seq, fate);
        }
    }

    /// What became of chunk `seq`, while it is among the last [`BATCH_OUTCOMES`].
    pub(super) fn fate(&self, seq: u64) -> Option<BatchOutcome> {
        self.fates.get(seq)
    }

    /// The cell [`note_skipped`](Self::note_skipped) counts into.
    pub(super) fn skipped_cell(&self) -> Tally<Chunks> {
        self.skipped.clone()
    }

    /// The cell [`note_dropped_midi`](Self::note_dropped_midi) counts into.
    pub(super) fn dropped_midi_cell(&self) -> Tally<u64> {
        self.dropped_midi.clone()
    }

    /// How many unsolicited parameter events were dropped, over the life of
    /// the bridge.
    pub(super) fn dropped_param_events(&self) -> u64 {
        self.dropped_param_events.get()
    }

    /// Pushes a block-lane command, then unparks the audio lane, so the
    /// socket round-trip starts immediately rather than after a poll interval.
    /// The audio thread never waits on the reply, but the unpark still matters: it
    /// is what keeps a block's reply arriving in time to be collected on the
    /// *next* block rather than the one after.
    ///
    /// The unpark is wait-free and allocation-free (`Unparker::unpark`: a fence
    /// and a load, plus a futex wake when the lane is parked), and it
    /// cannot be lost, so nothing backs it with a timeout. A push the queue
    /// refuses unparks nothing: the lane is, or is about to be, draining.
    ///
    /// A refused command is counted ([`note_command_refused`](Self::note_command_refused)).
    pub(super) fn push_block(&self, cmd: AudioCommand) -> bool {
        self.try_push_block(cmd).is_ok()
    }

    /// [`push_block`](Self::push_block), handing a refused command back
    /// rather than dropping it, so the audio thread can return what it holds
    /// to its pools instead of freeing it.
    pub(super) fn try_push_block(&self, cmd: AudioCommand) -> Result<(), AudioCommand> {
        if let Err(cmd) = self.blocks.push(cmd) {
            self.note_command_refused();
            return Err(cmd);
        }
        self.unparker.unpark();
        Ok(())
    }

    pub(super) fn pop_block(&self) -> Option<AudioCommand> {
        self.blocks.pop()
    }

    /// Pushes a control-lane command and rings the control lane's bell: at most
    /// one non-blocking write, no lock, no allocation. A refused command is
    /// counted.
    ///
    /// **The order against blocks is one way, by design.** A command never
    /// overtakes a block pushed before it: it is stamped with the newest block
    /// pushed and the control lane sends it only after the audio lane has dealt
    /// with that block. A command pushed before a block may still reach the
    /// server after it: holding the block for the command would let a stuck
    /// control request stall audio.
    pub(super) fn push_control(&self, cmd: ControlCommand) -> bool {
        self.push_control_back(cmd).is_ok()
    }

    /// [`push_control`](Self::push_control), giving the command back when the
    /// queue is full (counted).
    pub(super) fn push_control_back(&self, cmd: ControlCommand) -> Result<(), ControlCommand> {
        let after = self.pushed();
        if let Err(refused) = self.controls.push(Sequenced { after, cmd }) {
            self.note_command_refused();
            return Err(refused.cmd);
        }
        self.control_unparker.unpark();
        Ok(())
    }

    /// Records that block `seq` is in the block queue. Called after the push
    /// succeeds, by whoever pushed it.
    pub(super) fn note_pushed(&self, seq: u64) {
        self.pushed.fetch_max(seq, Ordering::Release);
    }

    /// The newest block sequence pushed.
    pub(super) fn pushed(&self) -> u64 {
        self.pushed.load(Ordering::Acquire)
    }

    /// Records that the audio lane has dealt with block `seq` (sent or skipped),
    /// or with every block submitted so far when it idles. The audio lane's
    /// thread only; a waiter is woken by the lane's turn-done notice.
    pub(super) fn note_handled(&self, seq: u64) {
        self.handled.fetch_max(seq, Ordering::Release);
    }

    /// The newest block sequence the audio lane has dealt with.
    pub(super) fn handled(&self) -> u64 {
        self.handled.load(Ordering::Acquire)
    }

    /// Whether a setting waits for the control lane.
    pub(super) fn setting_waiting(&self) -> bool {
        let slots = self.settings.lock();
        slots.rate.is_some() || slots.mode.is_some()
    }

    /// Writes `setting` into its slot and rings the control lane's bell: no
    /// queue, so no setting is refused. `applied` is answered once the control
    /// lane has applied it.
    ///
    /// **A setting jumps the control queue**: the control lane takes the slots
    /// before it pops a queued command, so a `SetParameter` queued before a
    /// render mode is sent after it. Across kinds the lane takes a rate before a
    /// render mode, which is also the order the audio lane's given-up settings
    /// are applied in.
    pub(super) fn put_setting(&self, setting: Setting, applied: Reply<()>) {
        {
            let mut slots = self.settings.lock();
            if slots.closed {
                // No lane takes it: dropping the reply ends its waiter.
                return;
            }
            match setting {
                Setting::SampleRate(rate) => slots.rate = Some((rate, applied)),
                Setting::RenderMode(mode) => slots.mode = Some((mode, applied)),
                Setting::Fence => slots.fence = Some(applied),
            }
        }
        self.control_unparker.unpark();
        // A control command waiting for blocks the audio lane is holding for
        // this setting must see it.
        self.progress.notify();
    }

    /// Ends the settings' slots: the control lane takes no more, so a setting
    /// waiting is dropped with its reply and one put later is dropped on
    /// arrival. Each waiter sees its reply disconnect. Control lane's thread,
    /// as it ends (stop or crash).
    pub(super) fn close_settings(&self) {
        let dropped = {
            let mut slots = self.settings.lock();
            slots.closed = true;
            (slots.rate.take(), slots.mode.take(), slots.fence.take())
        };
        drop(dropped);
    }

    /// The setting waiting for the control lane, if any: a rate first, then a
    /// render mode, then a fence.
    pub(super) fn take_setting(&self) -> Option<(Setting, Reply<()>)> {
        let mut slots = self.settings.lock();
        if let Some((rate, applied)) = slots.rate.take() {
            return Some((Setting::SampleRate(rate), applied));
        }
        if let Some((mode, applied)) = slots.mode.take() {
            return Some((Setting::RenderMode(mode), applied));
        }
        slots.fence.take().map(|applied| (Setting::Fence, applied))
    }

    /// Record a render-mode or rate change the audio lane gave up on. Audio
    /// lane's thread only.
    pub(super) fn note_unapplied(&self) {
        self.unapplied_writer.add(1);
    }

    /// The cell [`note_unapplied`](Self::note_unapplied) counts into.
    pub(super) fn unapplied_cell(&self) -> Tally<u64> {
        self.unapplied.clone()
    }

    pub(super) fn pop_control(&self) -> Option<Sequenced<ControlCommand>> {
        self.controls.pop()
    }

    /// The slot a request that owes a reply holds until it is answered.
    pub(super) fn owing(&self) -> &ControlSlot {
        &self.owing
    }

    /// The gate a waiter on the bridge's progress sleeps on.
    pub(super) fn progress(&self) -> &Arc<crate::Wake> {
        &self.progress
    }

    /// The bridge thread finished a turn: it answered what it was asked and
    /// read the lane it was woken for.
    pub(super) fn turn_done(&self) {
        self.progress.notify();
    }
}

/// The producing end of the audio-response ring, owned by the audio lane's
/// thread: a push can come from nowhere else, which makes the ring
/// single-producer. What the control lane learns (a crash, say) reaches it as a
/// wake-up of that thread, never as a push.
/// A full ring drops the newest response and the ring counts it.
pub(crate) struct ResponseWriter {
    ring: RingWriter<AudioResponse>,
}

/// The consuming end of the audio-response ring, owned by whatever collects
/// chunks (the pipeline's batcher): a pop can come from nowhere else, which
/// makes the ring single-consumer.
#[must_use]
pub struct Replies {
    ring: RingReader<AudioResponse>,
}

impl std::fmt::Debug for Replies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replies").finish_non_exhaustive()
    }
}

/// A new response ring, its two ends and the count of responses it lost.
pub(crate) fn response_ring() -> (ResponseWriter, Replies, Counter<u64>) {
    let (writer, reader) = ring(RESPONSE_QUEUE_SIZE, Overflow::DropNewest);
    let lost = writer.losses();
    (
        ResponseWriter { ring: writer },
        Replies { ring: reader },
        lost,
    )
}

impl Replies {
    /// The oldest queued response.
    pub(super) fn pop(&mut self) -> Option<AudioResponse> {
        self.ring.pop()
    }
}

impl ResponseWriter {
    /// Queues a response for the audio thread; a full ring drops it and
    /// counts it lost. Then tells whoever waits on the bridge's progress.
    pub(super) fn push(&mut self, resp: AudioResponse, channels: &Channels) {
        // The ring counts the response it refuses (`Overflow::DropNewest`).
        let _ = self.ring.push(resp);
        channels.progress().notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;

    /// **A fates word is the code in its low two bits and the sequence above
    /// them**: a literal word, the round trip at the edges of the sequence, and
    /// every code.
    #[test]
    fn a_fates_word_holds_the_code_low_and_the_sequence_above() {
        assert_eq!(pack(5, 3), 0b10111);
        assert_eq!(unpack(0b10111), (5, 3));
        let max = u64::MAX >> CODE_BITS;
        for seq in [0, 1, BATCH_OUTCOMES as u64, max] {
            for fate in [
                BatchOutcome::Published,
                BatchOutcome::Refused,
                BatchOutcome::Skipped,
            ] {
                let (held, code) = unpack(pack(seq, fate.code()));
                assert_eq!((held, BatchOutcome::from_code(code)), (seq, Some(fate)));
            }
        }
        assert_eq!(
            BatchOutcome::from_code(0),
            None,
            "an empty word holds no fate"
        );
        assert_eq!(unpack(0), (0, 0));
    }

    /// **A control command is stamped with the blocks pushed, not the blocks
    /// submitted**: a block counted by `note_submitted` and not yet queued is
    /// not one the idle audio lane could have dealt with, so a command must not
    /// wait on it as if it were, nor be let through ahead of it.
    #[test]
    fn a_control_command_is_stamped_with_the_newest_block_pushed() {
        let (c, _parker) = channels();
        c.note_submitted(5);
        assert!(c.push_control(ControlCommand::CloseEditor));
        assert_eq!(c.pop_control().map(|s| s.after), Some(0), "5 is not queued");
        c.note_pushed(5);
        assert!(c.push_control(ControlCommand::CloseEditor));
        assert_eq!(c.pop_control().map(|s| s.after), Some(5));
    }

    fn channels() -> (Channels, Parker) {
        let (channels, waits) =
            Channels::new(SampleRate::SR_48K, Arc::default()).expect("the channels");
        (channels, waits.audio)
    }

    /// A fate for a block never submitted is dropped, so a peer-supplied
    /// `seq` cannot claim a word of the ring, wrapped or not, and the fates
    /// behind it stay readable.
    #[test]
    fn a_fate_for_a_block_never_submitted_is_dropped() {
        let (c, _) = channels();
        c.note_submitted(3);
        c.note_outcome(3, BatchOutcome::Published);
        for hostile in [4, 3 + BATCH_OUTCOMES as u64, u64::MAX >> 1, u64::MAX] {
            c.note_outcome(hostile, BatchOutcome::Refused);
        }
        assert_eq!(
            c.fate(3),
            Some(BatchOutcome::Published),
            "block 3's fate survived"
        );
        assert_eq!(c.fate(4), None);
    }

    /// Abandoned chunks, owed replies collected late and replies are separate
    /// monotonic totals: a poller who sees the first grow while the last does
    /// not has a hang, and a late reply is also a reply.
    #[test]
    fn blocks_abandoned_late_and_replies_are_counted_apart() {
        let (c, _) = channels();
        assert_eq!(c.block_counts(), PipelineCounts::default());
        for _ in 0..3 {
            c.note_abandoned();
        }
        let counts = |abandoned, late, replies| PipelineCounts {
            abandoned: Chunks::new(abandoned),
            late: Chunks::new(late),
            replies: Chunks::new(replies),
        };
        assert_eq!(c.block_counts(), counts(3, 0, 0));
        c.note_replied();
        assert_eq!(c.block_counts(), counts(3, 0, 1), "an on-time reply");
        c.note_late();
        c.note_late();
        assert_eq!(c.block_counts(), counts(3, 2, 3), "owed replies collected");
    }

    /// An audio lane's thread as its loop runs one: sleep for a command, take it,
    /// report it, until `stop`. Returns the handle once it is parked.
    fn bridge(
        c: &Channels,
        mut parker: Parker,
        stop: &Arc<std::sync::atomic::AtomicBool>,
    ) -> (
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<AudioCommand>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (c, stop) = (c.clone(), Arc::clone(stop));
        let t = std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match c.pop_block() {
                    Some(cmd) => tx.send(cmd).unwrap(),
                    None => c.wait_for_block(&mut parker, || stop.load(Ordering::Acquire)),
                }
            }
        });
        (t, rx)
    }

    const PATIENCE: Duration = Duration::from_secs(20);

    /// **A push wakes a parked bridge, and the audio thread's half
    /// allocates nothing.** The ring runs inside `assert_no_alloc` with the
    /// bridge parked, so the futex-wake path is the one gated, not just the
    /// fence and load.
    #[test]
    fn a_push_wakes_a_parked_bridge_without_allocating() {
        let (c, parker) = channels();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, rx) = bridge(&c, parker, &stop);
        while !c.unparker.is_parked() {
            std::thread::yield_now();
        }
        assert_no_alloc::assert_no_alloc(|| assert!(c.push_block(AudioCommand::Reset)));
        let got = rx.recv_timeout(PATIENCE).expect("the push woke the bridge");
        assert!(matches!(got, AudioCommand::Reset));
        stop.store(true, Ordering::Release);
        c.wake();
        t.join().unwrap();
    }

    /// **No push is left sitting in the queue**: many hand-offs, each
    /// answered before the next, race the bridge's park against the ring.
    /// A lost wake-up leaves one unanswered and the receive times out (there
    /// is no backstop timeout to hide it).
    #[test]
    fn no_pushed_command_waits_on_a_missed_wake_up() {
        let (c, parker) = channels();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, rx) = bridge(&c, parker, &stop);
        for i in 0..20_000u32 {
            // Alternate a spin so the push lands both before and after the park.
            for _ in 0..(i % 64) {
                std::hint::spin_loop();
            }
            assert!(c.push_block(AudioCommand::Reset));
            rx.recv_timeout(PATIENCE)
                .unwrap_or_else(|_| panic!("hand-off {i} was never woken"));
        }
        stop.store(true, Ordering::Release);
        c.wake();
        t.join().unwrap();
    }

    /// **`wake` ends a wait on a stop flag with no command queued**, which
    /// is how `shutdown` reaches a parked bridge when its queue was full.
    #[test]
    fn wake_ends_a_wait_with_nothing_queued() {
        let (c, parker) = channels();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, _rx) = bridge(&c, parker, &stop);
        while !c.unparker.is_parked() {
            std::thread::yield_now();
        }
        stop.store(true, Ordering::Release);
        c.wake();
        t.join().unwrap();
    }

    /// **Every command a full queue refuses is counted, and an accepted one is
    /// not**, whatever the caller does with the answer. The refusal path
    /// allocates nothing.
    #[test]
    fn a_full_command_queue_counts_each_refusal() {
        let (c, _parker) = channels();
        for _ in 0..COMMAND_QUEUE_SIZE {
            assert!(c.push_block(AudioCommand::Reset));
        }
        assert_eq!(
            c.commands_refused_cell().get(),
            0,
            "accepted is not refused"
        );
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..3 {
                assert!(!c.push_block(AudioCommand::Reset));
            }
        });
        assert_eq!(c.commands_refused_cell().get(), 3);
    }

    /// **A response the full response ring refuses is counted lost.**
    #[test]
    fn a_full_response_ring_counts_each_loss() {
        let (c, _parker) = channels();
        let (writer, _replies, lost) = response_ring();
        let mut writer = writer;
        let resp = || AudioResponse::Error { seq: None };
        for _ in 0..RESPONSE_QUEUE_SIZE {
            writer.push(resp(), &c);
        }
        assert_eq!(lost.get(), 0);
        writer.push(resp(), &c);
        writer.push(resp(), &c);
        assert_eq!(lost.get(), 2);
    }

    /// **Closing the settings' slots disconnects the waiters' replies**: a
    /// setting waiting in a slot, and one put after the close, both end their
    /// waiter at once (the 60 s bound only guards a hang).
    #[test]
    fn closing_the_settings_drops_every_waiting_reply() {
        let (c, _) = channels();
        let guard = Duration::from_secs(60);
        let (waiting, reply) = super::super::ask::ask::<()>();
        c.put_setting(Setting::Fence, reply);
        c.close_settings();
        assert!(!c.setting_waiting());
        assert_eq!(
            waiting.recv_timeout_borrowed(guard),
            Err(RecvTimeoutError::Disconnected)
        );

        let (late, reply) = super::super::ask::ask::<()>();
        c.put_setting(
            Setting::RenderMode(tutti_plugin_types::RenderMode::Offline),
            reply,
        );
        assert_eq!(
            late.recv_timeout_borrowed(guard),
            Err(RecvTimeoutError::Disconnected)
        );
        assert!(c.take_setting().is_none());
    }
}
