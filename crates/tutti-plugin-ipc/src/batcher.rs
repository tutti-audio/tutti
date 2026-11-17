//! [`Batcher`] — the pipeline between the plugin node and the plugin-server
//! bridge: every chunk it ships is exactly [`chunk`](Batcher::chunk) frames,
//! and the output is exactly one chunk late.
//!
//! [`process`](Batcher::process) takes a call's input into a FIFO and writes
//! the call's output from a ring. When the FIFO fills, the chunk is staged
//! into the shared-memory slab and submitted; when the ring's first frame is
//! next needed, the chunk submitted last is collected into it.
//!
//! # Pipelined, never waiting
//!
//! The batcher **submits chunk N and plays chunk N−1's output**, never waiting
//! for a reply. A synchronous wait on the audio thread fails even when each
//! node's wait is individually reasonable — half its own block period —
//! because the budgets *sum*: the graph runs nodes serially in one callback,
//! so three stalled plugins spend 3 × 667 µs against a 1333 µs deadline.
//! Parallelising the graph would not help; plugins in series are a dependency
//! chain.
//!
//! Not waiting makes a stalled plugin cost zero, however many there are and
//! whatever the graph's shape. The price is one chunk of latency per
//! out-of-process plugin — *declared to PDC* (the node's `Shape::latency`,
//! through `PluginControls::declared_latency` in `tutti-plugin-host`) and so compensated rather than
//! heard. This is what JACK, PipeWire, AUv3 and every DAW that hosts plugins
//! out of process do.
//!
//! # A FIFO, so every chunk is whole
//!
//! The calls the node is handed need not line up with chunks: a host may
//! call in any lengths, and an export may render 100-frame blocks. Shipping each call as it came (a 36-frame submission,
//! then a 64-frame one collecting it) would drop or zero-pad frames wherever
//! consecutive lengths differ. So a
//! call's input only ever fills the FIFO, a submission is always one whole
//! chunk, and the output is read from the ring at the same position the input
//! is written: output frame `t` is the plugin's output for input frame
//! `t - chunk`, for any cut of the calls.
//!
//! # One device callback per chunk
//!
//! The chunk is settled by [`prepare`](Batcher::prepare): the host's device
//! quantum when it knows one, else the graph's `MaxBlock`, never past the slab's [`MAX_CHUNK`](tutti_plugin_types::MAX_CHUNK). Live, that makes a chunk
//! complete on a callback's last frame and its output first needed on the
//! next callback's first — so the server has a whole device period to answer,
//! and the plugin's latency is one device block, as a DAW hosting plugins out
//! of process has it.
//!
//! A fixed 64-frame chunk would give lower latency but does not work: inside
//! a 480-frame callback every chunk but the first is collected microseconds
//! after it was submitted, and 186 of 200 blocks rendered silent (441: 198;
//! 1024: 187); with the callback as the chunk, 0 of 200
//! (`tests/clap_nodes/clap_live.rs`). A chunk that is not the callback — a host
//! that does not know its quantum, a device calling back with more than
//! `MAX_CHUNK` frames, or one whose callbacks vary — still delays exactly
//! `chunk` frames, but may collect chunks the server had no time to answer.
//! An offline fork waits for every chunk, so it is exact whatever its chunk.
//!
//! Whether N−1's output is really there is decided by the slab's per-slot
//! sequence numbers, not by a reply arriving: a mismatch yields silence. See
//! `tutti-plugin-ipc`'s `transport::shm::header`.
//!
//! # `f32` rows, the plugin's format on the wire
//!
//! The FIFO and the ring hold `f32`, whatever width the block is. The
//! wire carries whatever the plugin negotiated at load, so a plugin that
//! processes in double is converted here, in the wire scratch, and nowhere
//! else: the `f64` stays inside the node.

use crate::OfflineWait;
use crate::{AudioSlab, FATES, Fate, HarmonyInputs, PluginBridge, SlabWriter};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tutti_buffers::{Float, InputOutput, Mono, MonoMut};
use tutti_plugin_types::{
    BlockStatus, ChunkCounters, MidiEventVec, NoteExpressionChanges, ParameterChanges,
    SampleFormat, SlabAccessError, TransportChanges, TransportInfo, WireMidi, convert_channel,
};
use tutti_types::{SampleRate, Samples};

/// Single-chunk wire scratch in the plugin-negotiated format. Allocated at
/// [`Batcher::new`] for the chunk ceiling, because the wire format is known
/// up-front and no chunk is ever longer.
enum WireStorage {
    F32 { input: Vec<f32>, output: Vec<f32> },
    F64 { input: Vec<f64>, output: Vec<f64> },
}

impl WireStorage {
    fn new(format: SampleFormat, ceiling: usize) -> Self {
        match format {
            SampleFormat::Float32 => Self::F32 {
                input: vec![0.0; ceiling],
                output: vec![0.0; ceiling],
            },
            SampleFormat::Float64 => Self::F64 {
                input: vec![0.0; ceiling],
                output: vec![0.0; ceiling],
            },
        }
    }

