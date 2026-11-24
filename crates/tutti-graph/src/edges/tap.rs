//! Taps: copy-out points of the compiled plan.
//!
//! A tap is declared on the [`Topology`](crate::Topology) as "copy these
//! sources" ([`TapSpec`](crate::TapSpec)); the compiler emits one
//! [`Op::Tap`](crate::Op::Tap) per tap, reading every lane's source, and, for an
//! aligned tap, the same delay an output channel gets for each. The outputs
//! form ([`TapSpec::Outputs`](crate::TapSpec::Outputs)) reads what each output
//! channel carries, after the alignment delays and the sum of a fan-in. A tap
//! that is not in the topology compiles to nothing. Opening a tap refuses a source that does not exist then
//! ([`MissingSource`](crate::MissingSource)); a lane whose port goes missing
//! later (a node replaced by a narrower one, a removed node) reads silence and
//! the commit succeeds, and the zeros count as neither data nor loss. The
//! reader is told only when **every** lane's node was removed: the tap is then
//! taken out of the plan (it compiles to no op, as a closed tap does) and,
//! once the plan without it is the one running, its [`FaultLatch`] is raised
//! ([`TapReader::source_gone`], [`Pull::Ended`] after the ring is drained).
//!
//! The ring each lane writes is `tutti_rt::ring`, the one single-producer,
//! single-consumer queue, as the editor-to-executor queue is. The producer is
//! owned by the executor (it rides a commit in, like a unit), the consumer by
//! the caller, so the audio thread takes no lock and frees nothing: a producer
//! left behind by a plan that no longer names it is retired with the old
//! state, on the control side.
//!
//! The lanes of a tap are written together by one op and read by whole
//! frames, so they cannot tear: frame `n` of every lane is the same frame,
//! whatever the reader's speed (see [`TapWriter`]).
//!
//! Dropping the [`TapReader`] first is safe. The tap then finds no reader and
//! writes nothing, and the rings are freed with the writer when the tap is
//! closed ([`Editor::close_tap`](crate::Editor::close_tap)) or pruned
//! ([`Editor::prune_taps`](crate::Editor::prune_taps)), on the control side.
//! Dropping the reader last frees the ring on the reader's thread.

use std::cell::Cell;
use std::sync::{Arc, Weak};

use crate::{TapAlign, TapId};
use tutti_buffers::ChannelLayout;
use tutti_buffers::{BufferError, Interleaved, InterleavedMut, PlanesMut, Pull, Source};
use tutti_rt::{Bell, ControlDrop, Counter, CounterWriter, FaultLatch, Listener, counter};
use tutti_rt::{Overflow, RingReader, RingWriter, ring};
use tutti_units::{Frame, Samples};

use super::alive::Alive;
use crate::compile::ids::AudioSlotId;
use crate::exec::slots::AudioSlots;
use tutti_buffers::Float;

/// How a tap is opened: its ring capacity and where its copy sits in time.
/// Taken by [`Editor::open_tap`](crate::Editor::open_tap).
#[derive(Debug)]
pub struct TapOptions {
    capacity: Samples,
    align: TapAlign,
}

impl TapOptions {
    /// Options for a tap whose every lane is a ring of `capacity` frames:
    /// aligned to the output ([`TapAlign::ToOutput`]).
    pub fn new(capacity: Samples) -> Self {
        Self {
            capacity,
            align: TapAlign::default(),
        }
    }

    /// Where the copy sits in time.
    #[must_use]
    pub fn align(mut self, align: TapAlign) -> Self {
        self.align = align;
        self
    }

    pub(crate) fn into_parts(self) -> (Samples, TapAlign) {
        (self.capacity, self.align)
    }
}

/// The executor's end of a tap: every lane's ring producer, written together.
///
/// A tap's lanes are rings of the same capacity that the executor fills with
/// the same number of frames each block and the reader drains by whole frames,
/// so frame `n` of every lane is the same frame by construction. One thread
/// writes and one reads, which is why the minimums below are safe: a ring's
/// vacancy only grows under the reader and its occupancy only grows under the
/// writer, so a count taken before a push can only be an underestimate.
pub struct TapWriter<F: Float = f32> {
    _control: ControlDrop<Self>,
    lanes: Vec<RingWriter<F>>,
    /// One [`Run`] for each block written, pushed before its samples.
    runs: RingWriter<Run>,
    /// The frame the next block of a writer that is not an op of a plan
    /// starts at: the frames offered so far, written or lost.
    next: Frame,
    /// Frames refused, counted per frame (the rings take whole blocks or none:
    /// they are `Refuse` and count nothing), shared with the reader.
    lost: CounterWriter<Samples>,
    /// Rung after each block written, for a reader that awaits frames.
    bell: Option<Bell>,
    /// The reader's end latch, raised by [`close`](Self::close).
    end: Arc<TapEnd>,
    /// Rings the bell when the writer is gone. Last, so it drops after the rings: the
    /// reader that wakes finds their writers gone.
    _gone: RingOnDrop,
}

