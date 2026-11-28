//! [`VoicePool`] — one graph node that plays every voice on a track.
//!
//! The pool lends its table of rows once per block, plays and sums its slots itself,
//! and hands back through a ring what it must not free. One node per track rather
//! than one per voice plus a dynamic summing unit: a voice joining or leaving is a
//! row of the table, not a graph edit, which is what keeps voice churn off the
//! commit path.

use super::clock::{BlockClock, Clock};
use super::command::{LoopFeed, Message, VoicePoolControls};
use super::roster::{Exit, Installed, Roster};
use super::table::{VoicePoolConfig, VoiceTable, VoiceTableKind};
use super::types::Reach;
use crate::SamplerFloat;
use crate::lanes::{BlockScratch, UNPREPARED_FRAMES};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tutti_buffers::{ChannelLayout, MAX_CHANNELS};
use tutti_graph::{
    Cx, ForkCause, ForkSource, Forked, HasPublished, IntoNode, Io, Node, NodeParts, Prepare,
    RenderMode, Shape, Status,
};
use tutti_rt::{CappedVec, Overflow, Publisher, Reader, RingReader, ring};
use tutti_units::{SampleRate, Tail};

// ---------------------------------------------------------------------------
// Retirement — values the audio thread must not drop.
// ---------------------------------------------------------------------------

/// Something the audio thread took ownership of and must not free in the callback.
///
/// Both variants exist for the same reason: dropping one can free a large
/// allocation (a slot's vocoder rings are about 120 KB per channel, and its wave may
/// be the last reference to a decoded buffer), and a streamed slot returns its
/// reader to its config on drop, which locks. Rather than teach each call site its
/// own evasion, everything the audio thread needs to shed goes down one ring and is
/// freed by the controls on the control thread.
///
/// The ring always has room for what the audio thread pushes: the controls reserve a
/// place in it for every slot and spare they send ([`RetireRoom`]), and send nothing
/// when none is left, so a push never drops one.
///
/// No field is ever read: the value is the payload, and receiving it on the control
/// thread and dropping it there is what frees it.
#[allow(
    dead_code,
    reason = "the payloads exist to be received and dropped on the control thread, never read"
)]
pub(crate) enum Retired<F: SamplerFloat> {
    /// A slot no row of the table matches, vocoder and all.
    Slot(Box<Installed<F>>),
    /// A spare vocoder a `SetSpare` took out of a slot (the loop moved off the
    /// voice), or found no use for (the slot has one, or no vocoder, or is gone).
    Spare(Box<tutti_dsp::PhaseVocoder<F>>),
}

/// What a [`VoicePool`] and its controls dropped or deferred, since the pool was
/// built: a reading a host polls ([`VoicePoolControls::drops`]), never a log line
/// from the callback.
///
/// Each is a count and only ever grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PoolDrops {
    /// Retired values the audio thread could neither queue nor park, and leaked
    /// rather than free in the callback. Always zero unless the retirement
    /// bound is wrong: a reservation exists for every value, and values
    /// retired after the controls are gone are parked in the pool.
    pub leaked: u64,
    /// Spare vocoders (or their withdrawal) the controls could not queue because the
    /// command ring or the retirement room was full. Nothing was sent: the next tick
    /// retries, and a wrap met meanwhile flushes as a seek does.
    pub spares_deferred: u64,
}

/// The retirement slots the controls have reserved of the ring: one for each slot
/// and spare sent, held until the value comes back and is collected. Never above
/// the ring's capacity, which is what lets the audio thread's push always fit.
#[derive(Debug, Default)]
pub(crate) struct RetireRoom {
    reserved: AtomicUsize,
}

