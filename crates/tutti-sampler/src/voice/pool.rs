//! [`VoicePool`] — one graph node that plays every voice on a track.
//!
//! The pool owns its slots, drains the commands its handle queued each buffer, and sums the
//! results itself. One node per track rather than one per voice plus a dynamic
//! summing unit: adding or removing a voice then costs a queued command instead
//! of a graph edit, which is what keeps voice churn off the commit path.

use crate::PlaceError;
use crate::streamer::StreamId;
use tutti_buffers::MAX_CHANNELS;

use super::clock::{BlockClock, Clock};
use super::command::{
    COMMAND_CAPACITY, GainError, Live, LoopFeed, MAX_RESIDENT_VOICES, Queued, Tier, VoiceCommand,
    VoicePoolHandle,
};
use super::loop_span::Loop;
use super::memory_source::MemorySource;
use super::slot::{PlaybackSlot, stretch_wanted};
use super::stretched::{self, Vocoders};
use super::types::{Playback, Reach, SlotId, Voice, VoiceSource};
use crate::SamplerFloat;
use crate::lanes::{BlockScratch, UNPREPARED_FRAMES};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tutti_buffers::ChannelLayout;
use tutti_buffers::Wave;
use tutti_graph::{
    Cx, ForkCause, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare, RenderMode, Shape,
    Status,
};
use tutti_rt::{CappedVec, Mutex, Overflow, RingReader, RingWriter, ring};
use tutti_timeline::{LoopReader, TempoReader};
use tutti_units::{SampleRate, Tail};

// ---------------------------------------------------------------------------
// Retirement — values the audio thread must not drop.
// ---------------------------------------------------------------------------

/// Something the drain took ownership of and must not free in the callback.
///
/// Every variant exists for the same reason: dropping it can free a large
/// allocation — a vocoder pair's rings (about 120 KB per channel), or a
/// decoded [`Wave`] whose last reference it is. Rather than teach each call
/// site its own evasion, everything the drain needs to shed goes down one
/// ring and is freed by [`VoicePoolHandle::collect_retired`] on the
/// control thread.
///
/// The ring always has room for what the drain pushes: the handle reserves a
/// slot of it for every command that might retire a value, and refuses the
/// command with [`SendError::RetirementFull`](super::command::SendError::RetirementFull)
/// when none is left, so the drain never drops one.
///
/// `pub(crate)`, not `pub`: [`PlaybackSlot`] is crate-private, and the retirement
/// ring is an internal thread-handoff detail. Callers only ever see the count
/// from [`VoicePoolHandle::collect_retired`], never the values.
///
/// # Why the payloads are never read
///
/// No field is ever inspected, and that is the design: the *value* is the
/// payload, and receiving it is what frees it. `collect_retired` pulls each one
/// and lets it fall out of scope, on the control thread. So `dead_code` is
/// correct that nothing reads them, and wrong that they are dead — deleting
/// any field would move the deallocation back into the audio callback.
///
/// The variants are also very different sizes (a `PlaybackSlot` dwarfs a bare
/// vocoder), which normally argues for boxing the large one. Not here: this value
/// exists to cross a thread boundary and be dropped, so a `Box` would add an
/// allocation on one side and a free on the other — the exact cost being avoided.
/// The ring holds `MAX_RESIDENT_VOICES`, so the waste is one slot-sized
/// element per queue entry, bounded and never in the callback's path.
#[allow(
    dead_code,
    clippy::large_enum_variant,
    reason = "the payloads exist to be received and dropped on the control thread, never read; boxing the large variant would re-add the audio-thread free this type exists to avoid"
)]
pub(crate) enum Retired<F: SamplerFloat> {
    /// A slot removed by `VoiceCommand::Remove`, vocoder and all.
    Slot(PlaybackSlot<F>),
    /// A vocoder the sender built for an `UpdateStretch` that turned out
    /// not to need it, because the slot already had one.
    ///
    /// The sender cannot know whether a vocoder is resident — that is audio-thread
    /// state — so it builds whenever the new values ask for stretching and accepts
    /// that some arrivals are redundant. Handing the spare back here is what keeps
    /// that trade free of an audio-thread free.
    ///
    /// The box the command carried: the drain moves it to the ring and the
    /// control thread frees it.
    Stretch(Box<Vocoders<F>>),
    /// A spare vocoder a `SetSpare` took out of a slot (the loop moved off the voice), or
    /// found no use for (the slot has one, or no vocoder, or is gone).
    Spare(Box<tutti_dsp::PhaseVocoder<F>>),
    /// A wave a `ReplaceWave` let go of: the one a memory slot swapped out,
    /// or the new one when no memory slot took it.
    ///
    /// An `Arc`, so retiring it frees nothing unless this was the last
    /// reference, and then it frees the whole decoded buffer: the reason it
    /// is retired at all.
    Wave(Arc<Wave<F>>),
    /// A voice an `AddVoice` brought to a pool already holding
    /// `MAX_RESIDENT_VOICES` slots, refused whole: its box, and the vocoder
    /// the sender built for it.
    ///
    /// Taking it would grow the slot vector (a reallocation in the
    /// callback), and dropping it would free the box, the source and the
    /// vocoder there. Counted as [`PoolDrops::voices_refused`].
    Refused {
        /// The slot the add named, which the handle frees on collecting this.
        id: SlotId,
        /// The stamp of the add, so the handle frees the slot only if it is
        /// still that add's.
        generation: u64,
        voice: Box<Voice<F>>,
        stretch: Option<Box<Vocoders<F>>>,
    },
}