/// Rings a bell as it drops.
struct RingOnDrop(Option<Bell>);

impl Drop for RingOnDrop {
    fn drop(&mut self) {
        if let Some(bell) = &self.0 {
            bell.ring();
        }
    }
}

/// How a tap ends: the latch the editor raises when every node the tap read was
/// removed (an event tap's too), and the bell it rings so a reader that awaits frames sees the end.
pub(crate) struct TapEnd {
    latch: FaultLatch,
    bell: Option<Bell>,
}

impl TapEnd {
    /// A tap end that rings `bell` when it is raised.
    pub(crate) fn new(bell: Option<Bell>) -> Self {
        Self {
            latch: FaultLatch::new(),
            bell,
        }
    }

    /// Raises the latch, then rings: the reader asks the latch before it reads the
    /// rings, so a frame written before the end is never missed.
    pub(crate) fn raise(&self) {
        self.latch.raise(());
        if let Some(bell) = &self.bell {
            bell.ring();
        }
    }

    pub(crate) fn is_raised(&self) -> bool {
        self.latch.is_raised()
    }
}

/// Whether a lane's reader is still held.
fn is_open<F: Float>(lane: &RingWriter<F>) -> bool {
    !lane.is_disconnected()
}

/// The graph frame a block's samples start at, and how many of them the rings
/// took (a block the rings had no room for whole is a gap between two runs,
/// never a run of its own). The reader's clock ([`TapReader::next_frame`]).
#[derive(Clone, Copy, Debug)]
struct Run {
    start: Frame,
    frames: usize,
}

/// A reader's place in the writer's [`Run`]s: the runs not yet read through,
/// the frame the next one handed out is, and how much of the current run is
/// left. `left` is zero when no run is under way, and every sample read has
/// its run (a run is written before its samples).
struct RunCursor {
    /// The writer's runs not yet read through.
    runs: RingReader<Run>,
    /// The graph frame of the next frame to hand out, while `left` is not
    /// zero; once a run is read through, the frame after it.
    at: Frame,
    /// Frames of the current run not yet handed out.
    left: usize,
}

impl RunCursor {
    fn new(runs: RingReader<Run>) -> Self {
        Self {
            runs,
            at: Frame::ZERO,
            left: 0,
        }
    }

    /// The graph frame of the next frame to hand out: the next run's start
    /// when none is under way.
    fn next_frame(&self) -> Frame {
        if self.left > 0 {
            return self.at;
        }
        self.runs.peek().map_or(self.at, |run| run.start)
    }

    /// Frames in the runs before the first gap, counting the one under way.
    fn contiguous(&self) -> Samples {
        let mut end = self.at + Samples::new(self.left);
        let mut run = self.left;
        for next in self.runs.iter() {
            if run > 0 && next.start != end {
                break;
            }
            run += next.frames;
            end = next.start + Samples::new(next.frames);
        }
        Samples::new(run)
    }

    /// Advances over `frames` just handed out, across run boundaries.
    fn consume(&mut self, mut frames: usize) {
        while frames > 0 {
            if self.left == 0 {
                let Some(run) = self.runs.pop() else {
                    debug_assert!(false, "a sample was read with no run to place it");
                    return;
                };
                self.at = run.start;
                self.left = run.frames;
            }
            let step = frames.min(self.left);
            self.at += Samples::new(step);
            self.left -= step;
            frames -= step;
        }
    }
}

/// The most blocks a ring holds the start frame of at once, however deep its
/// lanes: a block that finds this many waiting is refused whole and counted
/// lost, like one the lanes have no room for. A ring of fewer frames than this
/// cannot hold more runs than frames.
const MAX_RUNS: usize = 4096;

impl<F: Float> std::fmt::Debug for TapWriter<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapWriter").finish_non_exhaustive()
    }
}

impl<F: Float> TapWriter<F> {
    /// Ends the tap for good and drops the writer. The reader's end latch is
    /// raised before the writer's rings go, so the reader reads
    /// [`source_gone`](TapReader::source_gone) and, once it has drained what
    /// was written, answers [`Pull::Ended`], as
    /// when the editor removes the tap's sources. A plain drop does not raise
    /// it: that reader reads [`writer_gone`](TapReader::writer_gone) and keeps
    /// answering `Starved`. Control side: dropping a writer is never audio-thread work.
    pub fn close(self) {
        self.end.raise();
    }