    /// Stage `samples` as channel `ch` of chunk `seq`, converting to the
    /// wire's format. Staging only — the caller publishes once, after the
    /// last channel.
    fn stage(
        &mut self,
        slab: &mut SlabWriter,
        seq: u64,
        ch: usize,
        samples: &[f32],
    ) -> Result<(), SlabAccessError> {
        let n = samples.len();
        match self {
            Self::F32 { input, .. } => {
                let wire = &mut input[..n];
                convert_channel(Mono::new(samples), MonoMut::new(&mut *wire))
                    .expect("the wire row is the chunk's length");
                slab.write(seq, ch, wire)
            }
            Self::F64 { input, .. } => {
                let wire = &mut input[..n];
                convert_channel(Mono::new(samples), MonoMut::new(&mut *wire))
                    .expect("the wire row is the chunk's length");
                slab.write(seq, ch, wire)
            }
        }
    }

    /// Copy channel `ch` of chunk `seq`'s output into `out`, converting from
    /// the wire's format, and zero-pad past what the slab holds.
    ///
    /// Only call this for a `seq` the slab has confirmed (see
    /// [`Batcher::collectable`]): the copy count proves nothing about who
    /// wrote the bytes.
    ///
    /// `false` when the slab has no such channel (the batcher's widths and the
    /// slab's disagree): the channel is silence.
    fn recv(&mut self, slab: &AudioSlab, seq: u64, ch: usize, out: &mut [f32]) -> bool {
        let size = out.len();
        let (n, held) = match self {
            Self::F32 { output, .. } => {
                let wire = &mut output[..size];
                let read = slab.read_output_into(seq, ch, wire);
                let n = read.as_ref().map_or(0, |&n| n);
                convert_channel(Mono::new(&wire[..n]), MonoMut::new(&mut out[..n]))
                    .expect("the read and the row are `n` frames");
                (n, read.is_ok())
            }
            Self::F64 { output, .. } => {
                let wire = &mut output[..size];
                let read = slab.read_output_into(seq, ch, wire);
                let n = read.as_ref().map_or(0, |&n| n);
                // The narrowing is the point: the pipeline's rows are `f32`.
                convert_channel(Mono::new(&wire[..n]), MonoMut::new(&mut out[..n]))
                    .expect("the read and the row are `n` frames");
                (n, read.is_ok())
            }
        };
        out[n..].fill(0.0);
        held
    }
}

/// Everything the host produces for one chunk, aggregated for the bridge
/// call. Host-side only — the batcher unpacks it into the (unchanged)
/// positional `bridge.submit` arguments, so the IPC wire shape is untouched.
///
/// The chunk's parameter changes are not here: they stay in the node's
/// `ParamDelivery`, whose buffer the bridge swaps rather than moves
/// ([`Chunks::params`]), so nothing is allocated or freed per chunk.
#[derive(Debug)]
pub struct BlockPayload {
    /// The chunk's MIDI events.
    pub midi: MidiEventVec,
    /// The chunk's note expression changes.
    pub note_expression: NoteExpressionChanges,
    /// The chunk's chords, scales and text and integer note expressions.
    pub harmony: HarmonyInputs,
    /// The transport at the chunk's first frame.
    pub transport: TransportInfo,
    /// The transport changes after it, each at its chunk frame (doc 020
    /// stage 5b; `transport_walk`). Inline: nothing allocates.
    pub changes: TransportChanges,
}

impl BlockPayload {
    /// A chunk with nothing in it: no MIDI, no harmony, a stopped transport at
    /// `sample_rate` and no changes after it.
    #[must_use]
    pub fn new(sample_rate: SampleRate) -> Self {
        Self {
            midi: MidiEventVec::new(),
            note_expression: NoteExpressionChanges::default(),
            harmony: HarmonyInputs::default(),
            transport: TransportInfo::new(sample_rate),
            changes: TransportChanges::new(),
        }
    }
}