/// What a [`VoicePool`] and its handle dropped, refused or freed where they
/// should not have, since the pool was built: a reading a host polls
/// ([`VoicePoolHandle::drops`]), never a log line from the callback.
///
/// Each is a count of commands (or values) and only ever grows. They are taken
/// by the drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PoolDrops {
    /// Loop commands (set or cleared) the drain found aimed at a streaming
    /// slot whose stream the handle had not passed them to: a slot the
    /// handle's stream no longer belongs to. Not recorded, since the stream
    /// was not told.
    pub loops_not_forwarded: u64,
    /// Streaming directions (reverse, or back to forward) the drain found
    /// aimed at a slot whose stream the handle had not passed them to. Not
    /// recorded in the voice's `Playback`.
    pub directions_dropped: u64,
    /// [`replace_wave`](VoicePoolHandle::replace_wave) calls aimed at a
    /// streaming voice, which changes source by a fresh
    /// [`add_streaming`](VoicePoolHandle::add_streaming) instead.
    pub wave_swaps_ignored: u64,
    /// Adds for a new slot refused because the pool already held
    /// `MAX_RESIDENT_VOICES` (64) slots. The voice goes back through
    /// the retirement ring to be freed by
    /// [`collect_retired`](VoicePoolHandle::collect_retired).
    pub voices_refused: u64,
    /// Retired values the drain could neither queue nor park, and leaked
    /// rather than free in the callback. Always zero unless the retirement
    /// bound is wrong: a reservation exists for every value, and values
    /// retired after the handles are gone are parked in the pool.
    pub leaked: u64,
    /// Spare vocoders (or their withdrawal) the handle could not queue because the
    /// command ring or the retirement room was full. Nothing was sent: the next tick
    /// retries, and a wrap met meanwhile flushes as a seek does.
    pub spares_deferred: u64,
}

/// The retirement slots one handle has reserved of its ring: values in the
/// ring, plus commands queued that may still retire one. Never above the
/// ring's capacity, which is what lets the drain's push always fit. Each
/// `with_handle` makes a fresh one, shared by the pool and that handle only,
/// so an older handle (with its older ring) never counts against the new one.
#[derive(Debug, Default)]
pub(crate) struct RetireRoom {
    reserved: AtomicUsize,
}

impl RetireRoom {
    /// Reserves one of `capacity` slots for a command about to be queued;
    /// `false` when none is left. Called with the command writer locked, so
    /// reservations are serial and only releases race with it.
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

    /// Gives `n` reserved slots back: by the drain for a command that
    /// retired nothing, by `collect_retired` for the values it took.
    pub(crate) fn release(&self, n: usize) {
        self.reserved.fetch_sub(n, Ordering::AcqRel);
    }
}

/// The atomics behind [`PoolDrops`], shared by a pool and its handle. Bumped
/// with a relaxed add (by the drain on the audio thread); read on the control
/// thread.
#[derive(Debug, Default)]
pub(crate) struct DrainCounters {
    pub(crate) loops_not_forwarded: AtomicU64,
    pub(crate) directions_dropped: AtomicU64,
    wave_swaps_ignored: AtomicU64,
    voices_refused: AtomicU64,
    leaked: AtomicU64,
    pub(crate) spares_deferred: AtomicU64,
    /// Commands the handle has queued: not a drop, so not in [`PoolDrops`]. A
    /// pool whose count is zero holds the voices it was built with and no
    /// others, which is what lets a fork copy it.
    queued: AtomicU64,
}

impl DrainCounters {
    /// Whether the handle has queued a command, which the audio thread may
    /// have applied: the pool's voices are then not known here.
    pub(crate) fn commanded(&self) -> bool {
        self.queued.load(Ordering::Relaxed) > 0
    }

    pub(crate) fn note_queued(&self) {
        Self::bump(&self.queued);
    }

    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> PoolDrops {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        PoolDrops {
            loops_not_forwarded: load(&self.loops_not_forwarded),
            directions_dropped: load(&self.directions_dropped),
            wave_swaps_ignored: load(&self.wave_swaps_ignored),
            voices_refused: load(&self.voices_refused),
            leaked: load(&self.leaked),
            spares_deferred: load(&self.spares_deferred),
        }
    }
}

/// Where a voice's loop is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopRoute {
    /// On the voice's in-memory source, by the drain.
    Memory,
    /// On its streamer stream (the stream it reads, which a loop is set on
    /// through the handle's [`StreamHandle`](crate::StreamHandle), from the control
    /// thread).
    Stream(StreamId),
}

impl<F: SamplerFloat> Voice<F> {
    /// Where this voice's loop is applied.
    pub(crate) fn loop_route(&self) -> LoopRoute {
        match &self.source {
            VoiceSource::Memory(_) => LoopRoute::Memory,
            VoiceSource::Disk(disk) => LoopRoute::Stream(disk.stream()),
        }
    }
}

// ---------------------------------------------------------------------------
// VoicePool — the graph node.
// ---------------------------------------------------------------------------