    /// How many lanes (channels) this writer fills.
    #[must_use]
    pub fn width(&self) -> usize {
        self.lanes.len()
    }

    /// Writes a block of interleaved frames to the lanes together: every
    /// lane takes the same count, the fewest frames any ring has room for, and
    /// the rest is counted lost, once. A block of another width than the
    /// writer's is written to no lane and counted lost whole: the audio thread
    /// never errors, so the misuse shows in the reader's
    /// [`take_lost`](TapReader::take_lost). With no reader
    /// there is nothing to write or count. Audio thread: allocates nothing,
    /// locks nothing.
    ///
    /// The block's frame is the count of frames offered before it: a writer
    /// that is not an op of a plan numbers its frames from zero.
    #[inline]
    pub fn push_interleaved(&mut self, block: Interleaved<'_, F>) {
        if self.lanes.is_empty() || !self.lanes.first().is_some_and(is_open) {
            return;
        }
        let frames = block.frames().get();
        let start = self.next;
        self.next = start + Samples::new(frames);
        let take = if block.layout() == self.layout() {
            self.room(frames).min(frames)
        } else {
            0
        };
        if take > 0 {
            self.note(start, take);
        }
        for (channel, lane) in self.lanes.iter_mut().enumerate() {
            let pushed = lane.push_iter(block.iter().take(take).map(|frame| frame[channel]));
            // A reader dropped since the check above refuses its pushes.
            debug_assert!(
                pushed == take || lane.is_disconnected(),
                "room was counted across every lane"
            );
        }
        self.count_lost(frames - take);
        if take > 0 {
            self.ring();
        }
    }

    /// Rings the reader's bell, if it has one.
    #[inline]
    fn ring(&self) {
        if let Some(bell) = &self.bell {
            bell.ring();
        }
    }

    /// The channel layout of a block this writer takes: `width` channels.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        ChannelLayout::from_count(self.lanes.len().try_into().unwrap_or(u16::MAX))
    }

    /// Counts `frames` as lost without writing them: a block this writer
    /// could not take (a device of another width).
    #[inline]
    pub fn skip(&mut self, frames: usize) {
        if self.lanes.first().is_some_and(is_open) {
            self.next += Samples::new(frames);
            self.count_lost(frames);
        }
    }

    /// Frames every lane has room for, and zero when the run ring is full.
    #[inline]
    fn room(&self, frames: usize) -> usize {
        if self.runs.is_full() || frames == 0 {
            return 0;
        }
        self.lanes.iter().map(RingWriter::vacant).min().unwrap_or(0)
    }

    /// Records that `frames` frames starting at `start` are about to be
    /// pushed. Before the samples, so a reader that sees a sample sees its run.
    #[inline]
    fn note(&mut self, start: Frame, frames: usize) {
        let pushed = self.runs.push(Run { start, frames });
        debug_assert!(
            pushed.is_ok() || self.runs.is_disconnected(),
            "room() counted the run ring"
        );
    }

    #[inline]
    fn count_lost(&mut self, frames: usize) {
        if frames > 0 {
            self.lost.add(Samples::new(frames));
        }
    }
}

/// [`Op::Tap`](crate::Op::Tap): the first `frames` samples of each of `srcs`
/// into the tap's lanes, the block starting at graph frame `start`. Every lane
/// takes the same count, the fewest frames any ring has room for, and the rest
/// is counted lost, once for the tap. With no reader there is nothing to write
/// or count.
#[inline]
pub(crate) fn tap_op<F: Float, A: AudioSlots<F>>(
    tap: &mut TapWriter<F>,
    arena: &A,
    srcs: &[AudioSlotId],
    start: Frame,
    frames: usize,
) {
    if !tap.lanes.first().is_some_and(is_open) {
        return;
    }
    let take = tap.room(frames).min(frames);
    if take > 0 {
        tap.note(start, take);
    }
    for (lane, &src) in tap.lanes.iter_mut().zip(srcs) {
        let pushed = lane.push_slice(&arena.slot(src, frames)[..take]);
        // A reader dropped since the check above refuses its pushes.
        debug_assert!(
            pushed == take || lane.is_disconnected(),
            "room was counted across every lane"
        );
    }
    if take < frames {
        tap.lost.add(Samples::new(frames - take));
    }
    if take > 0 {
        tap.ring();
    }
}