impl RetireRoom {
    /// Reserves one of `capacity` slots for a value about to be sent; `false`
    /// when none is left. Called with the command writer locked, so reservations
    /// are serial and only releases race with it.
    pub(crate) fn reserve(&self, capacity: usize) -> bool {
        if self.reserved.load(Ordering::Acquire) >= capacity {
            return false;
        }
        self.reserved.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Whether a slot of `capacity` is left to reserve.
    pub(crate) fn has_room(&self, capacity: usize) -> bool {
        self.reserved.load(Ordering::Acquire) < capacity
    }

    /// Gives `n` reserved slots back: by the audio thread for a spare it kept, by
    /// the controls for the values they collected.
    pub(crate) fn release(&self, n: usize) {
        self.reserved.fetch_sub(n, Ordering::AcqRel);
    }
}

/// The atomics behind [`PoolDrops`], shared by a pool and its controls.
#[derive(Debug, Default)]
pub(crate) struct DrainCounters {
    pub(crate) leaked: AtomicU64,
    pub(crate) spares_deferred: AtomicU64,
}

impl DrainCounters {
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> PoolDrops {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        PoolDrops {
            leaked: load(&self.leaked),
            spares_deferred: load(&self.spares_deferred),
        }
    }
}

/// The messages the controls' ring holds: two for each voice, and two over.
pub(crate) fn message_capacity(voices: usize) -> usize {
    2 * voices + 2
}

/// The retirement ring: a slot and a spare for each voice, and a slot being
/// replaced for each.
pub(crate) fn retirement_capacity(voices: usize) -> usize {
    3 * voices
}

// ---------------------------------------------------------------------------
// VoicePool — the graph node.
// ---------------------------------------------------------------------------

/// Every voice on one track, played and summed by a single graph node.
///
/// Zero inputs, [`channels`](Self::channels) outputs. It is made from a
/// [`VoicePoolConfig`]: its rows are the voices, a published [`VoiceTable`] a
/// session writes. Each block it lends the table, applies it where it changed,
/// reads the transport from the block's `Env`, then reads and sums its slots, all
/// of it on the audio thread and allocation-free provided the control side did its
/// share: the controls build every voice, and free what the pool hands back.
///
/// # As a graph node
///
/// A `tutti_graph::Node`. Its controls ([`IntoNode`]) are its
/// [`VoicePoolControls`], the node's one published entry: a session's write of the
/// table is their `replace`, which builds the voices the new rows need and sends them
/// through a private ring, and their sweep collects what the pool retired.
///
/// A fork of a pool is a pool at its width over the table as it is when the fork
/// is taken, with its own copies of the voices and no ring. A streamed row forks
/// only for an offline render.
///
/// # Rows and slots
///
/// A slot sounds for a row of its key and structure ([`Row::same_structure`](super::table::Row::same_structure)):
/// the table's values (gain, speed, loop, direction, window, steering, stretch
/// factor and pitch) are applied to it where they differ from the ones it holds. A row
/// with no slot renders nothing until its voice arrives, and a slot with no row is
/// silent from the block the table changed in: it is not rendered, and goes back
/// through the retirement ring to be freed on the control side. A voice arrives
/// through the ring, in the order the controls send it, and the ring may be full: then
/// the controls send the rest on a later tick, so the voices of one table do not all
/// arrive at one block.
///
/// # The retirement ring cannot be full
///
/// Its capacity is `3 * voices`. Every slot and every spare the controls send holds one
/// reserved place of it from the send until the value is collected, and the controls
/// send nothing while `3 * voices` are reserved (`RetireRoom`). A value the audio thread
/// pushes is a slot or spare it holds, so the values in the ring plus those in the pool
/// never exceed the reservations: at most `voices` slots alive, `voices` spares, and
/// `voices` slots retired and not yet collected. A push therefore never finds the ring
/// full. If one did, the unmatched slot stays in the vector, muted and not rendered,
/// and the next block's merge pushes it again; it is never dropped or freed on the audio
/// thread.
///
/// A removal cuts: the slot is not rendered from the block the table changed in, with no
/// ramp, as the `Remove` it replaces cut at the block that drained it.
///
/// Both source tiers live here side by side, as the `Memory` / `Disk` arms of
/// [`VoiceSource`](super::types::VoiceSource); the slot's read forks on that enum
/// rather than on a trait, which is what keeps a per-tier difference visible at
/// the call site.
///
/// # Stretched voices
///
/// The pool declares no latency. A stretched voice of either tier seats its vocoder on the
/// source's own history (the file's, for a disk voice, from its ring) and sounds on time.
///
/// A stretching voice's seat (a seek, or play from a position the transport has not stood
/// at for 1 024 frames; a loop wrap is primed for, by a second vocoder the controls send a
/// memory voice under the loop) primes the vocoder over blocks and is silent for 1 024
/// frames (21 ms at 48 kHz) before it sounds on the timeline; a clip placed ahead of the
/// playhead, or at a position the transport has stood at, primes before it plays and is not
/// silent. A voice without a spare, a disk voice's loop wrap included, reseats as a seek.
pub struct VoicePool<F: SamplerFloat> {
    /// The slots, one per voice, summed in key order.
    pub(crate) roster: Roster<F>,
    /// The rows, lent once per block.
    table: Reader<VoiceTable<F>>,
    /// The table's sequence as last applied: a block that lends another merges.
    seen: Option<u64>,
    /// What the controls sent, taken off the ring at the top of a block before the
    /// table is lent, so the table a slot is checked against is at least the one it
    /// was planned for. As long as the ring.
    arrivals: CappedVec<Message<F>>,
    /// The ring the controls send slots and spares through.
    rx: RingReader<Message<F>>,
    /// The engine rate every slot's source is prepared at. Written by `prepare`.
    pub(crate) sample_rate: SampleRate,
    /// The render mode the pool was last prepared in, given to every source
    /// (a fork's file read waits only when it is offline).
    render_mode: RenderMode,
    /// Output width — the node's audio outputs, fixed at construction.
    ///
    /// Declared rather than inferred from the voices it holds: this unit is built
    /// on track creation, *before* any voice exists, and edges are wired against
    /// its shape. A width that followed its contents would re-arity a live graph
    /// node the moment a voice landed.
    pub(crate) channels: ChannelLayout,