/// Every voice on one track, played and summed by a single graph node.
///
/// Zero inputs, [`channels`](Self::channels) outputs. Each block it drains the
/// command queue, reads the transport from the block's `Env`, then reads and
/// sums its slots — all of it on the audio thread, and all of it
/// allocation-free provided the control side did its share:
/// `VoicePoolHandle::send` builds any vocoder a command implies, and
/// [`VoicePoolHandle::collect_retired`] frees what the drain hands back.
///
/// # As a graph node
///
/// A `tutti_graph::Node`. Its controls ([`IntoNode`]) are its
/// [`VoicePoolHandle`]: the command queue it drains at the top of each block
/// (a bounded lock-free queue the node owns the receiving end of), and the
/// retirement ring back. Voices placed on the timeline read the transport
/// from each block's `Env`, frame-exact at their windows' edges.
///
/// A fork of a pool is a pool at its width holding copies of the voices it was
/// built with ([`insert`](Self::insert)), with no command queue and no
/// streamer. That is the pool's state only while its handle has queued nothing:
/// a voice added or edited through the handle is applied on the audio thread,
/// where no control-side copy of it exists. So a fork of a pool whose handle
/// has queued a command is refused, naming the node, and a host that renders a
/// track's clips offline builds them into the pool with
/// [`insert`](Self::insert) before inserting it.
///
/// Both source tiers live here side by side, as the `Memory` / `Disk` arms of
/// [`VoiceSource`]; the slot's read forks on that enum rather than on a trait,
/// which is what keeps a per-tier difference visible at the call site.
///
/// # Stretched voices
///
/// The pool declares no latency. A stretched voice of either tier seats its vocoder on the
/// source's own history (the file's, for a disk voice, from its ring) and sounds on time.
///
/// A stretching voice's seat (a seek, or play from a position the transport has not stood
/// at for 1 024 frames; a loop wrap is primed for, by a second vocoder the handle sends a
/// memory voice under the loop) primes the vocoder over blocks and is silent for 1 024
/// frames (21 ms at 48 kHz) before it sounds on the timeline; a clip placed ahead of the
/// playhead, or at a position the transport has stood at, primes before it plays and is not
/// silent. A voice without a spare, a disk voice's loop wrap included, reseats as a seek.
pub struct VoicePool<F: SamplerFloat> {
    /// The resident slots, one per voice, summed in order. Reserved to
    /// `MAX_RESIDENT_VOICES` at construction, and the audio-thread
    /// `AddVoice` drain never grows it: an add for a new slot once it is full
    /// is refused and retired (`Retired::Refused`). Only a control-thread
    /// [`insert`](Self::insert) may grow it, and the drain then
    /// fills the larger capacity before refusing.
    pub(crate) voices: Vec<PlaybackSlot<F>>,
    /// Commands from the control thread, drained at the top of every block.
    /// This pool's own: a dead one until [`into_parts`](IntoNode::into_parts)
    /// (or [`with_handle`](Self::with_handle)) hands out the sending end; a
    /// clone gets a dead one — see this type's `Clone`.
    ///
    /// Both rings of the pool hold boxes: the pool is dropped on the control
    /// side, with its graph unit (`tutti_rt::ring`'s contract), so a command
    /// still queued, or a retirement pushed as the handle dropped, is freed
    /// there.
    pub(crate) rx: RingReader<Queued<F>>,

    /// Where removed slots go to be freed, off the audio thread.
    ///
    /// `VoiceCommand::Remove` is handled inside `drain_commands`, which runs
    /// from `tick`/`process` — the audio callback. Dropping the slot there frees
    /// its vocoder's rings (about 60 KB per channel) in the callback.
    ///
    /// Pushing to a bounded ring instead is lock-free and allocation-free.
    /// The control thread drains it via [`VoicePoolHandle::collect_retired`];
    /// if nobody does, the handle refuses the commands that would retire
    /// ([`SendError::RetirementFull`](super::command::SendError::RetirementFull)),
    /// so the ring never overflows.
    pub(crate) retired: RingWriter<Retired<F>>,
    /// Whether the command being drained retired a value.
    retired_now: bool,
    /// Values the drain retired after every handle was dropped, when the
    /// retirement ring has no reader: held until the pool is dropped, which
    /// is on the control side. Sized `COMMAND_CAPACITY` by `with_handle`, the
    /// most commands that can still be queued once the handles are gone.
    stranded: CappedVec<Retired<F>>,
    /// The retirement slots reserved of `retired`, shared with the handle.
    pub(crate) room: Arc<RetireRoom>,
    /// What the drain dropped or refused, shared with the handle
    /// ([`VoicePoolHandle::drops`]).
    pub(crate) counters: Arc<DrainCounters>,
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

    /// The project's tempo map, when the pool reads one:
    /// lent once per block to the clock every slot's placement reads. Without
    /// one, a voice takes the tempo of the transport as the block carries it.
    tempo: Option<TempoReader>,
    /// The transport's loop as the control side knows it, shared with the pool's handles.
    loops: Arc<LoopFeed>,

    /// The lanes every slot's block read renders through, one voice at a
    /// time (`PlaybackSlot::render_into`). Built with the pool and sized
    /// from `Prepare::max_block` in `prepare`, on the control thread.
    scratch: BlockScratch<F>,

    /// Whether a handle still reaches the pool; a fork's is frozen. It
    /// decides how exactly the pool states its ring-out
    /// ([`ring_out`](Self::ring_out)).
    reach: Reach,
}