/// The caller's end of a tap: every lane, read together.
///
/// Drain it from a thread of your own. Dropping it leaves the tap open until
/// the editor closes or prunes it; the audio thread then writes nothing for it.
/// It must not be dropped on the audio thread (debug builds panic there).
pub struct TapReader<F: Float = f32> {
    _control: ControlDrop<Self>,
    /// The tap's id in a topology; `None` for a reader outside any.
    id: Option<TapId>,
    lanes: Vec<RingReader<F>>,
    lost: Counter<Samples>,
    /// What `lost` read at the last [`take_lost`](Self::take_lost).
    lost_seen: Cell<Samples>,
    /// Where the reader is in the writer's runs.
    cursor: RunCursor,
    /// The editor watches this: when it dies, the reader is gone.
    alive: Alive,
    /// The wait side of the bell the writer rings; `None` for a tap opened
    /// without one.
    listener: Option<Listener>,
    /// Raised when the tap was taken out of the plan for good.
    end: Arc<TapEnd>,
}

impl<F: Float> std::fmt::Debug for TapReader<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapReader")
            .field("id", &self.id)
            .field("width", &self.lanes.len())
            .finish_non_exhaustive()
    }
}

impl<F: Float> TapReader<F> {
    /// A reader outside any topology and the writer that fills it: `width`
    /// lanes, each holding `capacity` frames (at least one). A plan's tap gets
    /// its reader from the editor, which names it; a writer that is not an op
    /// of a plan (the engine's, after the fader; an inlet's producer) takes
    /// one here, and its reader has no [`id`](Self::id).
    #[must_use]
    pub fn pair(width: usize, capacity: Samples) -> (Self, TapWriter<F>) {
        Self::build(None, width, capacity, None)
    }

    /// As [`pair`](Self::pair), with the writer ringing `bell` and the reader
    /// awaiting it ([`notified`](Self::notified)): the bell and its listener
    /// come from one [`Bells`](tutti_rt::Bells) set. Hidden: the engine's heard
    /// tap is its one caller, and no app names a bell, so the umbrella exports
    /// neither half of one.
    #[doc(hidden)]
    #[must_use]
    pub fn pair_notified(
        width: usize,
        capacity: Samples,
        bell: (Bell, Listener),
    ) -> (Self, TapWriter<F>) {
        Self::build(None, width, capacity, Some(bell))
    }

    /// The reader of the tap `id` names, and its writer.
    pub(crate) fn named(
        id: TapId,
        width: usize,
        capacity: Samples,
        bell: Option<(Bell, Listener)>,
    ) -> (Self, TapWriter<F>) {
        Self::build(Some(id), width, capacity, bell)
    }

    fn build(
        id: Option<TapId>,
        width: usize,
        capacity: Samples,
        bell: Option<(Bell, Listener)>,
    ) -> (Self, TapWriter<F>) {
        let (bell, listener) = match bell {
            Some((bell, listener)) => (Some(bell), Some(listener)),
            None => (None, None),
        };
        let (lost, lost_reader) = counter::<Samples>();
        let end = Arc::new(TapEnd::new(bell.clone()));
        let (run_prod, run_cons) = ring::<Run>(capacity.get().clamp(1, MAX_RUNS), Overflow::Refuse);
        let (cons, prods): (Vec<_>, Vec<_>) = (0..width)
            .map(|_| ring::<F>(capacity.get().max(1), Overflow::Refuse))
            .map(|(prod, cons)| (cons, prod))
            .unzip();
        let reader = Self {
            _control: ControlDrop::new(),
            id,
            lanes: cons,
            lost: lost_reader,
            lost_seen: Cell::new(Samples::ZERO),
            cursor: RunCursor::new(run_cons),
            alive: Alive::default(),
            listener,
            end: Arc::clone(&end),
        };
        let writer = TapWriter {
            _control: ControlDrop::new(),
            lanes: prods,
            runs: run_prod,
            next: Frame::ZERO,
            lost,
            _gone: RingOnDrop(bell.clone()),
            bell,
            end,
        };
        (reader, writer)
    }