/// What the plugin node gives the batcher per chunk: the chunk's payload, and
/// where its MIDI-out goes.
///
/// A trait rather than arguments because a chunk and a call are not the same
/// span (module docs, "A FIFO, so every chunk is whole"): the node is told
/// where each chunk **begins** in the call that begins it, and is asked for
/// its payload when the chunk is **submitted**, possibly a call later.
pub trait Chunks {
    /// A chunk of `chunk` frames begins at frame `at` of this call (its first
    /// frame is the next input frame the batcher takes).
    fn begin(&mut self, at: usize, chunk: usize);
    /// This call's frames `from..from + n` go into the current chunk at its
    /// frame `at`.
    fn take(&mut self, from: usize, n: usize, at: usize);
    /// The payload of the chunk being submitted now, `frames` long: what
    /// [`begin`](Self::begin) gathered for it.
    fn payload(&mut self, frames: usize) -> BlockPayload;
    /// The chunk's parameter changes, for the bridge to swap out (see
    /// `AudioBridge::submit`); `None` for a host that sends none.
    fn params(&mut self) -> Option<&mut ParameterChanges> {
        None
    }
    /// Whether the chunk's parameter changes went with it, once it is
    /// submitted.
    fn handed_off(&mut self, _delivered: bool) {}
    /// The chunk being submitted could not be staged: what
    /// [`begin`](Self::begin) gathered for it never reaches the plugin.
    fn unsent(&mut self) {}
    /// The bridge refused the chunk just handed over in its
    /// [`payload`](Self::payload), with its `midi` MIDI events: they never
    /// reach the plugin.
    fn refused(&mut self, _midi: usize) {}
    /// The plugin's MIDI-out for the chunk the ring now holds, at that
    /// chunk's frames, sorted: handed over once, as its output starts to
    /// play.
    fn midi_out(&mut self, events: &MidiEventVec);
    /// What the plugin said of itself at the end of the chunk the ring now
    /// holds (CLAP's process status), handed over with its
    /// [`midi_out`](Self::midi_out). [`BlockStatus::Continue`] when the reply
    /// had not arrived by the time the chunk was collected: a status that is
    /// late is not applied, and the caller errs long.
    fn status(&mut self, _status: BlockStatus) {}
    /// One of those events, at frame `frame` of this call: where the ring
    /// plays the chunk frame it was emitted at, so it keeps its place against
    /// the plugin's audio. Called in frame order.
    fn emit(&mut self, frame: usize, event: WireMidi);
}

/// The pipeline between the plugin node and the plugin-server bridge: an
/// input FIFO that ships a chunk when full, and an output ring the previous
/// chunk's output is read from.
pub(crate) struct Batcher {
    wire: WireStorage,
    /// The slab's input writer: the one in this process, so the node is the
    /// only thing that writes a chunk (`SlabWriter`). Reads go through the
    /// bridge's shared slab, which cannot write.
    slab: SlabWriter,

    /// The slab's per-chunk size: the most [`chunk`](Self::chunk) can be.
    ceiling: usize,
    /// The chunk [`prepare`](Self::prepare) settled on: the frames every
    /// submission carries, and so the pipeline's latency.
    chunk: usize,

    /// Input frames not shipped yet, one row per input port (main plus
    /// sidechain/aux buses, bus-ordered: row `ch` is the slab's input-region
    /// channel `ch`, so a graph edge into the node's port 2 feeds the plugin's
    /// sidechain bus), `ceiling` long.
    fifo: Vec<Vec<f32>>,
    /// The output of the chunk collected last, one row per output port.
    ring: Vec<Vec<f32>>,
    /// Frames of the current chunk taken so far (`0..chunk`): where the next
    /// input frame goes, and which ring frame the next output reads.
    pos: usize,

    /// The sequence number the next submitted chunk will carry. Starts at 1
    /// because 0 means "nothing published" in a freshly zeroed slab.
    next_seq: u64,
    /// The chunk submitted and not collected yet, or `None` when nothing is
    /// in flight (start-up, after a reset, after a failed submission).
    expect_seq: Option<u64>,
    /// `Some` only on an **offline fork** (`Ipc::watch`): before
    /// collecting a chunk, wait up to this long for the server to publish it.
    /// See [`await_output`](Self::await_output).
    offline_wait: Option<Arc<OfflineWait>>,
    /// The plugin's MIDI-out for the chunk the ring holds, at that chunk's
    /// frames, sorted (`collect`). Inline, so the drain never allocates.
    reply: MidiEventVec,
    /// What the plugin said at the end of the chunk the ring holds
    /// (`collect`): `Continue` until its reply is read.
    said: BlockStatus,
    /// What the pipeline reports to its controls.
    counters: Arc<ChunkCounters>,
    /// Chunks collected unanswered, and not yet seen answered
    /// ([`Self::watch_late`]); `0` for an empty entry (sequences start at 1).
    /// At most `FATES` matter: the bridge remembers no more.
    unanswered: [u64; FATES],
}

impl std::fmt::Debug for Batcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Batcher")
            .field("chunk", &self.chunk)
            .field("ceiling", &self.ceiling)
            .field("next_seq", &self.next_seq)
            .field("expect_seq", &self.expect_seq)
            .finish_non_exhaustive()
    }
}

impl Batcher {
    /// A batcher for a plugin with these widths and wire format, writing
    /// into `slab`, whose chunks hold `ceiling` frames. Control thread:
    /// allocates the wire scratch, the FIFO and the ring. The chunk starts at
    /// the ceiling; [`prepare`](Self::prepare) narrows it to the graph's
    /// `MaxBlock`.
    pub fn new(
        inputs: usize,
        outputs: usize,
        format: SampleFormat,
        ceiling: usize,
        slab: SlabWriter,
    ) -> Self {
        Self {
            wire: WireStorage::new(format, ceiling),
            slab,
            ceiling,
            chunk: ceiling,
            fifo: vec![vec![0.0; ceiling]; inputs],
            ring: vec![vec![0.0; ceiling]; outputs],
            pos: 0,
            next_seq: 1,
            expect_seq: None,
            offline_wait: None,
            reply: MidiEventVec::new(),
            said: BlockStatus::Continue,
            counters: Arc::default(),
            unanswered: [0; FATES],
        }
    }