    /// The transport as the pool's voices read it, kept across blocks so a
    /// jump (a seek, a loop wrap) is seen where it happens and every slot's
    /// buffered audio is flushed there.
    ///
    /// One clock for the whole reader, not one per slot. Every slot reads
    /// the same transport, so N clocks would be N chances to disagree about
    /// whether the playhead moved — and a disagreement would flush some slots
    /// and not others, which is worse than flushing none.
    pub(crate) clock: Clock,

    /// The transport's loop as the pool last rendered it, shared with the pool's controls.
    loops: Arc<LoopFeed>,

    /// The lanes every slot's block read renders through, one voice at a
    /// time (`PlaybackSlot::render_into`). Built with the pool and sized
    /// from `Prepare::max_block` in `prepare`, on the control thread.
    scratch: BlockScratch<F>,

    /// Whether controls still reach the pool; a fork's are frozen. It
    /// decides how exactly the pool states its ring-out
    /// ([`ring_out`](Self::ring_out)).
    reach: Reach,

    /// The controls, until [`into_parts`](IntoNode::into_parts) hands them out.
    controls: Option<VoicePoolControls<F>>,
}

// Hand-rolled: the roster holds non-`Debug` slots. Print the slot count + scalars
// rather than the slot internals.
impl<F: SamplerFloat> std::fmt::Debug for VoicePool<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoicePool")
            .field("voices", &self.roster.voices.len())
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl<F: SamplerFloat> VoicePool<F> {
    /// The pool over `table` at `channels`, with `voices` places, its controls and
    /// the rings between them. Control side.
    fn over(
        channels: ChannelLayout,
        voices: usize,
        table: Publisher<VoiceTable<F>>,
        reach: Reach,
    ) -> Self {
        let (commands, rx) = ring(message_capacity(voices), Overflow::Refuse);
        let (retired_tx, retired) = ring(retirement_capacity(voices), Overflow::Refuse);
        let room = Arc::<RetireRoom>::default();
        let counters = Arc::<DrainCounters>::default();
        let loops = Arc::<LoopFeed>::default();
        let exit = Exit::new(
            retired_tx,
            retirement_capacity(voices),
            Arc::clone(&room),
            Arc::clone(&counters),
        );
        let controls = VoicePoolControls::new(
            table.clone(),
            commands,
            retired,
            channels,
            voices,
            room,
            counters,
            Arc::clone(&loops),
        );
        Self {
            roster: Roster::new(voices, exit),
            table: table.reader(),
            seen: None,
            arrivals: CappedVec::with_capacity(message_capacity(voices)),
            rx,
            sample_rate: SampleRate::SR_44K1,
            render_mode: RenderMode::Realtime,
            channels,
            clock: Clock::new(),
            loops,
            scratch: BlockScratch::new(UNPREPARED_FRAMES, channels.count() as usize),
            reach,
            controls: Some(controls),
        }
    }

    /// The spare vocoders the pool's voices carry.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn spare_count(&self) -> usize {
        self.roster.voices.iter().filter(|v| v.has_spare()).count()
    }