    /// A future that resolves once the tap has written a block or ended since
    /// the last time one resolved: await it, then drain the reader. Two
    /// writes before it resolves it once, so drain until the reader is
    /// [`Starved`](Pull::Starved) after it.
    ///
    /// `None` for a tap opened without a bell (a graph used without a
    /// session, such as an offline render's, or [`pair`](Self::pair)'s): it
    /// has nothing to await, so poll it.
    pub fn notified(&mut self) -> Option<impl Future<Output = ()> + '_> {
        self.listener.as_mut().map(Listener::rung)
    }

    /// A handle to see whether this reader is still alive: its strong count
    /// is zero once the reader is dropped.
    #[must_use]
    pub fn watch(&self) -> Weak<()> {
        self.alive.watch()
    }

    /// The latch the editor raises when the tap leaves the plan because every
    /// node it read was removed.
    pub(crate) fn end_latch(&self) -> Arc<TapEnd> {
        Arc::clone(&self.end)
    }

    /// Whether the tap was taken out of the plan because every node it read
    /// was removed, and no frame will be written after what is in its rings.
    ///
    /// **Ask before reading the rings** ([`FaultLatch`]'s order): a `true`
    /// answer, then a read that finds nothing, is the end; read first and
    /// ask after, and a frame written between the two is lost. A tap with a
    /// source left, or one whose source was only replaced by a narrower node,
    /// never ends: its lanes read silence ([`Editor::open_tap`](crate::Editor::open_tap)).
    #[must_use]
    pub fn source_gone(&self) -> bool {
        self.end.is_raised()
    }

    /// Whether the writer is gone (its tap closed, or its graph dropped) and no frame
    /// will be written after what is in the rings. Unlike
    /// [`source_gone`](Self::source_gone), which the editor raises, a reader that awaits
    /// a bell asks this too: the writer's drop rings the bell it was opened with.
    ///
    /// **Ask before reading the rings**, as for `source_gone`.
    #[must_use]
    pub fn writer_gone(&self) -> bool {
        self.cursor.runs.is_disconnected()
    }

    /// The tap's id, the one [`Editor::close_tap`](crate::Editor::close_tap)
    /// takes; `None` for a reader [`pair`](Self::pair) made outside any topology.
    #[must_use]
    pub fn id(&self) -> Option<TapId> {
        self.id
    }

    /// How many lanes (channels) this tap carries.
    #[must_use]
    pub fn width(&self) -> usize {
        self.lanes.len()
    }

    /// The channel layout of an interleaved read: `width` channels.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        ChannelLayout::from_count(self.lanes.len().try_into().unwrap_or(u16::MAX))
    }

    /// Frames readable now: the fewest across lanes. The writer fills every
    /// lane by the same count, so a lane may briefly hold more than the
    /// others mid-block, and a read never goes past the shortest.
    #[must_use]
    pub fn available(&self) -> Samples {
        Samples::new(self.lanes.iter().map(RingReader::len).min().unwrap_or(0))
    }

    /// Pops frames into `out`, as many as fit; returns the frame count. The
    /// rest of `out` is as it was.
    ///
    /// # Errors
    ///
    /// [`BufferError::WidthMismatch`] (`src` the tap's width, `dst` the view's)
    /// if `out` is not as wide as the tap. Nothing is popped and `out` is left
    /// untouched.
    pub fn pop_into(&mut self, mut out: InterleavedMut<'_, F>) -> Result<Samples, BufferError> {
        self.check_width(out.layout())?;
        let frames = self.available().min(out.frames()).get();
        for (channel, lane) in self.lanes.iter_mut().enumerate() {
            let mut dst = out.iter_mut();
            lane.pop_each(frames, |sample| {
                if let Some(frame) = dst.next() {
                    frame[channel] = sample;
                }
            });
        }
        self.cursor.consume(frames);
        Ok(Samples::new(frames))
    }

    /// Refuses a view of another width than the tap's.
    fn check_width(&self, view: ChannelLayout) -> Result<(), BufferError> {
        let tap = self.layout();
        if view == tap {
            Ok(())
        } else {
            Err(BufferError::WidthMismatch {
                src: tap,
                dst: view,
            })
        }
    }

    /// Pops up to `planes.frames()` frames into `planes`, one plane per lane
    /// in lane order, and returns the frame count: each plane's first frames
    /// are written and the rest of it is as it was.
    ///
    /// # Errors
    ///
    /// [`BufferError::WidthMismatch`] (`src` the tap's width, `dst` the view's)
    /// if `planes` is not as wide as the tap. Nothing is popped and `planes`
    /// is left untouched.
    ///
    /// The shape of a graph node's outputs: no interleaved scratch to
    /// allocate, and the lanes copy straight out of their rings.
    pub fn pop_planes(&mut self, mut planes: PlanesMut<'_, '_, F>) -> Result<Samples, BufferError> {
        self.check_width(planes.layout())?;
        let frames = self.available().min(planes.frames()).get();
        for (lane, mut plane) in self.lanes.iter_mut().zip(planes.iter_mut()) {
            let popped = lane.pop_slice(&mut plane.samples_mut()[..frames]);
            debug_assert_eq!(popped, frames, "available counted every lane");
        }
        self.cursor.consume(frames);
        Ok(Samples::new(frames))
    }

    /// The graph frame of the next frame this reader hands out: the clock a
    /// consumer places what it reads on.
    ///
    /// The executor stamps each block it writes with the graph frame it starts
    /// at ([`Env::frame`](crate::Env)), the reader keeps the count, and a block
    /// the rings had no room for (counted by [`take_lost`](Self::take_lost))
    /// advances it too: once a later block has arrived, this is the frame of
    /// *that* block's first frame, so a gap shows as a jump between the frame a
    /// read ended on and this one. Before the next block arrives the reader
    /// cannot know there was a gap, and this is the frame after the last one
    /// read. A tap that is [aligned](crate::TapAlign::ToOutput) is on the
    /// output's clock: its frame `F` is what the outputs carry at `F`.
    ///
    /// A writer that is not an op of a plan ([`pair`](Self::pair): an inlet's
    /// producer, the engine's heard tap) has no graph clock; it numbers the
    /// frames it was offered from zero, written or lost.
    #[must_use]
    pub fn next_frame(&self) -> Frame {
        self.cursor.next_frame()
    }

    /// Frames readable now before the first gap (a block the rings refused):
    /// what a consumer that places every frame reads at once, so a gap is
    /// never inside a read. At most [`available`](Self::available).
    #[must_use]
    pub fn contiguous(&self) -> Samples {
        self.cursor.contiguous().min(self.available())
    }

    /// Frames the executor could not push since the last call, and clears the
    /// count: nonzero is a gap in what was read.
    pub fn take_lost(&self) -> Samples {
        let mut seen = self.lost_seen.get();
        let lost = self.lost.take_since(&mut seen);
        self.lost_seen.set(seen);
        lost
    }
}