// Hand-rolled: `voices` holds non-`Debug` `PlaybackSlot`s (each wraps a sampler +
// stretch DSP). Print the slot count + scalars rather than the slot internals.
impl<F: SamplerFloat> std::fmt::Debug for VoicePool<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoicePool")
            .field("voices", &self.voices.len())
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl<F: SamplerFloat> VoicePool<F> {
    /// A pool with no command queue and no retirement ring yet (both
    /// dead): [`with_handle`](Self::with_handle) or
    /// [`into_parts`](IntoNode::into_parts) makes them.
    fn from_parts(channels: impl Into<ChannelLayout>) -> Self {
        // A zero-capacity ring never accepts: a pool nothing controls (a fork,
        // which owns its voices outright) receives no command, so retires
        // nothing.
        let (retired, _) = ring(0, Overflow::Refuse);
        let sample_rate = SampleRate::SR_44K1;
        let channels = channels.into();
        Self {
            retired,
            retired_now: false,
            stranded: CappedVec::with_capacity(0),
            room: Arc::default(),
            counters: Arc::default(),
            // Reserved, not empty: `AddVoice` is drained inside `process`, so
            // a `push` that grows this vector would be a reallocation in the
            // audio callback. The drain refuses an add past the capacity
            // instead (`Retired::Refused`).
            voices: Vec::with_capacity(MAX_RESIDENT_VOICES),
            rx: ring(0, Overflow::Refuse).1,
            sample_rate,
            render_mode: RenderMode::Realtime,
            channels,
            clock: Clock::new(),
            tempo: None,
            loops: Arc::default(),
            scratch: BlockScratch::new(UNPREPARED_FRAMES, channels.count() as usize),
            reach: Reach::Live,
        }
    }

    /// This pool reading the project's tempo map through `tempo`
    /// ([`TransportSettings::tempo_reader`](tutti_timeline::TransportSettings::tempo_reader)).
    ///
    /// A placed voice plays at its own speed from the frame the playhead
    /// reaches its start, so over a map whose tempo changes (steps, ramps)
    /// how long it has played is the map's seconds between its start and
    /// the playhead, not the beats over the tempo of the moment: without the
    /// map a voice is re-seated at every change as though it had started at
    /// the new tempo. A voice with no map (and a constant one) keeps the
    /// transport's tempo, bit for bit. A fork reads the same map.
    #[must_use]
    pub fn tempo(mut self, tempo: TempoReader) -> Self {
        self.tempo = Some(tempo);
        self
    }

    /// This pool reading the transport's loop through `reader`
    /// ([`MotionFsm::loop_reader`](tutti_timeline::MotionFsm::loop_reader)).
    ///
    /// The pool's handle reads the loop region on its reclaim tick, control-side, and sends a spare
    /// vocoder to each stretched voice whose window covers the loop's start (see
    /// [`VoicePoolHandle::follow_loop`], which does the same for a pool no
    /// transport is bound to). A fork reads no transport: its voices keep the spares
    /// they were built with.
    #[must_use]
    pub fn loops(self, reader: LoopReader) -> Self {
        self.loops.bind(reader);
        self
    }

    /// The spare vocoders the pool's voices carry.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn spare_count(&self) -> usize {
        self.voices.iter().filter(|s| s.has_spare()).count()
    }

    /// Output width — the node's audio outputs, as a [`ChannelLayout`]
    /// rather than a bare count. Fixed for the pool's lifetime.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// A stereo pool. Placed voices play their windows of the transport; for
    /// one at a non-stereo width, [`with_channels`](Self::with_channels). Its
    /// controls come with it into the graph ([`IntoNode`]).
    ///
    /// A streaming voice's loop and direction are its [`StreamHandle`](crate::StreamHandle)'s,
    /// set from the control thread, never from here: a streaming voice is
    /// added with its stream through the handle
    /// ([`add_streaming`](VoicePoolHandle::add_streaming)), and the pool
    /// takes no other.
    #[must_use]
    pub fn new() -> Self {
        Self::from_parts(ChannelLayout::STEREO)
    }

    /// A pool at an explicit output width.
    ///
    /// Each slot's vocoder is built at this width too, so a wide voice is
    /// not truncated on the stretch path.
    ///
    /// # Errors
    ///
    /// [`PlaceError::WidthTooWide`] past [`MAX_CHANNELS`]: the pool's read
    /// stacks and lanes are that wide, so a wider pool would declare outputs
    /// it never writes (a caller reading them sees whatever the buffer held).
    /// [`PlaceError::EmptyWidth`] for no channels: a node with no outputs is
    /// one nothing can be wired to.
    pub fn with_channels(channels: impl Into<ChannelLayout>) -> Result<Self, PlaceError> {
        Ok(Self::from_parts(crate::voice_width(channels.into())?))
    }

    /// This pool with a fresh command queue and retirement ring, and the
    /// handle that drives them: what [`into_parts`](IntoNode::into_parts)
    /// hands the graph and the caller. For a host (or a test) that calls
    /// the node by hand. A handle taken before goes dead
    /// ([`SendError::Disconnected`](super::command::SendError::Disconnected)).
    ///
    /// The handle holds the [`StreamHandle`](crate::StreamHandle) of every streaming voice
    /// added through it ([`add_streaming`](VoicePoolHandle::add_streaming)): a
    /// loop or a direction set for one is set on its stream.
    #[must_use]
    pub fn with_handle(mut self) -> (Self, VoicePoolHandle<F>) {
        let (tx, rx) = ring(COMMAND_CAPACITY, Overflow::Refuse);
        let (retired_tx, retired) = ring(MAX_RESIDENT_VOICES, Overflow::Refuse);
        self.rx = rx;
        self.retired = retired_tx;
        self.stranded = CappedVec::with_capacity(COMMAND_CAPACITY);
        self.room = Arc::default();
        // The handle mirrors the reader's width and shares its rate so `send`
        // can build a vocoder that matches it, on the control thread.
        let handle = VoicePoolHandle {
            tx: Arc::new(Mutex::new(tx)),
            retired: Arc::new(Mutex::new(retired)),
            channels: self.channels,
            counters: Arc::clone(&self.counters),
            room: Arc::clone(&self.room),
            streams: Arc::default(),
            free: Arc::default(),
            // Voices inserted before the handle was made are live too.
            loops: Arc::clone(&self.loops),
            live: Arc::new(Mutex::new(
                self.voices
                    .iter()
                    .map(|s| {
                        let tier = match s.voice.source {
                            VoiceSource::Memory(_) => Tier::Memory,
                            VoiceSource::Disk(_) => Tier::Disk,
                        };
                        // Stamp 0: no add of the handle's, never refused.
                        let window = match &s.voice.source {
                            VoiceSource::Memory(source) => source.window(),
                            VoiceSource::Disk(_) => None,
                        };
                        let mut live = Live::added(tier, 0, window, s.stretch.is_some());
                        live.spare = s.has_spare();
                        (s.id, live)
                    })
                    .collect(),
            )),
            stamps: Arc::default(),
        };
        (self, handle)
    }

    /// The ring-out the pool declares ([`Shape::tail`]). A pool a handle can
    /// still add voices to promises [`Tail::Unbounded`]; a fork's voices are
    /// all it will ever have, so it states the longest of theirs
    /// ([`Voice::ring_out`]).
    fn ring_out(&self) -> Tail {
        match self.reach {
            Reach::Live => Tail::Unbounded,
            Reach::Frozen => self.voices.iter().fold(Tail::None, |tail, slot| {
                tail.beside(slot.voice.ring_out(Reach::Frozen))
            }),
        }
    }

    /// Number of voice slots currently materialised (drained from the command
    /// queue). Diagnostic / test helper.
    pub fn voice_count(&self) -> usize {
        self.voices.len()
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
            for slot in &mut self.voices {
                slot.render_into(clock, from..to, n, &mut self.scratch, out);
            }
            from = to;
        }
    }

    /// Its scratch's frames and width.
    #[cfg(test)]
    pub(crate) fn scratch_size(&self) -> (usize, usize) {
        (self.scratch.frames(), self.scratch.width())
    }

    /// The control intent recorded for a slot. Test helper: lets a test assert
    /// that a dropped command did NOT leave a `Playback` claiming it applied.
    #[cfg(test)]
    pub(crate) fn playback_of(&self, id: SlotId) -> Option<&Playback> {
        self.voices
            .iter()
            .find(|s| s.id == id)
            .map(|s| &s.voice.play)
    }

    /// Drops every voice slot. Control thread (it frees them).
    pub fn clear_voices(&mut self) {
        self.voices.clear();
    }

    fn slot_mut(&mut self, id: SlotId) -> Option<&mut PlaybackSlot<F>> {
        self.voices.iter_mut().find(|s| s.id == id)
    }

    /// Apply a loop setting to a slot, by tier, and record it in the slot's
    /// `Playback`.
    ///
    /// - In-memory: primes / clears the loop range on the `MemorySource`
    ///   in-unit (`MemorySource::set_loop`).
    /// - Streaming: the loop is the stream's — the streamer reads the loop's
    ///   fade lead-in off disk and publishes the loop as applied, neither
    ///   reachable from the reader — and the control thread has already set it
    ///   on the stream's handle ([`VoicePoolHandle::send`]), so here it is
    ///   only recorded. Its callers are `insert_voice_inner`, whose voice
    ///   carries a loop only once its stream took it (its sender set it `Off`
    ///   otherwise), and [`apply_queued_loop`](Self::apply_queued_loop), which
    ///   calls it for a streaming slot only when the command named that
    ///   slot's stream; so the record never claims a loop no stream was
    ///   given.
    ///
    /// Allocation-free and send-free: the drain runs it.
    fn apply_loop(&mut self, id: SlotId, setting: Option<Loop>) {
        let Some(slot) = self.voices.iter_mut().find(|s| s.id == id) else {
            return;
        };
        if let VoiceSource::Memory(sampler) = &mut slot.voice.source {
            sampler.set_loop(setting);
        }
        slot.voice.play.loop_ = setting;
    }

    /// A queued loop command's setting, applied as
    /// [`apply_loop`](Self::apply_loop) does, except that a streaming slot
    /// records it only when `forwarded` names that slot's own stream: the
    /// handle holds that stream and sets the loop on it. Otherwise (the handle
    /// did not hold the slot's stream) it is counted
    /// ([`PoolDrops::loops_not_forwarded`]) and the slot keeps its loop, so
    /// the decision rests on the command, not on the handle's table.
    ///
    /// Allocation-free and send-free: the drain runs it.
    fn apply_queued_loop(
        &mut self,
        id: SlotId,
        setting: Option<Loop>,
        forwarded: Option<StreamId>,
    ) {
        let Some(slot) = self.voices.iter().find(|s| s.id == id) else {
            return;
        };
        if let LoopRoute::Stream(stream) = slot.voice.loop_route()
            && forwarded != Some(stream)
        {
            DrainCounters::bump(&self.counters.loops_not_forwarded);
            return;
        }
        self.apply_loop(id, setting);
    }

    /// A queued direction command, recorded in the slot's `Playback`. A streaming
    /// slot records it only when `forwarded` names its own stream, as a loop is
    /// ([`apply_queued_loop`](Self::apply_queued_loop)); otherwise it is counted
    /// ([`PoolDrops::directions_dropped`]) and the slot keeps its direction.
    /// In-memory: `play.direction` is what the hot read reverses by.
    ///
    /// Allocation-free and send-free: the drain runs it.
    fn apply_queued_direction(
        &mut self,
        id: SlotId,
        direction: super::types::PlayDirection,
        forwarded: Option<StreamId>,
    ) {
        let Some(slot) = self.voices.iter_mut().find(|s| s.id == id) else {
            return;
        };
        if let LoopRoute::Stream(stream) = slot.voice.loop_route()
            && forwarded != Some(stream)
        {
            DrainCounters::bump(&self.counters.directions_dropped);
            return;
        }
        slot.voice.play.direction = direction;
    }

    /// Inserts an in-memory voice as a new slot, `source` played as `play`
    /// says (its gain, speed, direction, loop, stretch and pitch; the loop in
    /// `play` is the voice's, replacing any the source holds), building the
    /// vocoder here if the voice needs one. An insert at an id already
    /// resident replaces that slot.
    ///
    /// For a pool that is not yet in a graph (a host rebuilding a track's
    /// clips offline before inserting the pool); a live pool is added to
    /// through its handle ([`VoicePoolHandle::add`]). A streaming voice has no
    /// door here: its loop and direction are its stream's, set through the
    /// handle ([`VoicePoolHandle::add_streaming`]).
    ///
    /// Allocates (the vocoder, when the voice stretches, and the slot's box):
    /// control thread only.
    ///
    /// # Errors
    ///
    /// [`GainError::NotFinite`] for a `play` gain that is not finite; nothing
    /// is inserted.
    pub fn insert(
        &mut self,
        id: SlotId,
        source: MemorySource<F>,
        play: Playback,
    ) -> Result<(), GainError> {
        GainError::check(play.gain)?;
        self.insert_voice(
            id,
            Voice {
                source: VoiceSource::Memory(source),
                play,
            },
        );
        Ok(())
    }

    /// [`insert`](Self::insert) for a voice of either tier. The `Playback` is
    /// control-INTENT, and each tier applies it its own way: `VoiceSource::apply_*`
    /// (in-memory stores on the `MemorySource`, streaming writes the stream's
    /// cells the streamer ring also reads) for gain and speed, `apply_loop`
    /// for the loop (in-memory primes it in-unit; a streaming loop is the
    /// stream's, set by the control thread first, then recorded). A streaming
    /// voice's direction is the stream's too, so it is only recorded.
    pub(crate) fn insert_voice(&mut self, id: SlotId, voice: Voice<F>) {
        // A pool driven by hand has no handle to send a spare later: it carries one.
        let stretch =
            stretch_wanted(&voice.play).then(|| stretched::vocoder::<F>(self.channels, true));
        // Control side: a slot this replaces is freed here.
        drop(self.insert_voice_inner(id, Box::new(voice), stretch));
    }

    /// Allocation-free, free-free and send-free, so the drain can run it: the
    /// voice arrives boxed and the box moves into the slot, and a slot this
    /// one replaces is returned, for the caller to retire or free. A streaming voice's loop and direction arrive already set on its
    /// stream (or `Off` and forward); see [`apply_loop`](Self::apply_loop).
    pub(super) fn insert_voice_inner(
        &mut self,
        id: SlotId,
        mut voice: Box<Voice<F>>,
        stretch: Option<Box<Vocoders<F>>>,
    ) -> Option<PlaybackSlot<F>> {
        let displaced = self
            .voices
            .iter()
            .position(|s| s.id == id)
            .map(|i| self.voices.swap_remove(i));
        // Take the loop out, leaving `Off` behind: `apply_loop` needs the slot
        // present to look it up, and applies it (an in-memory loop is primed
        // on the source) as it records it.
        let loop_ = voice.play.loop_.take();
        let gain = voice.play.gain;
        let speed = voice.play.speed;
        let direction = voice.play.direction;
        // The slot is built at THIS READER's width: a slot narrower than the
        // reader would truncate on the stretch path only, which no stereo test
        // can observe. Its vocoder, if any, was built by the sender on
        // the control thread (`VoicePoolHandle::send`) and only moves in here.
        let mut slot = PlaybackSlot::with_channels(id, voice, self.channels);
        slot.stretch = stretch;
        // The source runs at the pool's rate (the memory tier's conversion,
        // the disk tier's step), as `prepare` sets every resident slot's.
        // Allocation-free: a rate and a ratio.
        slot.voice
            .source
            .set_render(self.sample_rate, self.render_mode);
        self.voices.push(slot);

        // Realise the remaining intent through the same appliers the update
        // commands use. The slot is now present, so `slot_mut` / `apply_loop`
        // find it by id.
        if let Some(slot) = self.slot_mut(id) {
            slot.voice.source.apply_gain(gain);
            slot.voice.play.gain = gain;
            slot.voice.source.apply_speed(speed);
            slot.voice.play.speed = speed;
            slot.voice.play.direction = direction;
        }
        self.apply_loop(id, loop_);
        displaced
    }

    /// Hands a value the drain let go of to the control thread.
    ///
    /// The ring always has room: the handle reserved a slot for the command
    /// being drained before queueing it (`VoicePoolHandle::send`), a command
    /// retires at most one value, and `drain_commands` stops while the ring
    /// is full. A push the ring refuses can only be a disconnected ring
    /// (every handle dropped, so nothing will collect): the value is parked
    /// in `stranded` and freed with the pool on the control side.
    fn retire(&mut self, value: Retired<F>) {
        self.retired_now = true;
        let Err(returned) = self.retired.push(value) else {
            return;
        };
        debug_assert!(
            self.retired.is_disconnected(),
            "the retirement ring has room for every reservation"
        );
        if let Err(last) = self.stranded.push_within_capacity(returned.value) {
            // Unreachable: once the handles are gone no command arrives, and
            // at most `COMMAND_CAPACITY` are queued, each retiring one value.
            debug_assert!(
                false,
                "more retirements after disconnect than queued commands"
            );
            DrainCounters::bump(&self.counters.leaked);
            std::mem::forget(last);
        }
    }

    fn drain_commands(&mut self) {
        // A command that may retire is taken only while the ring has room for
        // its value, which the handle's reservation guarantees.
        while let Some(next) = self.rx.peek() {
            if next.cmd.may_retire() && self.retired.is_full() {
                debug_assert!(false, "a reservation keeps room in the retirement ring");
                break;
            }
            let Some(Queued {
                cmd,
                forwarded,
                generation,
            }) = self.rx.pop()
            else {
                break;
            };
            let reserved = cmd.may_retire();
            self.retired_now = false;
            match cmd {
                VoiceCommand::AddVoice { id, voice, stretch } => {
                    // A new slot in a full vector would grow it here, in the
                    // callback. Refuse it whole instead (an add at a resident
                    // id replaces in place and always fits).
                    let replaces = self.voices.iter().any(|s| s.id == id);
                    if !replaces && self.voices.len() == self.voices.capacity() {
                        DrainCounters::bump(&self.counters.voices_refused);
                        self.retire(Retired::Refused {
                            id,
                            generation,
                            voice,
                            stretch,
                        });
                        continue;
                    }
                    // The vocoder arrives prebuilt from `send` (control thread);
                    // this only moves it into the slot.
                    if let Some(old) = self.insert_voice_inner(id, voice, stretch) {
                        self.retire(Retired::Slot(old));
                    }
                }
                VoiceCommand::Remove(id) => {
                    // `retain` would DROP the slot here, on the audio thread,
                    // freeing its vocoder's rings inside the
                    // callback. Swap it out and hand it to the control thread
                    // instead — a lock-free push, no free.
                    if let Some(i) = self.voices.iter().position(|s| s.id == id) {
                        let slot = self.voices.swap_remove(i);
                        self.retire(Retired::Slot(slot));
                    }
                }
                VoiceCommand::ReplaceWave { id, wave } => {
                    // Whichever wave this arm ends up not keeping — the old
                    // one a memory slot swaps out, or the new one when there
                    // is no memory slot to take it — may hold the last
                    // reference to a decoded buffer, and dropping it here
                    // would free that buffer in the callback. It leaves
                    // through the retirement ring instead.
                    let shed = match self.slot_mut(id).map(|s| &mut s.voice.source) {
                        // Match the tier explicitly rather than calling through
                        // a shared setter: the streaming half has nothing to
                        // do, and a shared setter would make a `ReplaceWave`
                        // aimed at a disk voice do nothing and say nothing.
                        // Swapping a streaming source means re-registering the
                        // streamer stream on a different file, which is a
                        // control-thread op issued as a fresh `AddVoice`.
                        Some(VoiceSource::Memory(sampler)) => sampler.replace_wave(wave),
                        Some(VoiceSource::Disk(_)) => {
                            DrainCounters::bump(&self.counters.wave_swaps_ignored);
                            wave
                        }
                        None => wave,
                    };
                    self.retire(Retired::Wave(shed));
                }
                VoiceCommand::UpdatePlacement { id, window } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // The source owns the window: each backend's setter
                        // overwrites it, and the next block's read places the
                        // voice by it.
                        slot.voice.source.apply_window(Some(window));
                    }
                }
                VoiceCommand::UpdateGain { id, gain } => {
                    if let Some(slot) = self.slot_mut(id) {
                        slot.voice.source.apply_gain(gain);
                        slot.voice.play.gain = gain;
                    }
                }
                VoiceCommand::UpdateSpeed { id, speed } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Unified across tiers: both backends carry speed in-unit.
                        // In-memory stores it on the `MemorySource`; streaming
                        // writes the stream's cells, which the reader holds
                        // and the streamer reads to size its fills.
                        // `apply_speed` covers both, so a host never forks
                        // streaming speed onto a streamer command.
                        slot.voice.source.apply_speed(speed);
                        slot.voice.play.speed = speed;
                    }
                }
                VoiceCommand::UpdateLoop { id, setting } => {
                    // A streaming slot's loop is set on its stream by the handle
                    // (`VoicePoolHandle::send`) if `forwarded` names it.
                    self.apply_queued_loop(id, setting, forwarded);
                }
                VoiceCommand::UpdateReverse { id, direction } => {
                    // In-memory: `voice.play.direction` drives the reversed index
                    // in the hot read. Streaming: the stream's direction is its
                    // ring's (the handle set it on the stream), and
                    // `voice.play.direction` only records it. One command reaches
                    // both, so a host sends reverse ONCE.
                    self.apply_queued_direction(id, direction, forwarded);
                }
                VoiceCommand::SetSpare { id, spare } => {
                    // Whatever the slot does not keep is retired, never dropped here.
                    let left = match self.slot_mut(id) {
                        Some(slot) => slot.set_spare(spare),
                        None => spare,
                    };
                    if let Some(unit) = left {
                        self.retire(Retired::Spare(unit));
                    }
                }
                VoiceCommand::UpdateStretch {
                    id,
                    stretch_factor,
                    pitch_cents,
                    stretch,
                } => {
                    // Take the surplus out of `set_stretch` and retire it rather
                    // than letting it drop here: this is the audio callback, and
                    // dropping a vocoder frees its rings. Same
                    // treatment as `Remove` above.
                    //
                    // `slot_mut` returning `None` (the slot was removed between
                    // send and drain) must retire the vocoder too — an early
                    // `return`/`continue` here would drop it on this thread.
                    let surplus = match self.slot_mut(id) {
                        Some(slot) => slot.set_stretch(stretch_factor, pitch_cents, stretch),
                        None => stretch,
                    };
                    if let Some(unit) = surplus {
                        self.retire(Retired::Stretch(unit));
                    }
                }
            }
            if reserved && !self.retired_now {
                // Reserved, but nothing came of it: the slot is free again.
                self.room.release(1);
            }
        }
    }
}