    /// The controls of a pool driven by hand, without a graph: what a test that
    /// renders a pool itself steers it through. A pool inserted into a graph takes
    /// them from `into_parts`.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn controlled(mut self) -> (Self, VoicePoolControls<F>) {
        let controls = self
            .controls
            .take()
            .expect("a pool's controls are taken once");
        (self, controls)
    }

    /// Output width — the node's audio outputs, as a [`ChannelLayout`]
    /// rather than a bare count. Fixed for the pool's lifetime.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// The ring-out the pool declares ([`Shape::tail`]). A pool its controls can
    /// still add voices to promises [`Tail::Unbounded`]; a fork's voices are all it
    /// will ever have, so it states the longest of theirs
    /// ([`Voice::ring_out`](super::types::Voice::ring_out)).
    fn ring_out(&self) -> Tail {
        match self.reach {
            Reach::Live => Tail::Unbounded,
            Reach::Frozen => self
                .roster
                .voices
                .iter()
                .fold(Tail::None, |tail, v| tail.beside(v.ring_out(Reach::Frozen))),
        }
    }

    /// Number of voice slots currently installed. Diagnostic / test helper.
    pub fn voice_count(&self) -> usize {
        self.roster.voices.len()
    }

    /// A block longer than the scratch, in pieces as long as it, every slot
    /// per piece: only a pool driven by hand without a `prepare` (its
    /// `UNPREPARED_FRAMES`) or past the `max_block` it was prepared for
    /// reaches this.
    #[cold]
    #[inline(never)]
    fn render_in_pieces(
        &mut self,
        clock: &BlockClock<'_>,
        frames: usize,
        n: usize,
        out: &mut [&mut [F]],
    ) {
        let piece = self.scratch.frames();
        let mut from = 0;
        while from < frames {
            let to = (from + piece).min(frames);
            for voice in self.roster.voices.iter_mut().filter(|v| !v.stopped()) {
                voice.render_into(clock, from..to, n, &mut self.scratch, out);
            }
            from = to;
        }
    }

    /// Its scratch's frames and width.
    #[cfg(test)]
    pub(crate) fn scratch_size(&self) -> (usize, usize) {
        (self.scratch.frames(), self.scratch.width())
    }

    /// The top of a block: takes what the controls sent off the ring, lends the
    /// table, and makes the slots what its rows say. Allocation-free, free-free and
    /// send-free.
    fn admit(&mut self) {
        while let Some(message) = self.rx.pop() {
            if let Err(message) = self.arrivals.push_within_capacity(message) {
                // Unreachable: the buffer is as long as the ring. A slot or spare
                // retired here keeps its reservation until collected; a withdrawal
                // (`SetSpare(None)`) carries nothing to retire, so its reservation is
                // released.
                debug_assert!(false, "the arrival buffer holds a whole ring");
                self.roster.exit.retire(match message {
                    Message::Install(slot) => Retired::Slot(slot),
                    Message::SetSpare { spare, .. } => match spare {
                        Some(spare) => Retired::Spare(spare),
                        None => {
                            self.roster.exit.kept();
                            continue;
                        }
                    },
                });
            }
        }
        let table = self.table.lend();
        let seq = table.seq();
        if self.seen != Some(seq) || !self.arrivals.is_empty() || self.roster.stuck {
            self.seen = Some(seq);
            self.roster.merge(table.rows());
        }
        for message in self.arrivals.drain(..) {
            match message {
                Message::Install(slot) => {
                    self.roster
                        .place(table.rows(), slot, self.sample_rate, self.render_mode)
                }
                Message::SetSpare { id, spare } => self.roster.set_spare(id, spare),
            }
        }
    }
}

impl<F: SamplerFloat> From<VoicePoolConfig<F>> for VoicePool<F> {
    /// A pool over the config's table, its voices on their way: the controls have
    /// sent each row's voice, and the first block takes them in. Control side.
    fn from(config: VoicePoolConfig<F>) -> Self {
        let table = Publisher::from_arc(Arc::clone(config.table()));
        let voices = usize::from(config.capacity().get());
        let pool = Self::over(config.channels(), voices, table, Reach::Live);
        if let Some(controls) = &pool.controls {
            controls.send_missing();
        }
        pool
    }
}