/// A tap's reader is a [`Source`], so a recording pump (`tutti-session`'s
/// `PumpLoop`) takes it as it is: record what the graph is producing.
///
/// Drain it from a thread of your own, never from the audio thread. It is as
/// wide as the tap: a stereo output tap is stereo, a six-lane tap six
/// channels, so a sink must be built to match.
///
/// **A tap that falls behind loses frames, and a recording closes up over the
/// gap.** The rings are bounded (the capacity named at the open); what the
/// consumer does not drain in time is dropped and counted
/// ([`take_lost`](Source::take_lost)), and the stream has no marker where
/// they were.
///
/// Like every [`Source`] it carries no sample rate: a tap runs at the graph's,
/// so build a sink at that rate or the file plays at the wrong speed.
impl<F: Float> Source for TapReader<F> {
    type Sample = F;

    /// As wide as the tap: one channel a lane.
    fn layout(&self) -> ChannelLayout {
        Self::layout(self)
    }

    /// Pops up to `out.len()` frames into `out` ([`pop_into`](TapReader::pop_into)).
    ///
    /// **The graph does not end**, so an empty tap is [`Starved`](Pull::Starved):
    /// it means the graph has not rendered since the last pull (the audio
    /// thread was between blocks, or the tap was only just opened: it flows
    /// from the commit that carries it). A consumer that read it as
    /// end-of-stream would stop within a block of starting. The tap's own
    /// sources can end: once every node it read was removed and the frames
    /// before the removal are read, it answers [`Ended`](Pull::Ended)
    /// ([`source_gone`](TapReader::source_gone)).
    fn pull(&mut self, out: InterleavedMut<'_, F>) -> Pull {
        if out.layout() != Self::layout(self) {
            return Pull::Failed;
        }
        // The latch first, then the ring: `FaultLatch::drain`'s order.
        let gone = self.source_gone();
        let Ok(frames) = self.pop_into(out) else {
            return Pull::Failed;
        };
        if !frames.is_zero() {
            Pull::Frames(frames)
        } else if gone {
            Pull::Ended
        } else {
            Pull::Starved
        }
    }

