//! Lock-free message bus between audio thread and bridge thread.
//!
//! Two queues survive at this level: the command queue (audio thread → bridge
//! thread) and the audio-response ring (bridge thread → audio thread, RT, one
//! producer and one consumer).
//! Reply-bearing commands carry their own per-request `Reply<T>` so each
//! response is routed back to its specific caller — no shared
//! control-response queue, no risk of mismatched responses.
//!
//! `ProcessPayload` recycling lives in [`PayloadPool`](super::payload_pool::PayloadPool),
//! a separate primitive whose `acquire`/`recycle` names document the RT
//! contract.

use super::messages::{AudioResponse, Command};
use crossbeam::queue::ArrayQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tutti_plugin_types::BlockCounts;
use tutti_rt::{
    AudioThreadCell, Counter, Doorbell, LockExt, Overflow, PackedAtomic, RingReader, RingWriter,
    Sleeper, Tally, TallyWriter, doorbell, ring, tally,
};
use tutti_types::AtomicSampleRate;
use tutti_units::SampleRate;

/// How many commands the audio thread can queue before `push_command` fails.
/// This also bounds how many `Process` payloads can be outstanding at once,
/// which is why [`super::payload_pool::PayloadPool`] sizes itself from it.
pub(super) const COMMAND_QUEUE_SIZE: usize = 128;
const RESPONSE_QUEUE_SIZE: usize = 128;

#[derive(Clone)]
pub(super) struct Channels {
    commands: Arc<ArrayQueue<Command>>,
    /// The audio-response ring's producing end: the bridge thread's alone
    /// (every `push_audio_response` runs there), so one writer.
    responses_in: Arc<AudioThreadCell<RingWriter<AudioResponse>>>,
    /// The consuming end: the audio side's alone (`take_replies`, called by the
    /// pipeline that collects a chunk), so one reader. A full ring drops the
    /// newest response and counts it in `responses_lost`.
    responses_out: Arc<AudioThreadCell<RingReader<AudioResponse>>>,
    /// The newest block sequence the audio thread has submitted. Owned here
    /// rather than on the batcher because the *bridge* thread is what reads it,
    /// to decide whether a dequeued block is still worth sending.
    newest_seq: Arc<AtomicU64>,
    /// The bridge thread's doorbell: the thread sleeps on it when the queue
    /// is empty, and the audio thread rings it after a push. The ring is
    /// wait-free (no lock, no allocation) and cannot be missed
    /// ([`Doorbell`]'s protocol), so the bridge needs no polling timeout.
    bell: Doorbell,
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
    /// How many blocks the bridge thread abandoned on a missed budget, ever:
    /// a loss, and with [`replies`](Self::replies) the hang verdict's input.
    abandoned: Tally<u64>,
    /// The bridge thread's writer of `abandoned`, minted in `new`.
    abandoned_writer: TallyWriter<u64>,
    /// How many owed replies the bridge thread collected after abandoning the
    /// blocks that asked for them, ever: the other half of `abandoned`.
    late: Tally<u64>,
    /// The bridge thread's writer of `late`, minted in `new`.
    late_writer: TallyWriter<u64>,
    /// Commands the audio side could not hand to the bridge thread, ever: a
    /// full command queue refused one, the payload pool had no payload for a
    /// block, or a parameter buffer had no room to come back to. Written
    /// through any clone of the channels, read by the plugin node's controls.
    commands_refused: Tally<u64>,
    /// The writer of `commands_refused`, minted in `new`.
    commands_refused_writer: TallyWriter<u64>,
    /// Parameter buffers the audio thread leaked rather than free, ever: the
    /// free list had no room for one it returned (a broken sizing bound).
    leaked_buffers: Tally<u64>,
    /// The writer of `leaked_buffers`, minted in `new`.
    leaked_writer: TallyWriter<u64>,
    /// Audio responses the bridge thread could not queue for the audio
    /// thread (the response ring was full), ever: the ring's own count.
    responses_lost: Counter<u64>,
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
    skipped: Tally<u64>,
    /// The bridge thread's writer of `skipped`, minted in `new`.
    skipped_writer: TallyWriter<u64>,
    /// What became of each of the last [`FATES`] chunks the bridge thread
    /// dealt with, for the batcher to look up.
    fates: Arc<Fates>,
    /// The control connection has something the bridge thread has not read:
    /// set by the watcher thread, cleared by the bridge thread after it has
    /// read the lane dry. The bridge's doorbell is rung with it, so a bridge
    /// parked with no command queued reads the lane when news arrives.
    news: Arc<std::sync::atomic::AtomicBool>,
    /// A watch on the control connection, kept while the watcher thread runs:
    /// [`bytes_waiting`](Self::bytes_waiting) asks it whether bytes the bridge
    /// has not read are there, which `news` alone misses between the bridge's
    /// clearing it and the watcher's setting it again.
    probe: Arc<std::sync::Mutex<Option<Arc<crate::transport::control::Watch>>>>,
    /// Notified on the bridge thread when a response is queued and when it has
    /// finished a turn (a lane read dry, a command answered): what a control or
    /// worker thread waiting on the bridge sleeps on. The server's watch
    /// notifies it too, so a waiter wakes at the server's end.
    progress: Arc<crate::Wake>,
}