    /// Reports to `counters` (the controls' cells) instead of its own.
    pub(crate) fn with_counters(mut self, counters: Arc<ChunkCounters>) -> Self {
        self.counters = counters;
        self
    }

    /// Settle the chunk for `p`: the host's device quantum when it knows one
    /// (live: one chunk per device callback, so the server has a whole
    /// device period to answer), else the graph's `MaxBlock` (an offline
    /// render, or a host that does not know its callback), never past the
    /// slab's ceiling (module docs, "One device callback per chunk"). Starts
    /// the pipeline over, as a re-prepare starts the node over.
    pub fn prepare(&mut self, max_block: Samples, quantum: Option<Samples>) {
        let want = quantum.map_or(max_block.get(), Samples::get);
        self.chunk = self.ceiling.min(want).max(1);
        self.reset();
    }

    /// The slab's input writer and the sequence the next chunk would carry,
    /// given back: a served plugin's deactivation rejoins them with the rest
    /// of the plugin (`Ipc::deactivate`), and the next activation resumes at
    /// [`starting_at`](Self::starting_at) so no sequence of this one is reused.
    pub(crate) fn into_parts(self) -> (SlabWriter, u64) {
        (self.slab, self.next_seq)
    }

    /// Issues sequences from `next_seq` on. A slab outlives a batcher, and a
    /// publish of the old batcher's can still be in the audio socket: the
    /// sequence stays monotonic across the slab's batchers, as it does across
    /// a [`reset`](Self::reset). `next_seq` is at least 1.
    pub(crate) fn starting_at(mut self, next_seq: u64) -> Self {
        self.next_seq = next_seq.max(1);
        self
    }

    /// The frames every submission carries.
    pub fn chunk(&self) -> usize {
        self.chunk
    }

    /// Make every collect wait for its chunk, up to `budget` per chunk: the
    /// offline mode of a forked instance.
    ///
    /// The pipeline never waits because it runs on the audio thread, where a
    /// wait is a dropout (module docs). An offline fork runs on a render
    /// worker with no deadline, as fast as the plugin answers — so there the
    /// same rule is the defect: a render loop outpaces the subprocess, every
    /// chunk it has not published yet reads as silence, and the export is
    /// mostly silent with no error. Waiting keeps the pipelined shape and so
    /// the declared one-chunk latency, and makes the output a function of the
    /// input rather than of scheduling.
    ///
    /// The budget and the latches are the [`OfflineWait`]: the batcher records
    /// a miss or a dead server there, and the fork's health probe reports
    /// them.
    pub fn set_offline_wait(&mut self, wait: Arc<OfflineWait>) {
        self.offline_wait = Some(wait);
    }