    /// The frames the executor could not push since the last call
    /// ([`TapReader::take_lost`]).
    fn take_lost(&mut self) -> Samples {
        Self::take_lost(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::arena::Arena;

    /// **A closed writer ends its reader**: the reader drains what was
    /// written, then answers `Ended`; a dropped one answers `Starved`.
    #[test]
    fn a_closed_writer_ends_its_reader_after_the_drain() {
        use tutti_buffers::{Pull, Source};
        let mut out = [0.0f32; 4];
        let (mut reader, mut writer) = TapReader::<f32>::pair(1, Samples::new(8));
        writer.push_interleaved(Interleaved::new(&[1.0, 2.0], writer.layout()).unwrap());
        writer.close();
        assert!(reader.source_gone());
        let layout = reader.layout();
        assert!(matches!(
            Source::pull(&mut reader, InterleavedMut::new(&mut out, layout).unwrap()),
            Pull::Frames(n) if n.get() == 2
        ));
        assert!(matches!(
            Source::pull(&mut reader, InterleavedMut::new(&mut out, layout).unwrap()),
            Pull::Ended
        ));
        let (mut reader, writer) = TapReader::<f32>::pair(1, Samples::new(8));
        drop(writer);
        assert!(matches!(
            Source::pull(&mut reader, InterleavedMut::new(&mut out, layout).unwrap()),
            Pull::Starved
        ));
    }

    /// **A dropped writer rings the bell after its rings are gone**, so the reader that
    /// wakes reads the writer gone with every frame written before it still there.
    #[test]
    fn a_dropped_writer_rings_and_the_reader_reads_it_gone() {
        use std::future::Future;
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};

        let executor = tutti_platform::Executor::new();

        let bells = tutti_rt::Bells::new(&executor).expect("the bells' tick task");
        let bell = bells.bell();
        let (mut reader, mut writer) =
            TapReader::<f32>::named(TapId::of(2), 1, Samples::new(8), Some(bell));
        writer.push_interleaved(Interleaved::new(&[1.0, 2.0], writer.layout()).unwrap());
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(
            pin!(reader.notified().expect("a tap opened with a bell")).poll(&mut cx),
            Poll::Ready(())
        );
        assert!(!reader.writer_gone());
        drop(writer);
        assert_eq!(
            pin!(reader.notified().expect("a tap opened with a bell")).poll(&mut cx),
            Poll::Ready(()),
            "the drop rings"
        );
        assert!(reader.writer_gone(), "the ring follows the writer's end");
        let mut out = [0.0f32; 2];
        let popped = reader
            .pop_into(InterleavedMut::new(&mut out, reader.layout()).unwrap())
            .unwrap();
        assert_eq!(
            (popped.get(), out),
            (2, [1.0, 2.0]),
            "what was written is still read"
        );
    }

    /// **A tap rings its bell as it writes a block and when it ends, and not
    /// otherwise.** A reader that awaits frames is woken by the audio thread's
    /// ring; a block the rings had no room for wrote nothing and rings nothing.
    #[test]
    fn a_tap_rings_its_bell_after_a_block_and_at_its_end() {
        use std::future::Future;
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};

        let executor = tutti_platform::Executor::new();

        let bells = tutti_rt::Bells::new(&executor).expect("the bells' tick task");
        let bell = bells.bell();
        let (mut reader, mut writer) =
            TapReader::<f32>::named(TapId::of(1), 1, Samples::new(4), Some(bell));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(
            pin!(reader.notified().expect("a tap opened with a bell")).poll(&mut cx),
            Poll::Pending,
            "nothing written"
        );

        writer.push_interleaved(Interleaved::new(&[1.0, 2.0], writer.layout()).unwrap());
        assert_eq!(
            pin!(reader.notified().expect("a tap opened with a bell")).poll(&mut cx),
            Poll::Ready(()),
            "a block rings"
        );

        // The ring holds four frames: a block of eight finds room for none of it once
        // it is full.
        writer.push_interleaved(Interleaved::new(&[0.0; 4], writer.layout()).unwrap());
        assert_eq!(
            pin!(reader.notified().expect("a tap opened with a bell")).poll(&mut cx),
            Poll::Ready(())
        );
        writer.push_interleaved(Interleaved::new(&[0.0; 4], writer.layout()).unwrap());
        assert_eq!(
            pin!(reader.notified().expect("a tap opened with a bell")).poll(&mut cx),
            Poll::Pending,
            "a block with no room wrote nothing and rings nothing"
        );

        reader.end_latch().raise();
        assert_eq!(
            pin!(reader.notified().expect("a tap opened with a bell")).poll(&mut cx),
            Poll::Ready(()),
            "the end rings"
        );
        assert!(reader.source_gone());
    }

    /// **A tap opened without a bell has nothing to await**: `notified` is `None`,
    /// never a future that hangs or resolves at once.
    #[test]
    fn a_tap_opened_without_a_bell_has_no_notified_future() {
        let (mut reader, _writer) = TapReader::<f32>::pair(1, Samples::new(4));
        assert!(reader.notified().is_none());
    }