/// How many chunks back [`Fates`] remembers.
pub(crate) const FATES: usize = 16;

/// What the bridge thread learned of a chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fate {
    /// The server published its audio: the reply said so.
    Published,
    /// The server did not publish it: its input slot had been recycled.
    Refused,
    /// The bridge never sent it: more than a chunk behind.
    Skipped,
}

/// The last [`FATES`] chunks' fates, one word each: the sequence above two
/// bits of fate, so a lookup of a sequence the entry no longer holds finds
/// nothing. Written by the bridge thread alone, read on the audio thread.
///
/// Each word stands alone and nothing is published through it (the audio is
/// gated by the slab's sequence words), so a reader sees a fate or not yet,
/// and tries again at its next look.
struct Fates([PackedAtomic<u64>; FATES]);

impl Fates {
    fn new() -> Self {
        Self(std::array::from_fn(|_| PackedAtomic::new(0)))
    }

    /// Stores `seq`'s fate, unless the word already holds a newer chunk's: a
    /// late reply to an old chunk (a server that stalled for more than
    /// [`FATES`] chunks) must not erase the newer fate. A plain load decides:
    /// only the bridge thread writes.
    fn record(&self, seq: u64, fate: Fate) {
        let code = match fate {
            Fate::Published => 1,
            Fate::Refused => 2,
            Fate::Skipped => 3,
        };
        let word = &self.0[(seq % FATES as u64) as usize];
        if word.load() >> 2 > seq {
            return;
        }
        word.store(seq << 2 | code);
    }

    fn get(&self, seq: u64) -> Option<Fate> {
        let word = self.0[(seq % FATES as u64) as usize].load();
        if word >> 2 != seq {
            return None;
        }
        match word & 3 {
            1 => Some(Fate::Published),
            2 => Some(Fate::Refused),
            3 => Some(Fate::Skipped),
            _ => None,
        }
    }
}