impl<F: SamplerFloat> Clone for VoicePool<F> {
    /// A copy of the voices and settings, with no command ring and no
    /// retirement ring of its own: a copy draining the live handle's
    /// commands would steal edits from the audio thread (a ring's one reader
    /// takes each message), so the `Receiver` stays with the
    /// pool that owns it.
    fn clone(&self) -> Self {
        Self {
            // Dead, like the command `Receiver` beside it: a clone must not hand
            // slots back to the live pool's control thread.
            retired: ring(0, Overflow::Refuse).0,
            retired_now: false,
            stranded: CappedVec::with_capacity(0),
            room: Arc::default(),
            counters: Arc::default(),
            voices: {
                // Reserved like a new pool's, so a clone's drain refuses at the
                // same count rather than at whatever `collect` sized.
                let mut voices = Vec::with_capacity(MAX_RESIDENT_VOICES.max(self.voices.len()));
                voices.extend(self.voices.iter().map(|s| {
                    PlaybackSlot {
                        id: s.id,
                        voice: s.voice.clone(),
                        stretch: s
                            .stretch
                            .as_ref()
                            .map(|v| stretched::vocoder::<F>(s.channels, v.spare.is_some())),
                        cursor: None,
                        spare_cursor: None,
                        seats: 0,
                        spare_seats: 0,
                        channels: s.channels,
                    }
                }));
                voices
            },
            rx: ring(0, Overflow::Refuse).1,
            sample_rate: self.sample_rate,
            render_mode: self.render_mode,
            clock: Clock::new(),
            tempo: self.tempo.clone(),
            loops: Arc::default(),
            channels: self.channels,
            scratch: BlockScratch::sized_like(&self.scratch),
            reach: self.reach,
        }
    }
}