    /// With [`set_offline_wait`](Self::set_offline_wait): block until the
    /// chunk this call collects is published, the bridge crashes, or the
    /// budget runs out. A no-op otherwise, and when nothing is in flight.
    ///
    /// **The first miss is the last wait.** A chunk still missing at the
    /// budget is collected as silence, as the live path would, and the wait
    /// latches `gave_up` and the bridge thread's wait for a reply returns to
    /// the live bound: from then on every chunk is collected at once
    /// (silence, unless the server catches up), so a hung server costs one
    /// budget per render, not one per chunk — an hour-long export through a
    /// wedged plugin would otherwise take days to report its failure. The
    /// fork's health probe turns the latch into `ForkFaultKind::TimedOut`,
    /// which the renderer reports.
    ///
    /// **A dead server ends the wait at once**, and reads as `Crashed`: the
    /// bridge's crash latch is the server's [`ServerWatch`](crate::ServerWatch),
    /// which the process's end sets as well as the stream's, so a wait that
    /// sends no command (and so never reads the socket) still sees it.
    fn await_output(&mut self, bridge: &PluginBridge) {
        let (Some(watch), Some(seq)) = (&self.offline_wait, self.expect_seq) else {
            return;
        };
        if watch.stopped_waiting() {
            return;
        }
        // A plugin that refused its rate publishes nothing, by design: the
        // chunk is silence at once, and the render's health says why.
        // The flag first: the reason is a lock and a clone, taken only when
        // there is a refusal to record.
        if bridge.rate_refused() {
            if let Some(refusal) = bridge.rate_refusal() {
                watch.latch_refusal(refusal);
            }
            return;
        }
        let deadline = Instant::now() + watch.budget();
        while !bridge.is_crashed() && !self.slab.has_output(seq) {
            if Instant::now() >= deadline {
                watch.give_up();
                // No reply is awaited past the live bound from here on: a
                // hung server must not hold the bridge thread one budget per
                // chunk the render still submits.
                bridge.set_reply_wait(watch.reply_wait());
                tracing::warn!(
                    seq,
                    budget = ?watch.budget(),
                    "offline plugin fork: block not published in time; \
                     rendering silence from here on"
                );
                return;
            }
            // Short, not a spin: this is a worker thread, and the server needs
            // the core more than this loop does.
            std::thread::sleep(Duration::from_micros(50));
        }
        // The latch outlives the bridge: the health probe reads the watch, and
        // a render that ends before the next poll drops the bridge (with the
        // executor) before anyone asks.
        if bridge.is_crashed() {
            return;
        }
        // The server publishes a chunk's audio, then sends its reply: wait
        // for the reply too, or an export's MIDI-out would depend on how the
        // two raced. Within the same budget; a reply that never comes costs
        // its MIDI, never the audio (no `give_up`).
        while !bridge.take_replies(seq, &mut self.reply, &mut self.said) {
            if bridge.is_crashed() || Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_micros(50));
        }
    }

    /// Drop the chunk in flight, the frames taken towards the next one and
    /// the output not yet played, and start collecting fresh.
    ///
    /// **`next_seq` is deliberately not reset.** This is the load-bearing
    /// subtlety of the whole pipeline: if the count restarted at 1, a chunk
    /// submitted before a seek could be published after it and accepted as the
    /// new chunk 1 — the seek would replay a fragment of pre-seek audio. Keeping
    /// the sequence monotonic makes a late publish structurally unmatchable.
    /// (A `u64` at ~750 chunks/s takes on the order of 780,000 years to wrap.)
    ///
    /// The server's own `Reset` is a no-op for this purpose, so clearing
    /// `expect_seq` here is the *entire* mechanism by which a seek stops old
    /// audio from arriving.
    pub fn reset(&mut self) {
        self.expect_seq = None;
        self.unanswered = [0; FATES];
        self.counters.in_flight.store(0, Ordering::Release);
        self.pos = 0;
        self.reply.clear();
        for row in self.fifo.iter_mut().chain(self.ring.iter_mut()) {
            row.fill(0.0);
        }
    }

    /// Whether chunk `expect_seq`'s output is really available to read.
    ///
    /// Two independent conditions, and the crash check is first on purpose. A
    /// crashed bridge never publishes, so the sequence would mismatch anyway and
    /// the audio would come out silent either way — but relying on that makes
    /// correct behaviour a *coincidence* of the sequence numbering rather than a
    /// decision. One relaxed atomic load, already on this path.
    fn collectable(&self, bridge: &PluginBridge) -> Option<u64> {
        if bridge.is_crashed() {
            return None;
        }
        let seq = self.expect_seq?;
        self.slab.has_output(seq).then_some(seq)
    }

    /// Fill the ring with the output of the chunk in flight: the one submitted
    /// last, whose input the ring's frames were played against a chunk ago.
    /// Silence when nothing is collectable (start-up, a reset, a crashed
    /// bridge, a chunk the server never answered).
    ///
    /// Called when the ring's first frame is needed and not before, so a chunk
    /// submitted at the end of one call has until the next call's output to
    /// be answered — the time the pipeline exists to give the plugin.
    ///
    /// The chunk's MIDI-out comes with it: its reply (and any earlier one that
    /// missed its own collection, at frame 0) is drained into `reply`, handed
    /// to `host` once and then emitted as the ring plays it (`process`).
    fn collect(&mut self, bridge: &PluginBridge, host: &mut impl Chunks) {
        self.reply.clear();
        self.said = BlockStatus::Continue;
        self.watch_late(bridge);
        self.await_output(bridge);
        let chunk = self.chunk;
        match self.collectable(bridge) {
            Some(seq) => self.read_ring(seq),
            None => {
                // A chunk in flight the server had not answered: a dropout.
                // Not while the plugin refuses its rate: its silence is
                // deliberate, and the status says so.
                if let Some(seq) = self.expect_seq
                    && !bridge.is_crashed()
                    && !bridge.rate_refused()
                {
                    self.counters.missed.fetch_add(1, Ordering::Relaxed);
                    self.remember_unanswered(seq);
                }
                for row in &mut self.ring {
                    row[..chunk].fill(0.0);
                }
            }
        }
        bridge.take_replies(
            self.expect_seq.unwrap_or(0),
            &mut self.reply,
            &mut self.said,
        );
        let last = u32::try_from(chunk.saturating_sub(1)).unwrap_or(u32::MAX);
        for e in self.reply.iter_mut() {
            e.offset = e.offset.min(last);
        }
        tutti_plugin_types::insertion_sort_by_key(&mut self.reply, |e| e.offset);
        host.midi_out(&self.reply);
        host.status(self.said);
        self.expect_seq = None;
        self.counters.in_flight.store(0, Ordering::Release);
    }

    /// Fill the ring with chunk `seq`'s output from the slab. A channel the slab
    /// lacks plays as silence and counts as a missed chunk: the batcher and its
    /// slab disagree.
    fn read_ring(&mut self, seq: u64) {
        let chunk = self.chunk;
        for (ch, row) in self.ring.iter_mut().enumerate() {
            if !self.wire.recv(self.slab.slab(), seq, ch, &mut row[..chunk]) {
                self.counters.missed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Notes `seq`, collected unanswered, to be resolved when the server's
    /// answer is seen: into an empty entry, else over the oldest, which is
    /// counted [`unanswered`](ChunkCounters::unanswered).
    fn remember_unanswered(&mut self, seq: u64) {
        let at = self
            .unanswered
            .iter()
            .position(|&s| s == 0)
            .unwrap_or_else(|| (0..FATES).min_by_key(|&i| self.unanswered[i]).unwrap_or(0));
        if self.unanswered[at] != 0 {
            self.counters.unanswered.fetch_add(1, Ordering::Relaxed);
        }
        self.unanswered[at] = seq;
    }

    /// Resolves the chunks collected unanswered whose fate is known: published
    /// ([`ChunkCounters::late`]), refused ([`ChunkCounters::refused`]) or
    /// skipped (counted by the bridge). A chunk whose fate the bridge never
    /// learned within the last [`FATES`] chunks is
    /// [`unanswered`](ChunkCounters::unanswered). Relaxed adds and loads of
    /// the slab's sequence words and the bridge's fate words: RT-safe.
    fn watch_late(&mut self, bridge: &PluginBridge) {
        for seq in &mut self.unanswered {
            if *seq == 0 {
                continue;
            }
            if self.slab.has_output(*seq) {
                self.counters.late.fetch_add(1, Ordering::Relaxed);
                *seq = 0;
                continue;
            }
            match bridge.chunk_fate(*seq) {
                Some(Fate::Published) => {
                    self.counters.late.fetch_add(1, Ordering::Relaxed);
                    *seq = 0;
                }
                Some(Fate::Refused) => {
                    self.counters.refused.fetch_add(1, Ordering::Relaxed);
                    *seq = 0;
                }
                Some(Fate::Skipped) => *seq = 0,
                None if *seq + (FATES as u64) / 2 < self.next_seq => {
                    self.counters.unanswered.fetch_add(1, Ordering::Relaxed);
                    *seq = 0;
                }
                None => {}
            }
        }
    }

    /// Ship the full FIFO as the next chunk: stage every input port, publish
    /// the input slot **exactly once, after the last channel** (a per-channel
    /// publish would let the server observe the slot as valid while later
    /// channels are still being copied, and half of this chunk spliced onto
    /// half of the previous one sounds almost right — far worse than
    /// silence), and hand the chunk and its payload to the bridge.
    fn submit(&mut self, bridge: &PluginBridge, host: &mut impl Chunks) {
        self.watch_late(bridge);
        let chunk = self.chunk;
        let seq = self.next_seq;
        let mut staged = true;
        for (ch, row) in self.fifo.iter().enumerate() {
            if self
                .wire
                .stage(&mut self.slab, seq, ch, &row[..chunk])
                .is_err()
            {
                staged = false;
                break;
            }
        }
        if !staged {
            self.counters.missed.fetch_add(1, Ordering::Relaxed);
            // The chunk will never be collectable (a channel the slab lacks
            // lands here too, and counts); its output plays as silence.
            // Its parameter changes did not go either: the host keeps them for
            // the next chunk. Its MIDI is lost (`Chunks::unsent`).
            self.expect_seq = None;
            host.handed_off(false);
            host.unsent();
            return;
        }
        self.slab.publish(seq);
        let p = host.payload(chunk);
        let midi = p.midi.len();
        let mut none = ParameterChanges::new();
        let submitted = bridge.submit(
            seq,
            chunk,
            p.midi,
            host.params().unwrap_or(&mut none),
            p.note_expression,
            p.harmony,
            p.transport,
            p.changes,
        );
        host.handed_off(submitted.params);
        if submitted.block {
            self.next_seq += 1;
            self.expect_seq = Some(seq);
            self.counters.in_flight.store(seq, Ordering::Release);
        } else {
            self.expect_seq = None;
            host.refused(midi);
        }
    }

    /// Take `io.frames()` frames of `io`'s input (one plane per input port) and
    /// write as many frames of its output (one plane per output port): output
    /// frame `t` is the plugin's output for input frame `t - chunk`, exactly,
    /// however the calls are cut.
    ///
    /// The block is converted to the pipeline's `f32` rows by the
    /// [`Sample`](tutti_buffers::Sample) rule, and back. An input port past the
    /// input's planes is taken as silence; an output port past the output's planes
    /// is not written.
    pub fn process<S: Float>(
        &mut self,
        bridge: &PluginBridge,
        io: &mut InputOutput<'_, '_, '_, S>,
        host: &mut impl Chunks,
    ) {
        let frames = io.frames().0;
        let (input, mut output) = io.split();
        let chunk = self.chunk;
        let mut i = 0;
        while i < frames {
            if self.pos == 0 {
                // A new chunk: its output-side frames need the chunk before it.
                self.collect(bridge, host);
                host.begin(i, chunk);
            }
            let n = (chunk - self.pos).min(frames - i);
            let (at, to) = (self.pos, self.pos + n);
            let call = Samples(i)..Samples(i + n);
            host.take(i, n, at);
            for (c, row) in self.ring.iter().enumerate() {
                if let Some(mut plane) = output.get_plane_mut(c) {
                    convert_channel(Mono::new(&row[at..to]), plane.slice_mut(call.clone()))
                        .expect("the ring rows and the call's frames are `n` long");
                }
            }
            let first = self.reply.partition_point(|e| (e.offset as usize) < at);
            for e in &self.reply[first..] {
                let o = e.offset as usize;
                if o >= to {
                    break;
                }
                host.emit(i + o - at, *e);
            }
            for (ch, row) in self.fifo.iter_mut().enumerate() {
                match input.get_plane(ch) {
                    Some(plane) => {
                        convert_channel(plane.slice(call.clone()), MonoMut::new(&mut row[at..to]))
                            .expect("the call's frames and the fifo rows are `n` long");
                    }
                    None => row[at..to].fill(0.0),
                }
            }
            self.pos = to;
            i += n;
            if self.pos == chunk {
                self.submit(bridge, host);
                self.pos = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioSlab;
    use tutti_plugin_types::MAX_CHUNK;

    /// A real slab's input writer, `samples` frames per channel, stereo both
    /// ways: these tests stage nothing, but a batcher cannot exist without the
    /// writer its chunks go through.
    fn slab(samples: usize) -> SlabWriter {
        let name = tutti_fixtures::unique_name("batcher");
        let stereo = tutti_plugin_types::ChannelLayout::STEREO;
        AudioSlab::create(
            name,
            crate::SlabLayout {
                samples_per_channel: samples,
                format: SampleFormat::Float32,
                slots: crate::RING_SLOTS as u32,
                inputs: vec![stereo],
                outputs: vec![stereo],
            },
        )
        .unwrap()
    }

    /// **A chunk's output is read from the slab it is given, and a channel the
    /// slab lacks answers `false` and plays as silence** instead of indexing
    /// past it. The slab here is not the bridge's: a batcher reads the one it
    /// writes.
    #[test]
    fn the_output_is_read_from_the_slab_given_and_a_missing_channel_is_refused() {
        let writer = slab(MAX_CHUNK);
        let server = AudioSlab::open(writer.name().to_owned(), writer.layout()).unwrap();
        let mut server = server;
        server.write(1, 0, &[0.5f32; 8]).unwrap();
        server.publish(1);
        let mut wire = WireStorage::new(SampleFormat::Float32, MAX_CHUNK);
        let mut out = [9.0f32; 8];
        assert!(wire.recv(writer.slab(), 1, 0, &mut out));
        assert_eq!(
            out, [0.5; 8],
            "the slab's own output, as the server wrote it"
        );
        let mut out = [9.0f32; 8];
        assert!(
            !wire.recv(writer.slab(), 1, 2, &mut out),
            "the slab has two channels"
        );
        assert_eq!(out, [0.0; 8], "a channel it lacks is silence");
    }

    /// **A batcher wider than its slab counts a missed chunk for each channel
    /// the slab lacks, and the others still play.**
    #[test]
    fn a_channel_the_slab_lacks_is_counted_as_missed() {
        let writer = slab(MAX_CHUNK);
        let mut server = AudioSlab::open(writer.name().to_owned(), writer.layout()).unwrap();
        for ch in 0..2 {
            server.write(1, ch, &[0.25f32; 8]).unwrap();
        }
        server.publish(1);
        // Three output rows over a two-channel slab.
        let mut b = Batcher::new(2, 3, SampleFormat::Float32, MAX_CHUNK, writer);
        b.chunk = 8;
        // The refusal is a `Copy` error: nothing is allocated on the audio
        // thread to report it.
        assert_no_alloc::assert_no_alloc(|| b.read_ring(1));
        assert_eq!(b.counters.missed.load(Ordering::Relaxed), 1);
        assert_eq!(b.ring[0][..8], [0.25; 8], "the channels it has play");
        assert_eq!(b.ring[2][..8], [0.0; 8], "the one it lacks is silence");
    }

    /// **A write to a channel the slab lacks is refused without allocating.**
    #[test]
    fn a_write_to_a_channel_the_slab_lacks_is_refused_without_allocating() {
        let mut writer = slab(MAX_CHUNK);
        let mut wire = WireStorage::new(SampleFormat::Float32, MAX_CHUNK);
        let samples = [0.5f32; 8];
        let refused = assert_no_alloc::assert_no_alloc(|| wire.stage(&mut writer, 1, 2, &samples));
        assert_eq!(refused, Err(SlabAccessError::Channel));
        assert_eq!(wire.stage(&mut writer, 1, 1, &samples), Ok(()));
    }

    /// A fresh batcher has nothing in flight, so its first chunk must be silence
    /// rather than whatever the output slot happens to contain.
    #[test]
    fn a_fresh_batcher_expects_nothing() {
        let b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK, slab(MAX_CHUNK));
        assert_eq!(b.expect_seq, None);
        assert_eq!(b.next_seq, 1, "0 means 'never published' in the slab");
    }

    /// **A full list of unanswered chunks counts the oldest unanswered**: the
    /// newest misses are the ones whose fate can still be learned, so one more
    /// than the list holds replaces the earliest, which is counted
    /// ([`ChunkCounters::unanswered`]); `reset` forgets them all.
    #[test]
    fn a_full_unanswered_list_counts_the_oldest() {
        let counters = Arc::new(ChunkCounters::default());
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK, slab(MAX_CHUNK))
            .with_counters(Arc::clone(&counters));
        for seq in 1..=FATES as u64 {
            b.remember_unanswered(seq);
        }
        assert_eq!(counters.unanswered(), 0, "the list was not full yet");
        b.remember_unanswered(FATES as u64 + 1);
        assert_eq!(counters.unanswered(), 1, "chunk 1, the oldest, is replaced");
        assert!(!b.unanswered.contains(&1));
        b.reset();
        assert_eq!(b.unanswered, [0; FATES]);
    }

    /// `reset` (a seek) drops the in-flight chunk but must NOT rewind the
    /// sequence. Rewinding would let a chunk submitted before the seek be
    /// published after it and accepted as the new chunk 1, replaying a
    /// fragment of pre-seek audio.
    #[test]
    fn reset_drops_the_in_flight_block_without_rewinding() {
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK, slab(MAX_CHUNK));
        b.next_seq = 100;
        b.expect_seq = Some(99);

        b.reset();
        assert_eq!(b.expect_seq, None, "the pre-seek block is abandoned");
        assert_eq!(
            b.next_seq, 100,
            "the sequence is monotonic across a seek — this is what makes a \
             late pre-seek publish structurally unmatchable"
        );
    }

    /// The *consequence* of that monotonicity: no sequence issued after a seek
    /// can equal one issued before it, so a late pre-seek publish cannot be
    /// mistaken for post-seek audio however long it takes to arrive.
    ///
    /// `has_output` compares sequences for equality, so this disjointness is what
    /// actually stops the replay. The test above pins the two fields; this pins
    /// what they buy.
    ///
    /// Asserted here rather than end-to-end, deliberately. Driving a real seek
    /// through the mock server means suspending it mid-block so the held reply
    /// lands after the reset — but the mock is single-threaded, so a suspended
    /// server also stops draining the 128-slot command queue. Every post-seek
    /// submit is then rejected and the pipeline goes silent for a reason that has
    /// nothing to do with `reset`. Measured, not assumed: all 8 post-seek submits
    /// failed. A real subprocess keeps reading its socket however slow its DSP
    /// is, so that failure is an artifact of the harness and the end-to-end test
    /// would have been pinning the artifact.
    #[test]
    fn no_post_seek_sequence_can_collide_with_a_pre_seek_one() {
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK, slab(MAX_CHUNK));

        let mut pre_seek = Vec::new();
        for _ in 0..5 {
            pre_seek.push(b.next_seq);
            b.next_seq += 1;
        }
        // One of them is still in flight when the seek happens.
        b.expect_seq = Some(*pre_seek.last().unwrap());

        b.reset();

        let mut post_seek = Vec::new();
        for _ in 0..5 {
            post_seek.push(b.next_seq);
            b.next_seq += 1;
        }

        for issued in &post_seek {
            assert!(
                !pre_seek.contains(issued),
                "sequence {issued} was issued both before and after the seek: a \
                 pre-seek block publishing late would satisfy `has_output` for a \
                 post-seek block and replay its audio"
            );
        }
        assert!(
            post_seek[0] > *pre_seek.last().unwrap(),
            "post-seek sequences must continue past the pre-seek ones, not \
             restart alongside them"
        );
    }

    /// The pipeline's chunk — and so the latency it declares — comes from
    /// `prepare`: the host's device quantum when it has one (one chunk per
    /// callback, so a live plugin keeps in step with the device rather than a
    /// 64-frame grid), else the graph's
    /// `MaxBlock`, capped by the slab's ceiling either way.
    #[test]
    fn the_pipeline_chunk_is_the_device_quantum_else_the_max_block() {
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK, slab(MAX_CHUNK));
        b.prepare(Samples(1024), Some(Samples(480)));
        assert_eq!(Samples(b.chunk()), Samples(480));
        b.prepare(Samples(64), Some(Samples(2048)));
        assert_eq!(
            Samples(b.chunk()),
            Samples(2048),
            "the quantum, not MaxBlock"
        );
        b.prepare(Samples(1024), None);
        assert_eq!(Samples(b.chunk()), Samples(1024), "no quantum: MaxBlock");
        b.prepare(Samples(32), None);
        assert_eq!(Samples(b.chunk()), Samples(32));

        let mut small_slab = Batcher::new(2, 2, SampleFormat::Float32, 16, slab(16));
        small_slab.prepare(Samples(1024), None);
        assert_eq!(Samples(small_slab.chunk()), Samples(16));
    }

    /// A re-prepare starts the pipeline over: the chunk in flight was
    /// submitted under the old chunk size and must not be collected into the
    /// new one — without advancing the sequence, as for a seek.
    #[test]
    fn prepare_drops_the_chunk_in_flight() {
        let mut b = Batcher::new(2, 2, SampleFormat::Float32, MAX_CHUNK, slab(MAX_CHUNK));
        b.next_seq = 7;
        b.expect_seq = Some(6);
        b.prepare(Samples(128), None);
        assert_eq!(b.expect_seq, None);
        assert_eq!(b.next_seq, 7);
    }
}