impl Channels {
    /// The channels and the [`Sleeper`] of the bridge thread's doorbell,
    /// which that thread alone waits on.
    pub(super) fn new(sample_rate: SampleRate, progress: Arc<crate::Wake>) -> (Self, Sleeper) {
        let (bell, sleeper) = doorbell();
        let (dropped_param_events, dropped_midi, skipped) = (tally(), tally(), tally());
        let (abandoned, late, commands_refused, leaked_buffers) =
            (tally(), tally(), tally(), tally());
        let (responses_in, responses_out) = ring(RESPONSE_QUEUE_SIZE, Overflow::DropNewest);
        let channels = Self {
            commands: Arc::new(ArrayQueue::new(COMMAND_QUEUE_SIZE)),
            responses_lost: responses_in.losses(),
            responses_in: Arc::new(AudioThreadCell::new(responses_in)),
            responses_out: Arc::new(AudioThreadCell::new(responses_out)),
            newest_seq: Arc::new(AtomicU64::new(0)),
            bell,
            sample_rate: Arc::new(AtomicSampleRate::new(sample_rate)),
            reply_wait_ns: Arc::new(AtomicU64::new(0)),
            abandoned_writer: abandoned.writer(),
            abandoned,
            late_writer: late.writer(),
            late,
            commands_refused_writer: commands_refused.writer(),
            commands_refused,
            leaked_writer: leaked_buffers.writer(),
            leaked_buffers,
            replies: Arc::new(AtomicU64::new(0)),
            param_events_writer: dropped_param_events.writer(),
            dropped_param_events,
            midi_writer: dropped_midi.writer(),
            dropped_midi,
            skipped_writer: skipped.writer(),
            skipped,
            fates: Arc::new(Fates::new()),
            news: Arc::default(),
            probe: Arc::default(),
            progress,
        };
        (channels, sleeper)
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

    /// Park the bridge thread until a command is queued or `stop` says to
    /// leave, whichever is already true or comes first. May return with
    /// neither (a spurious wake-up): the caller rechecks. `stop` reads what
    /// whoever ends the loop publishes before calling [`wake`](Self::wake).
    /// `sleeper` is the one [`new`](Self::new) returned.
    pub(super) fn wait_for_command(&self, sleeper: &mut Sleeper, stop: impl FnOnce() -> bool) {
        sleeper.wait(|| !self.commands.is_empty() || stop());
    }

    /// The control connection has bytes to read: publishes it and rings the
    /// bridge thread's doorbell. The watcher thread's call.
    pub(super) fn set_news(&self) {
        self.news.store(true, Ordering::Release);
        self.bell.ring();
    }

    /// Whether [`set_news`](Self::set_news) was called since the last
    /// [`clear_news`](Self::clear_news).
    pub(super) fn news_pending(&self) -> bool {
        self.news.load(Ordering::Acquire)
    }

    /// Keeps `watch` as the probe [`bytes_waiting`](Self::bytes_waiting) asks.
    pub(super) fn set_probe(&self, watch: Arc<crate::transport::control::Watch>) {
        *self.probe.lock_recover() = Some(watch);
    }

    /// Lets the connection's duplicate go once nothing watches it.
    pub(super) fn clear_probe(&self) {
        *self.probe.lock_recover() = None;
    }

    /// Whether the control connection has bytes the bridge thread has not read
    /// (one `poll`, no wait). `false` with no probe.
    pub(super) fn bytes_waiting(&self) -> bool {
        self.probe
            .lock_recover()
            .as_ref()
            .is_some_and(|watch| watch.wait_readable(Duration::ZERO))
    }

    /// The bridge thread has read the control connection dry. Cleared *after*
    /// the read: a byte that arrives during it is still readable, so the
    /// watcher sees it and sets the flag again.
    pub(super) fn clear_news(&self) {
        self.news.store(false, Ordering::Release);
    }

    /// Wake the bridge thread to re-check its stop condition (shutdown).
    /// Publish the condition first. Real-time safe, like the ring in
    /// [`push_command`](Self::push_command).
    pub(super) fn wake(&self) {
        self.bell.ring();
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
        self.late_writer.add(1);
        self.replies.fetch_add(1, Ordering::Release);
    }

    /// Record that a block's reply missed its budget and is now owed. Called
    /// by the bridge thread only, before any settle of that reply.
    pub(super) fn note_abandoned(&self) {
        self.abandoned_writer.add(1);
    }

    /// Record that the plugin answered (a reply for a block, on time or
    /// owed): it is not hung. Bridge thread only, after the reply is queued.
    pub(super) fn note_replied(&self) {
        self.replies.fetch_add(1, Ordering::Release);
    }

    /// The block counts over the life of the bridge. Separate reads: a reply
    /// landing between them shows up on the next call, which is all a poller
    /// needs. A counted reply is queued.
    pub(super) fn block_counts(&self) -> BlockCounts {
        BlockCounts {
            abandoned: self.abandoned.get(),
            late: self.late.get(),
            replies: self.replies.load(Ordering::Acquire),
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

    /// The count of audio responses lost to a full response ring.
    pub(super) fn responses_lost_cell(&self) -> Counter<u64> {
        self.responses_lost.clone()
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
        self.skipped_writer.add(1);
    }

    /// Record what became of chunk `seq`. Bridge thread only. A chunk not
    /// yet submitted has no fate: a peer-supplied `seq` far ahead would
    /// otherwise claim a word of the ring and erase the fates behind it.
    pub(super) fn note_fate(&self, seq: u64, fate: Fate) {
        if self.sent(seq) {
            self.fates.record(seq, fate);
        }
    }

    /// What became of chunk `seq`, while it is among the last [`FATES`].
    pub(super) fn fate(&self, seq: u64) -> Option<Fate> {
        self.fates.get(seq)
    }

    /// The cell [`note_skipped`](Self::note_skipped) counts into.
    pub(super) fn skipped_cell(&self) -> Tally<u64> {
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

    /// Pushes, then rings the bridge thread's doorbell, so the socket
    /// round-trip starts immediately rather than after a poll interval. The
    /// audio thread never waits on the reply, but the ring still matters: it
    /// is what keeps a block's reply arriving in time to be collected on the
    /// *next* block rather than the one after.
    ///
    /// The ring is wait-free and allocation-free (`Doorbell::ring`: a fence
    /// and a load, plus a futex wake when the bridge is parked), and it
    /// cannot be lost, so nothing backs it with a timeout. A push the queue
    /// refuses rings nothing: the bridge is, or is about to be, draining.
    ///
    /// A refused command is counted ([`note_command_refused`](Self::note_command_refused)).
    pub(super) fn push_command(&self, cmd: Command) -> bool {
        self.try_push_command(cmd).is_ok()
    }

    /// [`push_command`](Self::push_command), handing a refused command back
    /// rather than dropping it, so the audio thread can return what it holds
    /// to its pools instead of freeing it.
    pub(super) fn try_push_command(&self, cmd: Command) -> Result<(), Command> {
        if let Err(cmd) = self.commands.push(cmd) {
            self.note_command_refused();
            return Err(cmd);
        }
        self.bell.ring();
        Ok(())
    }

    pub(super) fn pop_command(&self) -> Option<Command> {
        self.commands.pop()
    }

    /// Queues a response for the audio thread; a full ring drops it and
    /// counts it lost. Bridge thread only.
    pub(super) fn push_audio_response(&self, resp: AudioResponse) {
        // The ring counts the response it refuses (`Overflow::DropNewest`).
        let _ = self.responses_in.borrow_mut().push(resp);
        self.progress.notify();
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

    /// The oldest queued response. The audio side only.
    pub(super) fn pop_audio_response(&self) -> Option<AudioResponse> {
        self.responses_out.borrow_mut().pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels() -> (Channels, Sleeper) {
        Channels::new(SampleRate::SR_48K, Arc::default())
    }

    /// A fate for a block never submitted is dropped, so a peer-supplied
    /// `seq` cannot claim a word of the ring, wrapped or not, and the fates
    /// behind it stay readable.
    #[test]
    fn a_fate_for_a_block_never_submitted_is_dropped() {
        let (c, _) = channels();
        c.note_submitted(3);
        c.note_fate(3, Fate::Published);
        for hostile in [4, 3 + FATES as u64, u64::MAX >> 1, u64::MAX] {
            c.note_fate(hostile, Fate::Refused);
        }
        assert_eq!(c.fate(3), Some(Fate::Published), "block 3's fate survived");
        assert_eq!(c.fate(4), None);
    }

    /// Abandoned blocks, owed replies collected late and replies are separate
    /// monotonic totals: a poller who sees the first grow while the last does
    /// not has a hang, and a late reply is also a reply.
    #[test]
    fn blocks_abandoned_late_and_replies_are_counted_apart() {
        let (c, _) = channels();
        assert_eq!(c.block_counts(), BlockCounts::default());
        for _ in 0..3 {
            c.note_abandoned();
        }
        let counts = |abandoned, late, replies| BlockCounts {
            abandoned,
            late,
            replies,
        };
        assert_eq!(c.block_counts(), counts(3, 0, 0));
        c.note_replied();
        assert_eq!(c.block_counts(), counts(3, 0, 1), "an on-time reply");
        c.note_late();
        c.note_late();
        assert_eq!(c.block_counts(), counts(3, 2, 3), "owed replies collected");
    }

    /// A bridge thread as `pump` runs one: sleep for a command, take it,
    /// report it, until `stop`. Returns the handle once it is parked.
    fn bridge(
        c: &Channels,
        mut sleeper: Sleeper,
        stop: &Arc<std::sync::atomic::AtomicBool>,
    ) -> (
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<Command>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (c, stop) = (c.clone(), Arc::clone(stop));
        let t = std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match c.pop_command() {
                    Some(cmd) => tx.send(cmd).unwrap(),
                    None => c.wait_for_command(&mut sleeper, || stop.load(Ordering::Acquire)),
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
        let (c, sleeper) = channels();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, rx) = bridge(&c, sleeper, &stop);
        while !c.bell.is_asleep() {
            std::thread::yield_now();
        }
        assert_no_alloc::assert_no_alloc(|| assert!(c.push_command(Command::Reset)));
        let got = rx.recv_timeout(PATIENCE).expect("the push woke the bridge");
        assert!(matches!(got, Command::Reset));
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
        let (c, sleeper) = channels();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, rx) = bridge(&c, sleeper, &stop);
        for i in 0..20_000u32 {
            // Alternate a spin so the push lands both before and after the park.
            for _ in 0..(i % 64) {
                std::hint::spin_loop();
            }
            assert!(c.push_command(Command::Reset));
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
        let (c, sleeper) = channels();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (t, _rx) = bridge(&c, sleeper, &stop);
        while !c.bell.is_asleep() {
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
        let (c, _sleeper) = channels();
        for _ in 0..COMMAND_QUEUE_SIZE {
            assert!(c.push_command(Command::Reset));
        }
        assert_eq!(
            c.commands_refused_cell().get(),
            0,
            "accepted is not refused"
        );
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..3 {
                assert!(!c.push_command(Command::Reset));
            }
        });
        assert_eq!(c.commands_refused_cell().get(), 3);
    }

    /// **A response the full response queue refuses is counted lost.**
    #[test]
    fn a_full_response_queue_counts_each_loss() {
        let (c, _sleeper) = channels();
        let resp = || AudioResponse::Error { seq: None };
        for _ in 0..RESPONSE_QUEUE_SIZE {
            c.push_audio_response(resp());
        }
        assert_eq!(c.responses_lost_cell().get(), 0);
        c.push_audio_response(resp());
        c.push_audio_response(resp());
        assert_eq!(c.responses_lost_cell().get(), 2);
    }
}