    /// An `f64` block reaches an `f64` tap sample for sample, with the
    /// digits `f32` cannot hold.
    #[test]
    fn an_f64_block_reaches_the_f64_tap_unnarrowed() {
        const FRAMES: usize = 600;
        let mut arena = Arena::<f64>::new(1, FRAMES);
        // Not representable in `f32`: a narrowing would be visible.
        let block: Vec<f64> = (0..FRAMES).map(|i| 0.1 * (i + 1) as f64).collect();
        arena
            .slot_mut(AudioSlotId::ZERO, FRAMES)
            .copy_from_slice(&block);
        let (mut reader, mut writer) = TapReader::<f64>::pair(1, Samples::new(1024));
        tap_op::<f64, _>(
            &mut writer,
            &arena,
            &[AudioSlotId::ZERO],
            Frame::ZERO,
            FRAMES,
        );
        let mut out = vec![0.0f64; FRAMES];
        assert_eq!(
            reader
                .pop_into(InterleavedMut::new(&mut out, reader.layout()).unwrap())
                .unwrap(),
            Samples::new(FRAMES)
        );
        assert_eq!(out, block);
        assert_ne!(f64::from(block[0] as f32), block[0]);
    }

    /// A stereo pop writes frame `i` as `[left_i, right_i]` and stops at the
    /// view's frames: a three-frame view takes three of four, in lane order,
    /// and the fourth stays queued.
    #[test]
    fn a_stereo_pop_interleaves_in_lane_order_and_stops_at_the_view() {
        let (mut reader, mut writer) = TapReader::<f32>::pair(2, Samples::new(16));
        let block = [1.0, -1.0, 2.0, -2.0, 3.0, -3.0, 4.0, -4.0];
        writer.push_interleaved(Interleaved::new(&block, writer.layout()).unwrap());
        let mut out = [9.0f32; 6];
        let view = InterleavedMut::new(&mut out, reader.layout()).unwrap();
        assert_eq!(reader.pop_into(view).unwrap(), Samples::new(3));
        assert_eq!(out, [1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
        assert_eq!(
            reader.available(),
            Samples::new(1),
            "the fourth frame stays"
        );
    }

    /// A pop into a view of another width is refused with both widths, leaves
    /// the view alone and consumes nothing: the next pop of the right width
    /// still gets every frame.
    #[test]
    fn a_pop_into_another_width_is_refused_and_consumes_nothing() {
        let (mut reader, mut writer) = TapReader::<f32>::pair(2, Samples::new(16));
        let block = [1.0, -1.0, 2.0, -2.0, 3.0, -3.0];
        writer.push_interleaved(Interleaved::new(&block, writer.layout()).unwrap());
        let three = ChannelLayout::from(3u16);
        let mut wide = [9.0f32; 9];
        let view = InterleavedMut::new(&mut wide, three).unwrap();
        assert_eq!(
            reader.pop_into(view),
            Err(BufferError::WidthMismatch {
                src: ChannelLayout::STEREO,
                dst: three
            })
        );
        assert_eq!(wide, [9.0; 9], "the view is untouched");
        let mut out = [0.0f32; 6];
        let view = InterleavedMut::new(&mut out, ChannelLayout::STEREO).unwrap();
        assert_eq!(reader.pop_into(view), Ok(Samples::new(3)));
        assert_eq!(out, block);
    }

    /// The same for planes: both widths named, nothing consumed.
    #[test]
    fn a_pop_into_planes_of_another_width_is_refused_and_consumes_nothing() {
        let (mut reader, mut writer) = TapReader::<f32>::pair(2, Samples::new(16));
        let block = [1.0, -1.0, 2.0, -2.0, 3.0, -3.0];
        writer.push_interleaved(Interleaved::new(&block, writer.layout()).unwrap());
        let mut one = [vec![9.0f32; 3]];
        assert_eq!(
            reader.pop_planes(PlanesMut::from_vecs(&mut one).unwrap()),
            Err(BufferError::WidthMismatch {
                src: ChannelLayout::STEREO,
                dst: ChannelLayout::MONO
            })
        );
        assert_eq!(one, [[9.0; 3]], "the view is untouched");
        let mut planes = [vec![0.0f32; 3], vec![0.0f32; 3]];
        let got = reader.pop_planes(PlanesMut::from_vecs(&mut planes).unwrap());
        assert_eq!(got, Ok(Samples::new(3)));
        assert_eq!(planes, [[1.0, 2.0, 3.0], [-1.0, -2.0, -3.0]]);
    }

    /// A block of another width than the writer's is written to no lane and
    /// counted lost whole.
    #[test]
    fn a_block_of_another_width_is_counted_lost() {
        let (reader, mut writer) = TapReader::<f32>::pair(2, Samples::new(16));
        let block = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mono_wide = Interleaved::new(&block, ChannelLayout::from(3u16)).unwrap();
        writer.push_interleaved(mono_wide);
        assert_eq!(reader.available(), Samples::new(0));
        assert_eq!(reader.take_lost(), Samples::new(2));
    }
}