impl<F: SamplerFloat> Node<F> for VoicePool<F> {
    /// No inputs, [`channels`](Self::channels) outputs; a source (fed out of
    /// band, through its table and ring), never skipped, with the ring-out of
    /// `ring_out`.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.channels)
            .tail(self.ring_out())
            // Its voices arrive while it plays, so its cost is not known when it is placed.
            .cost(tutti_graph::Cost::Heavy)
    }

    /// The rate every slot and its source run at, and
    /// the controls build vocoders at; the block scratch sized from
    /// `max_block` (a re-prepare at another size resizes it).
    fn prepare(&mut self, p: &Prepare) {
        let sample_rate = p.sample_rate();
        self.sample_rate = sample_rate;
        self.render_mode = p.render_mode();
        self.scratch
            .resize(p.max_block().get(), self.channels.count() as usize);
        for voice in self.roster.voices.iter_mut() {
            voice.set_render(sample_rate, self.render_mode);
        }
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        self.admit();
        self.loops.publish(cx.env.transport.looping);
        let clock = self.clock.observe_with(cx.env, cx.tempo);
        let frames = io.frames();
        let (_, mut outs) = io.split();
        for ch in outs.iter_mut() {
            ch.fill(F::ZERO);
        }
        // Stride derived once per block, above the loops.
        let n = (self.channels.count() as usize)
            .min(outs.len())
            .min(MAX_CHANNELS);
        let mut refs: [&mut [F]; MAX_CHANNELS] = std::array::from_fn(|_| Default::default());
        for (slot, ch) in refs.iter_mut().zip(outs.iter_mut()) {
            *slot = ch;
        }
        for voice in self.roster.voices.iter_mut() {
            voice.receive(self.sample_rate, self.render_mode);
        }
        // Each slot renders its ONE voice into the pool's lanes and adds them
        // in, a lane at a time, via the shared `PlaybackSlot::render_into`
        // (the same per-variant `VoiceSource` match a standalone `VoiceNode`
        // uses), in one piece: in a graph, `prepare` sized the lanes for
        // every block.
        if frames <= self.scratch.frames() {
            for voice in self.roster.voices.iter_mut().filter(|v| !v.stopped()) {
                voice.render_into(&clock, 0..frames, n, &mut self.scratch, &mut refs[..n]);
            }
        } else {
            self.render_in_pieces(&clock, frames, n, &mut refs[..n]);
        }
        Status::Modified
    }

    /// Every slot's buffered audio flushed, the transport forgotten.
    fn reset(&mut self) {
        for voice in self.roster.voices.iter_mut() {
            voice.flush();
        }
        self.clock.reset();
    }
}

/// A pool's fork: a pool over the table as it is when the fork is taken, with its
/// own copies of the voices (see "As a graph node").
struct PoolFork<F: SamplerFloat> {
    channels: ChannelLayout,
    voices: usize,
    /// The pool's table, read with `current` when a fork is taken (an owning read on
    /// the control side, never a lend).
    table: Publisher<VoiceTable<F>>,
}

impl<F: SamplerFloat> ForkSource<F> for PoolFork<F> {
    fn fork(&self, mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        let table = self.table.current();
        let mut pool = self.pool(Publisher::from_arc(Arc::clone(&table)));
        for (key, row) in table.rows() {
            let voice = Box::new(Installed::forked(*key, row, self.channels, mode)?);
            let placed = pool.roster.voices.push_within_capacity(voice);
            debug_assert!(placed.is_ok(), "a table fits the voices of its config");
        }
        Ok(Forked::new(Box::new(pool)))
    }
}

impl<F: SamplerFloat> PoolFork<F> {
    /// A pool over `table` that nothing steers: its controls are not kept.
    fn pool(&self, table: Publisher<VoiceTable<F>>) -> VoicePool<F> {
        let mut pool = VoicePool::over(self.channels, self.voices, table, Reach::Frozen);
        pool.controls = None;
        pool
    }
}

impl<F: SamplerFloat> IntoNode<F> for VoicePool<F> {
    type Controls = VoicePoolControls<F>;

    fn into_parts(mut self) -> NodeParts<VoicePoolControls<F>, F> {
        let controls = self
            .controls
            .take()
            .expect("a pool's controls are taken once");
        let fork = PoolFork {
            channels: self.channels,
            voices: self.roster.capacity,
            table: controls.table().clone(),
        };
        NodeParts::new(Box::new(self), controls.clone(), Some(Box::new(fork)), None)
            .published::<Self, VoiceTableKind<F>>(Arc::new(controls))
    }
}

impl<F: SamplerFloat> HasPublished<VoiceTableKind<F>> for VoicePool<F> {}