impl<F: SamplerFloat> Default for VoicePool<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: SamplerFloat> Node<F> for VoicePool<F> {
    /// No inputs, [`channels`](Self::channels) outputs; a source (fed out of
    /// band, through its queue), never skipped, with the ring-out of
    /// `ring_out`.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.channels)
            .tail(self.ring_out())
            // Its voices arrive while it plays, each a `VoiceNode`'s cost:
            // measured 13 ns per frame and channel at one plain voice, 82 at
            // eight, and one stretched voice alone is 288.
            .cost(tutti_graph::Cost::Heavy)
    }

    /// The rate every slot and its source run at, and
    /// the handle builds vocoders at; the block scratch sized from
    /// `max_block` (a re-prepare at another size resizes it).
    fn prepare(&mut self, p: &Prepare) {
        let sample_rate = p.sample_rate();
        self.sample_rate = sample_rate;
        self.render_mode = p.render_mode();
        self.scratch
            .resize(p.max_block().get(), self.channels.count() as usize);
        for slot in &mut self.voices {
            slot.voice.source.set_render(sample_rate, self.render_mode);
        }
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        self.drain_commands();
        // Lent for the block and given back after it: `render_in_pieces`
        // borrows the whole pool.
        let mut reader = self.tempo.take();
        let map = reader.as_mut().map(TempoReader::lend);
        let clock = self.clock.observe_with(cx.env, map.as_deref());
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
        // Each slot renders its ONE voice into the pool's lanes and adds them
        // in, a lane at a time, via the shared `PlaybackSlot::render_into`
        // (the same per-variant `VoiceSource` match a standalone `VoiceNode`
        // uses), in one piece: in a graph, `prepare` sized the lanes for
        // every block.
        if frames <= self.scratch.frames() {
            for slot in &mut self.voices {
                slot.render_into(&clock, 0..frames, n, &mut self.scratch, &mut refs[..n]);
            }
        } else {
            self.render_in_pieces(&clock, frames, n, &mut refs[..n]);
        }
        drop(map);
        self.tempo = reader;
        Status::Modified
    }

    /// Every slot's buffered audio flushed, the transport forgotten.
    fn reset(&mut self) {
        for slot in &mut self.voices {
            slot.flush_playhead_state();
        }
        self.clock.reset();
    }
}

/// A pool's fork: the voices it was built with, copied, while no command has
/// reached it (see "As a graph node").
struct PoolFork<F: SamplerFloat> {
    channels: ChannelLayout,
    tempo: Option<TempoReader>,
    /// The voices the pool held when it became a node, by slot.
    voices: Vec<(SlotId, Voice<F>)>,
    /// Shared with the handle: whether a command has been queued since.
    counters: Arc<DrainCounters>,
}

/// Why a pool refused a fork: the voices it holds are not known here.
#[derive(Debug, thiserror::Error)]
#[error(
    "a voice pool forks only while its handle has queued nothing: its voices \
     arrive on the audio thread, where no control-side copy of them exists"
)]
struct PoolCommanded;

impl<F: SamplerFloat> ForkSource<F> for PoolFork<F> {
    fn fork(&self, mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        if self.counters.commanded() {
            return Err(ForkCause::new(PoolCommanded));
        }
        let mut pool = VoicePool::from_parts(self.channels);
        pool.tempo = self.tempo.clone();
        for (id, voice) in &self.voices {
            pool.insert_voice(*id, voice.fork_copy(mode)?);
        }
        pool.reach = Reach::Frozen;
        Ok(Forked::new(Box::new(pool)))
    }
}

impl<F: SamplerFloat> IntoNode<F> for VoicePool<F> {
    type Controls = VoicePoolHandle<F>;

    fn into_parts(self) -> NodeParts<VoicePoolHandle<F>, F> {
        let fork = PoolFork {
            channels: self.channels,
            tempo: self.tempo.clone(),
            voices: self
                .voices
                .iter()
                .map(|slot| (slot.id, slot.voice.as_ref().clone()))
                .collect(),
            counters: Arc::clone(&self.counters),
        };
        let (pool, handle) = self.with_handle();
        NodeParts::new(
            Box::new(pool),
            handle.clone(),
            Some(Box::new(fork)),
            Some(Box::new(handle)),
        )
    }
}
