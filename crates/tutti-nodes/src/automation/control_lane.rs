//! [`ControlLaneNode`]: one target's [`ControlProgram`], evaluated on the
//! play-clock grid and sent to the target as curve segments.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use atomic_float::AtomicF64;
use tutti_buffers::Float;
use tutti_curve::Piece;
use tutti_graph::{
    Cost, Cx, ForkCause, ForkSource, Forked, InEvents, IntoNode, Io, Law, Node, NodeParts, Offset,
    OutEvents, ParamEvent, ParamKind, ParamRange, ParamRangeCell, ParamRangeReader, ParamSegment,
    PlayReading, Prepare, RenderMode, Shape, Status, TransportPosition,
};
use tutti_modulation::{
    CONTROL_GRID, ControlProgram, CurveSegment, DepthSnapshot, LayerClock, MAX_LANE_INPUTS,
    ProgramTime, SourceId, Waveform,
};
use tutti_rt::ParamCell;
use tutti_rt::{Publisher, Reader};
use tutti_units::{
    Frame, Normalized, ParamAddr, ParamId, SamplePosition, SampleSpan, Samples, Tail, UnitAddr,
    UnitParam,
};

use crate::SOURCE_EVENT_CAPACITY;

/// How a lane's value reaches one kind of parameter.
///
/// The kinds are [`PortLane`] and [`ForeignLane`]; the trait is sealed, so
/// nothing else implements it. A [`ControlLaneNode`] is built for one, from a
/// [`UnitLaneConfig`] or a [`ForeignLaneConfig`]. Its items are what the
/// lane's walk asks of its kind.
pub trait LaneKind: sealed::Sealed + Send + Sync + 'static {
    /// How the kind names its parameter.
    type Addr: Copy + Send + Sync + 'static;
    /// The target's range as the control side holds it: what a fork
    /// freezes.
    type Source: Clone + Send + Sync + 'static;
    /// The target's range as the node reads it, lent once per block.
    type Range: Send + 'static;
    /// Whether a program that is automation alone is sent as the curve's
    /// own shaped pieces (exact mode).
    const SHAPED: bool;

    /// The built-in param behind `addr`, which says whether its values
    /// run on a circle ([`UnitParam::is_circular`]): the one place a lane
    /// asks. `None` for a parameter of no tutti unit.
    fn unit(addr: Self::Addr) -> Option<UnitParam>;

    /// A segment for `addr` from `from` to `to` along `law`. The values
    /// are the parameter's own raw numbers (a param's raw unit values, or
    /// a foreign parameter's own normalisation): a lane is built for an
    /// address, not for a unit type, so they are bare `f64` from here to
    /// the segment, as [`ParamSegment::unit`] and [`ParamSegment::by_id`]
    /// take them.
    fn segment(addr: Self::Addr, from: f64, to: f64, law: Law) -> ParamSegment;

    /// A reader of `source`, registered here. Control side: allocates.
    fn reader(source: &Self::Source) -> Self::Range;

    /// The range now, in a cell of its own. Control side: it locks.
    fn frozen(source: &Self::Source) -> Self::Source;

    /// The range this block.
    fn load(range: &mut Self::Range) -> ParamRange;

    /// `value` as the lane sends it under `range`.
    fn clamp(range: ParamRange, value: f64) -> f64;

    /// Where along a piece from `from` to `to` the lane ends it early, as
    /// a fraction of the piece: the first bound of `range` it crosses.
    fn cut(range: ParamRange, from: f64, to: f64) -> Option<f64>;
}

/// A lane into a param port: a node's declared param, a node's mix, or an
/// edge's gain. It reads the port's own knob and range cells, the port clamps
/// what it sends, and a program that is automation alone is sent as the
/// curve's own shaped pieces.
#[derive(Clone, Copy, Debug)]
pub struct PortLane;

/// A lane into a parameter addressed by id (a hosted plugin's). It owns its
/// knob and its range, clamps its own pieces to that range, and sends points
/// only.
#[derive(Clone, Copy, Debug)]
pub struct ForeignLane;

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::PortLane {}
    impl Sealed for super::ForeignLane {}
}

impl LaneKind for PortLane {
    type Addr = UnitAddr;
    /// The port's own cell (`Editor::param_range_cell`); `None` for an
    /// unranged target, which takes no modulation.
    type Source = Option<ParamRangeCell>;
    type Range = Option<ParamRangeReader>;
    const SHAPED: bool = true;

    fn unit(addr: UnitAddr) -> Option<UnitParam> {
        Some(addr.param)
    }

    fn segment(addr: UnitAddr, from: f64, to: f64, law: Law) -> ParamSegment {
        ParamSegment::unit(addr, from, to, Samples::new(0)).law(law)
    }

    fn reader(source: &Self::Source) -> Self::Range {
        source.as_ref().map(ParamRangeCell::reader)
    }

    fn frozen(source: &Self::Source) -> Self::Source {
        source.as_ref().map(ParamRangeCell::detached)
    }

    #[inline]
    fn load(range: &mut Self::Range) -> ParamRange {
        range
            .as_mut()
            .map_or(ParamRange::UNBOUNDED, ParamRangeReader::load)
    }

    /// The port clamps for itself, per frame.
    #[inline]
    fn clamp(_: ParamRange, value: f64) -> f64 {
        value
    }

    #[inline]
    fn cut(_: ParamRange, _: f64, _: f64) -> Option<f64> {
        None
    }
}

impl LaneKind for ForeignLane {
    type Addr = ParamId;
    /// The lane's own cell, which its `ForeignLaneControls` publish to.
    type Source = Arc<Publisher<ParamRange>>;
    type Range = Reader<ParamRange>;
    const SHAPED: bool = false;

    fn unit(_: ParamId) -> Option<UnitParam> {
        None
    }

    fn segment(id: ParamId, from: f64, to: f64, law: Law) -> ParamSegment {
        ParamSegment::by_id(id, from, to, Samples::new(0)).law(law)
    }

    fn reader(source: &Self::Source) -> Self::Range {
        source.reader()
    }

    fn frozen(source: &Self::Source) -> Self::Source {
        Arc::new(Publisher::new(*source.current()))
    }

    #[inline]
    fn load(range: &mut Self::Range) -> ParamRange {
        *range.lend()
    }

    #[inline]
    fn clamp(range: ParamRange, value: f64) -> f64 {
        let (lo, hi) = (range.min(), range.max());
        if lo.is_nan() || hi.is_nan() || lo > hi {
            value
        } else {
            value.clamp(lo, hi)
        }
    }

    #[inline]
    fn cut(range: ParamRange, from: f64, to: f64) -> Option<f64> {
        [range.min(), range.max()]
            .into_iter()
            .filter(|x| x.is_finite())
            .filter(|&x| (from - x) * (to - x) < 0.0)
            .map(|x| (x - from) / (to - from))
            .min_by(f64::total_cmp)
    }
}

/// What a lane shares with its [`LaneControls`] and its fork source.
struct Shared {
    /// What the lane reads once per block, through the reader the node
    /// registered on it when it was built. A loan's sequence number
    /// (`Loan::seq`) tells a republish (a ramp) from a block that reads the
    /// same program. The fork and `program()` read `Publisher::current` (a
    /// `Loan` must not leave the block).
    program: Publisher<ControlProgram>,
    /// Set by [`LaneControls::resync`]: the next block sends at frame 0.
    resync: AtomicBool,
    knob: Option<ParamCell>,
    thinned: AtomicU64,
    truncated: AtomicU64,
    unranged: AtomicU64,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("seq", &self.program.seq())
            .field("resync", &self.resync.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// A reader on a lane's cell ([`LaneControls::probe_reader`]): what a test
/// of an owner's sweep lends through to stand in for a block. Not API.
#[doc(hidden)]
#[derive(Debug)]
pub struct LaneProbe(Reader<ControlProgram>);

impl LaneProbe {
    /// Borrows the program as the node does once per block; the borrow pins
    /// it until it drops.
    pub fn lend(&mut self) -> impl Sized + '_ {
        self.0.lend()
    }
}

/// The handles a [`ControlLaneNode`]'s owner keeps: the program it
/// evaluates, a reading of its knob, and its counters. Cheap to clone; every clone reaches the same
/// lane. A port lane's knob and range are its port's own cells, written
/// through the port (`Editor::set_param_range`, the param's `Param`); a
/// foreign lane's are its own, written through its [`ForeignLaneControls`].
#[derive(Clone, Debug)]
pub struct LaneControls(Arc<Shared>);

impl LaneControls {
    /// Hands the lane `program`, from its next block's first frame, as a
    /// ramp from the value in flight. Control thread only.
    pub fn publish(&self, program: ControlProgram) {
        self.publish_arc(Arc::new(program));
    }

    /// [`publish`](Self::publish) of a program already behind an `Arc`.
    fn publish_arc(&self, program: Arc<ControlProgram>) {
        self.0.program.publish(program);
    }

    /// Frees the programs the lane has let go of since its last publish (one
    /// a block still held when [`publish`](Self::publish) replaced it),
    /// without publishing; returns how many are still held. Control thread,
    /// every tick of its owner (the [`Modulation`](crate::Modulation)
    /// reconciler's [`reclaim`](crate::Modulation::reclaim)); with nothing
    /// retired it is a lock and a length check. Not public: a lane's owner
    /// sweeps it through the reconciler, and a foreign lane's through
    /// [`ForeignLaneControls::reclaim`], which frees its ranges too.
    pub(crate) fn reclaim(&self) -> usize {
        self.0.program.reclaim()
    }

    /// How many programs the lane's cell holds retired: replaced, and not
    /// yet freed by a publish or a [`reclaim`](Self::reclaim). Control side.
    /// For a test of an owner's sweep; not API.
    #[doc(hidden)]
    pub fn retired_programs(&self) -> usize {
        self.0.program.retired_len()
    }

    /// A reader registered on the lane's cell, as the node's own is, so a
    /// test of an owner's sweep can hold a block's borrow across a publish.
    /// Control thread: registering may allocate. Not API.
    #[doc(hidden)]
    pub fn probe_reader(&self) -> LaneProbe {
        LaneProbe(self.0.program.reader())
    }

    /// The program last published (or built with).
    pub fn program(&self) -> Arc<ControlProgram> {
        self.0.program.current()
    }

    /// Makes the lane send its value at its next block's first frame, even
    /// while steady: what to call after the lane's output is
    /// re-wired to another target, or its target replaced, since a lane
    /// sends nothing while nothing moves and a new target would otherwise
    /// hold its own base until something does. The send is a point, not a
    /// ramp: the new target has no value in flight to ramp from. Control
    /// thread.
    pub fn resync(&self) {
        self.0.resync.store(true, Ordering::Release);
    }

    /// The knob the lane reads now, if it has one: a port lane's is its
    /// port's own cell, a foreign lane's its own. The parameter's own raw
    /// number, as the lane sends it; a foreign lane's controls read it typed
    /// ([`ForeignLaneControls::value`]).
    #[must_use]
    pub fn value(&self) -> Option<f64> {
        self.0.knob.as_ref().map(|k| k.load(Ordering::Acquire))
    }

    /// Blocks whose grid the lane thinned to stay within
    /// [`SOURCE_EVENT_CAPACITY`]: each may differ between live and export.
    pub fn blocks_thinned(&self) -> u64 {
        self.0.thinned.load(Ordering::Relaxed)
    }

    /// Blocks in which the lane dropped points past [`SOURCE_EVENT_CAPACITY`]
    /// with no grid left to thin (more than that many corners in one block).
    pub fn blocks_truncated(&self) -> u64 {
        self.0.truncated.load(Ordering::Relaxed)
    }

    /// Blocks whose program had offsets while the range was not finite: the
    /// lane sent the base alone.
    pub fn blocks_unranged(&self) -> u64 {
        self.0.unranged.load(Ordering::Relaxed)
    }
}

/// The piece the target is on: the segment last sent, the frame it was
/// sent on, and the lane's next point.
#[derive(Clone, Copy)]
struct Flight {
    /// `None`: the lane had no value and sent nothing.
    seg: Option<ParamSegment>,
    /// The segment's raw endpoints (0 with no segment).
    from: f64,
    to: f64,
    sent: Frame,
    /// The next point; `None` while the value is steady (no point is due
    /// until something changes).
    next: Option<Frame>,
}

/// An event input's current segment.
#[derive(Clone, Copy)]
struct Input {
    source: SourceId,
    seg: ParamSegment,
    recv: Frame,
}

/// The state a block's walk moves on: copied before a walk, so a walk that
/// overflows is retried from the same place.
#[derive(Clone, Copy)]
struct Walk {
    flight: Option<Flight>,
    inputs: [Option<Input>; MAX_LANE_INPUTS],
}

/// What the lane saw at the end of its last block.
#[derive(Clone, Copy)]
struct Seen {
    /// The sequence number of the program it read (`Loan::seq`); `None`
    /// before its first block, so that block counts as a change.
    seq: Option<u64>,
    knob: Option<u64>,
    range: (u64, u64),
    /// The program's depths at the last block's start.
    depths: DepthSnapshot,
    /// The frame after the last block, the transport and play clock at its
    /// last frame: how a seek between blocks is told from a roll.
    end: Option<(Frame, TransportPosition, PlayReading)>,
}

/// One target's control lane: it reads a [`ControlProgram`] (published
/// through [`LaneControls`]), its target's knob and range, the transport
/// and play clock at its input's time, and its event input's signal
/// sources, and sends the target piecewise segments.
///
/// - **Points**, where a piece starts: every frame whose play count is a
///   multiple of the [control grid](CONTROL_GRID); every corner of the program (a
///   breakpoint, an edge, a peak, a zero crossing of a curved bipolar
///   layer, a rate anchor); every transport change, seek and loop wrap;
///   every input event; frame 0 when nothing is in flight or the program,
///   knob or range changed. At each point the lane sends a linear piece
///   from the value there to the value just before the next point.
/// - **A piece in flight continues** across a block edge untouched: the
///   lane does not re-send it, so what the target renders does not depend
///   on where blocks fall.
/// - **Exact mode.** A [`PortLane`] whose program is automation alone
///   sends the curve's own shaped pieces (as `AutomationLaneNode` sent
///   them); a curve that cannot say its pieces takes the grid. A
///   [`ForeignLane`] never uses it: a plugin receives points.
/// - **Steady.** When nothing moves (stopped with no seconds-clock layer,
///   or no layers but constant ones, and every input arrived) the lane
///   sends one held value and no grid.
/// - **The clamp** is the port's, per frame, for a [`PortLane`]. A
///   [`ForeignLane`] clamps its own pieces, split where they cross a bound.
///
/// - **Capacity.** It sends at most [`SOURCE_EVENT_CAPACITY`] segments a
///   block, as a [`SourceNode`](crate::SourceNode) does. Past it the lane
///   thins its grid and counts it ([`LaneControls::blocks_thinned`]); past it with
///   no grid left to thin, it drops the block's last points and counts that
///   ([`LaneControls::blocks_truncated`]).
///
/// The lane is its own node, not a [`SourceNode`](crate::SourceNode) over a
/// [`ControlSource`](tutti_modulation::ControlSource): a program reads two
/// clocks (the transport's beat and seconds beside the play count), takes
/// its signal sources' trajectories rather than MIDI, re-reads a published
/// program, knob and range each block, and clamps or splits its pieces for
/// its target, none of which a source states. For the same reason it reads
/// the play clock at its own points beside the transport, and finds play
/// starts as transport changes, rather than walking the play clock frame by
/// frame as a source node does: a second walk would only repeat what its
/// transport reading has found.
///
/// No allocation on the audio thread: per-input state is a fixed array of
/// [`MAX_LANE_INPUTS`], and the block's events are built in a buffer
/// reserved at `prepare`.
pub struct ControlLaneNode<K: LaneKind> {
    shared: Arc<Shared>,
    /// A reader of `shared.program`, registered when the node is built (a
    /// fork builds a node over a new cell, and registers there).
    program: Reader<ControlProgram>,
    /// The parameter the lane sends to.
    addr: K::Addr,
    /// The target's range as the control side holds it: what a fork freezes.
    source: K::Source,
    /// A reader of the target's range, registered with the node and lent
    /// once per block.
    range: K::Range,
    walk: Walk,
    seen: Seen,
    scratch: Vec<ParamEvent>,
}

impl<K: LaneKind> std::fmt::Debug for ControlLaneNode<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlLaneNode")
            .field("program", &self.program)
            .field("shaped", &K::SHAPED)
            .field("scratch_len", &self.scratch.len())
            .finish_non_exhaustive()
    }
}

fn fresh_walk() -> Walk {
    Walk {
        flight: None,
        inputs: [None; MAX_LANE_INPUTS],
    }
}

fn fresh_seen() -> Seen {
    Seen {
        seq: None,
        knob: None,
        range: (0, 0),
        depths: DepthSnapshot::EMPTY,
        end: None,
    }
}

/// The controls of a lane for a foreign unit's parameter, from
/// [`ForeignLaneConfig::build`]: its [`LaneControls`], and the knob and the
/// range it owns.
/// Cheap to clone; every clone reaches the same lane.
#[derive(Clone, Debug)]
pub struct ForeignLaneControls {
    lane: LaneControls,
    range: Arc<Publisher<ParamRange>>,
}

impl ForeignLaneControls {
    /// The lane's program and counters.
    #[must_use]
    pub fn lane(&self) -> &LaneControls {
        &self.lane
    }

    /// Sets the lane's range, the span its offsets scale by and the clamp,
    /// from the next block. Control thread; the value it replaces is freed
    /// by [`reclaim`](Self::reclaim).
    pub fn publish_range(&self, range: ParamRange) {
        self.range.publish(range);
    }

    /// Writes the lane's knob, the base its offsets add to. `false` for a
    /// lane built without one.
    pub fn store(&self, value: Normalized) -> bool {
        match &self.lane.0.knob {
            Some(k) => {
                k.store(value.get(), Ordering::Release);
                true
            }
            None => false,
        }
    }

    /// The knob now, if the lane has one.
    #[must_use]
    pub fn value(&self) -> Option<Normalized> {
        self.lane.value().map(Normalized::clamped)
    }

    /// Frees the programs and the ranges the lane has let go of since their
    /// last publish, as [`LaneControls::reclaim`] does for the programs
    /// alone; returns how many are still held. Control thread, every tick of
    /// its owner.
    pub fn reclaim(&self) -> usize {
        self.lane.reclaim() + self.range.reclaim()
    }
}

/// What a [`ControlLaneNode`] for a built-in parameter is made from: the parameter, its
/// program, and the cells of the target it reads.
///
/// The lane sends `param` (a [`UnitParam`], which is its index-0 address, a [`UnitAddr`], or a
/// `ParamKey::addr()`) to its param port, wired with `GraphSpec::connect_param`:
/// the program's values are the param's raw values ([`ParamSegment::unit`]).
#[derive(Debug)]
pub struct UnitLaneConfig {
    param: UnitAddr,
    program: ControlProgram,
    knob: Option<ParamCell>,
    range: Option<ParamRangeCell>,
}

impl UnitLaneConfig {
    /// A lane for `param` running `program`, with no knob and no range.
    #[must_use]
    pub fn new(param: impl Into<UnitAddr>, program: ControlProgram) -> Self {
        Self {
            param: param.into(),
            program,
            knob: None,
            range: None,
        }
    }

    /// The target node's own `Param` cell (`ParamSet::cell`): the base where the program has
    /// no automation value. Without it the lane sends nothing there.
    #[must_use]
    pub fn knob(mut self, knob: ParamCell) -> Self {
        self.knob = Some(knob);
        self
    }

    /// The port's range cell (`Editor::param_range_cell`), read once per block for the span
    /// the program's offsets scale by. Without it the lane is unranged and sends the base
    /// alone.
    #[must_use]
    pub fn range(mut self, range: ParamRangeCell) -> Self {
        self.range = Some(range);
        self
    }
}

impl From<UnitLaneConfig> for ControlLaneNode<PortLane> {
    fn from(config: UnitLaneConfig) -> Self {
        Self::build(
            Arc::new(config.program),
            config.knob,
            config.param,
            config.range,
        )
    }
}

/// What a [`ControlLaneNode`] for a foreign unit's parameter (a hosted plugin's) is made
/// from: its id, its program, and the range the lane clamps to.
///
/// The lane speaks the unit's own normalisation, keeps a knob of its own and a range of its
/// own, and never uses exact mode.
#[derive(Debug)]
pub struct ForeignLaneConfig {
    id: ParamId,
    program: ControlProgram,
    range: ParamRange,
    knob: Option<Normalized>,
}

impl ForeignLaneConfig {
    /// A lane for the foreign parameter `id` running `program`, clamped to `range`, with no
    /// knob.
    #[must_use]
    pub fn new(id: ParamId, program: ControlProgram, range: ParamRange) -> Self {
        Self {
            id,
            program,
            range,
            knob: None,
        }
    }

    /// A knob of the lane's own, starting at `knob`.
    #[must_use]
    pub fn knob(mut self, knob: Normalized) -> Self {
        self.knob = Some(knob);
        self
    }

    /// The lane, and the controls that set its range: the one way to build
    /// a lane for a foreign parameter.
    #[must_use]
    pub fn build(self) -> (ControlLaneNode<ForeignLane>, ForeignLaneControls) {
        let range = Arc::new(Publisher::new(self.range));
        let node = ControlLaneNode::build(
            Arc::new(self.program),
            self.knob.map(|k| Arc::new(AtomicF64::new(k.get())).into()),
            self.id,
            Arc::clone(&range),
        );
        let controls = ForeignLaneControls {
            lane: node.controls(),
            range,
        };
        (node, controls)
    }
}

impl<K: LaneKind> ControlLaneNode<K> {
    const SHAPE: Shape = Shape::EMPTY
        .events(1, 1)
        .param_events()
        .event_capacity(SOURCE_EVENT_CAPACITY)
        .cost(Cost::Moderate)
        .tail(Tail::None)
        .no_note_events();
    const IN: InEvents = Self::SHAPE
        .event_input(0)
        .expect("declared one event input");
    const OUT: OutEvents = Self::SHAPE
        .event_output(0)
        .expect("declared one event output");

    fn build(
        program: Arc<ControlProgram>,
        knob: Option<ParamCell>,
        addr: K::Addr,
        source: K::Source,
    ) -> Self {
        let shared = Shared {
            program: Publisher::from_arc(program),
            resync: AtomicBool::new(false),
            knob,
            thinned: AtomicU64::new(0),
            truncated: AtomicU64::new(0),
            unranged: AtomicU64::new(0),
        };
        Self {
            program: shared.program.reader(),
            range: K::reader(&source),
            shared: Arc::new(shared),
            addr,
            source,
            walk: fresh_walk(),
            seen: fresh_seen(),
            scratch: Vec::with_capacity(SOURCE_EVENT_CAPACITY.get() as usize),
        }
    }

    /// The lane's controls (also handed out by an insert).
    pub fn controls(&self) -> LaneControls {
        LaneControls(Arc::clone(&self.shared))
    }

    /// A fresh lane for a fork: the last published program, the knob and
    /// range as they are now in cells of its own, the same address, and no
    /// piece in flight. Control thread.
    #[must_use]
    pub fn fork_fresh(&self) -> Self {
        Self::fresh_over(&self.shared, self.addr, &self.source)
    }

    /// [`fork_fresh`](Self::fork_fresh) from a lane's parts, for the fork
    /// source, which keeps the parts rather than a lane: it reads the program
    /// with `Publisher::current` (an owning read under the publisher's lock,
    /// never a lend), the knob and the range, so it needs no read state of
    /// its own.
    fn fresh_over(s: &Shared, addr: K::Addr, source: &K::Source) -> Self {
        // Depth cells of its own, so a depth moved after the fork does not
        // reach it.
        let program = Arc::new(s.program.current().detached());
        let knob = s.knob.as_ref().map(ParamCell::detached);
        Self::build(program, knob, addr, K::frozen(source))
    }
}

/// The transport one frame on from `t`, as the executor's own history rolls
/// it (`FrameClock` advanced, loop wrapped).
fn roll_one(t: TransportPosition, rate: tutti_units::SampleRate) -> TransportPosition {
    if !t.playing || t.tempo.frames_per_beat(rate).is_none() {
        return t;
    }
    let mut clock = t.clock(rate);
    clock.advance(Samples::new(1), t.looping);
    TransportPosition::counted(t.playing, t.tempo, clock.origin(), t.looping)
}

/// Whether `got` is `expected` (a roll of the last block's last frame):
/// equal play state, tempo and loop, and the beat within a nanobeat.
fn continues(expected: &TransportPosition, got: &TransportPosition) -> bool {
    let (a, b) = (expected.beat().get(), got.beat().get());
    expected.playing == got.playing
        && expected.tempo.get().to_bits() == got.tempo.get().to_bits()
        && expected.looping == got.looping
        && (a - b).abs() <= 1e-9 * a.abs().max(1.0)
}

/// A curve piece's `(since, span)` for a point on a frame: the piece started
/// `since` frames before it and is `since + until` long, both fractional, so a
/// segment evaluates its law at each frame's exact place on the piece. `None`
/// when the piece is longer than a segment can count (`u32::MAX` frames): a
/// producer sends it as shorter pieces.
fn exact_span(since: f64, until: f64) -> Option<(SamplePosition, SamplePosition)> {
    let span = since + until;
    (span <= f64::from(u32::MAX)).then_some((SamplePosition::new(since), SamplePosition::new(span)))
}

/// A segment's value `j` frames (fractional) after its event, `to` once it
/// has arrived: the port's arithmetic, for a time between frames.
fn value_along(seg: &ParamSegment, from: f64, to: f64, j: f64) -> f64 {
    let (p, l) = (seg.position().get() as f64, seg.length().get() as f64);
    if l <= 0.0 {
        return to;
    }
    let t = (p + j) / l;
    if t < 1.0 {
        Piece::new(from, to)
            .law(seg.shape())
            .value(seg.unit_param(), Normalized::clamped(t))
    } else {
        to
    }
}

impl Input {
    /// The input's raw value at `at` frames after the block start of `base`
    /// (fractional): the port's own arithmetic on a frame, `f64` between.
    fn value(&self, at: f64, base: Frame) -> f64 {
        let since = base.get() as f64 + at - self.recv.get() as f64;
        let id = self.source.get();
        let (from, to) = (
            self.seg.start_by_id(id).unwrap_or(0.0),
            self.seg.target_by_id(id).unwrap_or(0.0),
        );
        if since.fract() == 0.0 && since >= 0.0 {
            self.seg.raw_value_after(Samples::new(since as usize))
        } else {
            value_along(&self.seg, from, to, since.max(0.0))
        }
    }

    /// Frames from `at` (after `base`) until the input arrives; `None` once
    /// it has.
    fn arrives_in(&self, at: f64, base: Frame) -> Option<f64> {
        let since = base.get() as f64 + at - self.recv.get() as f64;
        let arrive = self.seg.arrives_after().get() as f64;
        (arrive > since).then_some(arrive - since)
    }
}

/// Why a walk stopped short.
enum Overflow {
    /// The capacity filled with the grid at `stride`.
    Full,
}

/// One block's walk, borrowed together.
struct Block<'b, K: LaneKind> {
    cx: &'b Cx<'b>,
    program: &'b ControlProgram,
    knob: Option<f64>,
    range: ParamRange,
    span: f64,
    addr: K::Addr,
    /// The built-in param behind `addr`: a circular one takes the short arc
    /// between its values and sums its offsets as `wrap(base + offsets)`.
    unit: Option<UnitParam>,
    stride: u64,
    /// Thinning failed already: drop past the capacity instead.
    truncate: bool,
    seconds: bool,
    /// The program's depths, loaded once for the block.
    depths: DepthSnapshot,
}

impl<K: LaneKind> Block<'_, K> {
    fn len(&self) -> usize {
        self.cx.env.block_len.get()
    }

    fn base(&self) -> Frame {
        self.cx.env.frame
    }

    /// The inputs' raw values at `at` frames into the block, in the
    /// program's input order.
    fn inputs_at(&self, walk: &Walk, at: f64) -> [Option<f64>; MAX_LANE_INPUTS] {
        let mut out = [None; MAX_LANE_INPUTS];
        for (k, l) in self.program.inputs().iter().enumerate() {
            out[k] = walk
                .inputs
                .iter()
                .flatten()
                .find(|i| i.source == l.source)
                .map(|i| i.value(at, self.base()));
        }
        out
    }

    /// The program's value at `time`, for the lane's param.
    fn value_at(&self, time: &ProgramTime, ins: &[Option<f64>]) -> Option<f64> {
        self.program
            .value_at_in(self.unit, self.knob, time, self.span, ins, &self.depths)
    }

    /// The program's left limit at `time`, for the lane's param.
    fn value_before(&self, time: &ProgramTime, ins: &[Option<f64>]) -> Option<f64> {
        self.program
            .value_before_in(self.unit, self.knob, time, self.span, ins, &self.depths)
    }

    /// The earliest arrival of an input still moving at `at`, in frames.
    fn inputs_moving(&self, walk: &Walk, at: f64) -> Option<f64> {
        self.program
            .inputs()
            .iter()
            .filter_map(|l| {
                walk.inputs
                    .iter()
                    .flatten()
                    .find(|i| i.source == l.source)
                    .and_then(|i| i.arrives_in(at, self.base()))
            })
            .min_by(f64::total_cmp)
    }

    /// Sends `seg` at frame `p`.
    fn send(&self, out: &mut Vec<ParamEvent>, p: usize, seg: ParamSegment) -> Result<(), Overflow> {
        if out.len() >= SOURCE_EVENT_CAPACITY.get() as usize {
            return Err(Overflow::Full);
        }
        if let Some(at) = Offset::new(Samples::new(p), self.cx.env.block_len) {
            out.push(ParamEvent::segment(at, seg));
        }
        Ok(())
    }

    /// The point at frame `p`: one piece from the value there (or, ramping,
    /// from the value in flight) to the value just before the next point.
    fn point(
        &self,
        walk: &mut Walk,
        out: &mut Vec<ParamEvent>,
        p: usize,
        ramp: bool,
    ) -> Result<(), Overflow> {
        // Distances to the next point (`d`, the wrap, a corner, an input's
        // arrival) are fractional frame counts held as bare `f64` in this
        // function, and leave it as `Samples` or a `Frame`. A `SampleSpan`
        // cannot carry them through: the walk needs "no next point" (an
        // infinite distance), the nearest whole frame and how far off it is,
        // the ratio of two spans, and a signed difference of two positions,
        // and `FramesPerBeat::frames_in` hands its frames back bare.
        let env = self.cx.env;
        let off = Offset::new(Samples::new(p), env.block_len).unwrap_or(Offset::ZERO);
        let t = self.cx.transport_at(off);
        let play = self.cx.play_at(off);
        let rate = env.sample_rate;
        let fpb = t.tempo.frames_per_beat(rate).filter(|_| t.playing);
        let abs = self.base() + Samples::new(p);
        let beat = t.beat().get();
        let time = ProgramTime {
            beat: t.beat(),
            play: SamplePosition::new(play.play.get() as f64),
            play_epoch: play.epoch,
            rate,
        };
        // The loop's wrap, a point of its own, in frames from here.
        let wrap = match (fpb, t.looping) {
            (Some(f), Some(l)) if l.end().get() > beat => Some(f.frames_in(l.end() - t.beat())),
            _ => None,
        };
        let pf = p as f64;
        let moving = self.inputs_moving(walk, pf);
        let addr = self.addr;
        let make = |from: f64, to: f64, law: Law| K::segment(addr, from, to, law);
        let next_at = |d: Option<f64>| d.map(|d| abs + Samples::new(d.ceil().max(1.0) as usize));
        let flight = |seg: Option<(ParamSegment, f64, f64)>, next: Option<f64>| Flight {
            seg: seg.map(|s| s.0),
            from: seg.map_or(0.0, |s| s.1),
            to: seg.map_or(0.0, |s| s.2),
            sent: abs,
            next: next_at(next),
        };
        // A ramp that would re-send the piece in flight unchanged (same
        // end, same next point) leaves it alone: a republish that moves
        // nothing changes no frame.
        let old = walk.flight.filter(|f| ramp && f.seg.is_some());
        let unchanged = |from: Option<f64>, to: f64, next: Option<f64>, seg: &ParamSegment| {
            old.is_some_and(|o| {
                o.to.to_bits() == to.to_bits()
                    && from.is_none_or(|f| {
                        f.to_bits() == o.from.to_bits()
                            && o.seg.is_some_and(|s| {
                                s.shape() == seg.shape() && s.length() == seg.length()
                            })
                    })
                    && o.next == next_at(next)
            })
        };

        // Exact mode: a port's pure automation, rolling.
        if K::SHAPED && self.program.is_pure_automation() && moving.is_none() {
            let curve = self.program.automation().map(|c| &**c);
            if let (Some(curve), Some(f)) = (curve, fpb) {
                let exact = match curve.segment_at(t.beat()) {
                    Some(CurveSegment::Move {
                        start,
                        end,
                        from,
                        to,
                        shape,
                    }) if from.is_finite() && to.is_finite() && end.get() > beat => {
                        // The piece on whole frames: its start and end round to
                        // the nearest frame, so a piece sent in any block is
                        // the same piece. One past what a segment can count is
                        // not exact: the grid takes it.
                        exact_span(f.frames_in(t.beat() - start), f.frames_in(end - t.beat())).map(
                            |(since, span)| {
                                let seg = make(from, to, shape).resumed_exactly(since, span);
                                (seg, from, to, Some(f.frames_in(end - t.beat())))
                            },
                        )
                    }
                    Some(CurveSegment::Hold { value, until }) if value.is_finite() => Some((
                        make(value, value, Law::Linear),
                        value,
                        value,
                        until
                            .map(|u| f.frames_in(u - t.beat()))
                            .filter(|d| *d > 0.0),
                    )),
                    None if curve.value_at(t.beat()).is_none() => {
                        // No value anywhere: the knob, held.
                        self.knob.map(|k| (make(k, k, Law::Linear), k, k, None))
                    }
                    _ => None,
                };
                if let Some((seg, from, to, d)) = exact {
                    let d = match (d, wrap) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    if unchanged(Some(from), to, d, &seg) {
                        return Ok(());
                    }
                    self.send(out, p, seg)?;
                    walk.flight = Some(flight(Some((seg, from, to)), d));
                    return Ok(());
                }
            }
        }

        let ins = self.inputs_at(walk, pf);
        let n_in = self.program.inputs().len();
        let in_flight = walk.flight.and_then(|f| f.seg.map(|s| (s, f.sent)));
        let v0 = match (ramp, in_flight) {
            (true, Some((seg, sent))) => {
                Some(seg.raw_value_after(abs.since(sent).unwrap_or(Samples::new(0))))
            }
            _ => self.value_at(&time, &ins[..n_in]),
        };
        // What moves with time: a constant layer (a macro) does not.
        let layers = self.program.layers().iter().any(|l| !l.is_constant())
            || self.program.automation().is_some();
        let steady = moving.is_none() && (!layers || (fpb.is_none() && !self.seconds));
        let grid = (self.stride - play.play.get() % self.stride) as f64;
        let Some(v0) = v0 else {
            // No value: nothing sent, the target keeps its own; look again
            // at the next grid point.
            walk.flight = Some(flight(None, (!steady).then_some(grid)));
            return Ok(());
        };
        if steady {
            let v = if ramp {
                self.value_at(&time, &ins[..n_in]).map(|v| self.clamp(v))
            } else {
                Some(self.clamp(v0))
            };
            let seg = v.map(|v| make(v, v, Law::Linear));
            if let (Some(v), Some(seg)) = (v, seg) {
                if unchanged(None, v, None, &seg) {
                    return Ok(());
                }
                if !ramp {
                    self.send(out, p, seg)?;
                    walk.flight = Some(flight(Some((seg, v, v)), None));
                    return Ok(());
                }
            }
        }

        // The next point: the grid, or a corner before it.
        let mut d = if steady { f64::INFINITY } else { grid };
        let corners = self.program.next_corner(&time);
        let mut consider = |x: Option<f64>| {
            if let Some(x) = x.filter(|x| *x > 0.0 && x.is_finite()) {
                d = d.min(x);
            }
        };
        if let Some(f) = fpb {
            consider(corners.beat.map(|b| f.frames_in(b - t.beat())));
            consider(wrap);
        }
        consider(corners.play.map(|c| c.get() - time.play.get()));
        consider(moving);
        if !d.is_finite() {
            // Ramping into a steady value: one grid step.
            d = f64::from(CONTROL_GRID.stride());
        }
        // A corner on a frame computes a hair off it (a beat in frames): it
        // is that frame's, not the next one's.
        let nearest = d.round();
        if nearest >= 1.0 && (d - nearest).abs() < 1e-6 {
            d = nearest;
        }
        let end = ProgramTime {
            beat: fpb.map_or(t.beat(), |f| t.beat() + f.beats_over(d)),
            play: SamplePosition::new(time.play.get() + d),
            ..time
        };
        let ins_end = self.inputs_at(walk, pf + d);
        let mut v1 = self.value_before(&end, &ins_end[..n_in]).unwrap_or(v0);
        // A lane that clamps its own pieces ends this one where it crosses
        // a bound.
        if let Some(x) = K::cut(self.range, v0, v1) {
            // The first frame at or past the crossing. A crossing on a frame
            // can compute a hair past it (the piece's end is a left limit,
            // a nanobeat early): a position within a thousandth of a frame
            // of a whole one is that frame's own.
            let k = (x * d - 1e-3).ceil();
            if k >= 1.0 && k < d {
                v1 = v0 + (v1 - v0) * k / d;
                d = k;
            }
        }
        let (v0, v1) = (self.clamp(v0), self.clamp(v1));
        // The piece runs to the next point, which is on a whole frame: a
        // segment counts whole frames, so a corner between two frames ends
        // the line on the frame after it, extended along the same line. Each
        // frame before the corner is on the line it was, and the next point
        // replaces the piece on the frame that ends it.
        let len = SampleSpan::clamped(d).to_samples_ceil();
        let v1 = match len.get() as f64 {
            l if l > d && d > 0.0 => self.clamp(v0 + (v1 - v0) * (l / d)),
            _ => v1,
        };
        let seg = make(v0, v1, Law::Linear).resumed(Samples::new(0), len);
        if unchanged(None, v1, Some(d), &seg) {
            return Ok(());
        }
        self.send(out, p, seg)?;
        walk.flight = Some(flight(Some((seg, v0, v1)), Some(d)));
        Ok(())
    }

    /// `v` as the lane sends it: clamped by a lane that clamps its own
    /// pieces.
    fn clamp(&self, v: f64) -> f64 {
        K::clamp(self.range, v)
    }

    /// The block's walk from `walk`: points in frame order into `out`.
    fn run(
        &self,
        walk: &mut Walk,
        out: &mut Vec<ParamEvent>,
        events: &[ParamEvent],
        at_zero: Option<bool>,
    ) -> Result<(), Overflow> {
        let env = self.cx.env;
        let len = self.len();
        let changes = env.changes.as_slice();
        let mut ev = 0usize;
        let mut from = 0usize;
        let mut cur = at_zero.map(|g| (0usize, g));
        loop {
            if let Some((p, ramp)) = cur {
                // Input events on this frame land before its point.
                while let Some(e) = events.get(ev).filter(|e| e.offset.index() <= p) {
                    if let Some((id, seg)) = self.input_of(e) {
                        self.take_input(
                            walk,
                            id,
                            seg,
                            self.base() + Samples::new(e.offset.index()),
                        );
                    }
                    ev += 1;
                }
                if let Err(o) = self.point(walk, out, p, ramp) {
                    if !self.truncate {
                        return Err(o);
                    }
                    // Full with no grid left to thin: the rest of the block
                    // sends nothing, and the next block starts with a point.
                    if let Some(f) = walk.flight.as_mut() {
                        f.next = Some(self.base() + Samples::new(p));
                    }
                    for e in &events[ev..] {
                        if let Some((id, seg)) = self.input_of(e) {
                            self.take_input(
                                walk,
                                id,
                                seg,
                                self.base() + Samples::new(e.offset.index()),
                            );
                        }
                    }
                    return Err(o);
                }
                from = p + 1;
            }
            let sched = walk
                .flight
                .and_then(|f| f.next)
                .and_then(|n| n.since(self.base()))
                .map(|s| s.get())
                .filter(|&i| i >= from && i < len);
            let change = changes
                .iter()
                .map(|c| c.at.index())
                .find(|&i| i >= from && i < len);
            // Only an input the program reads is a point: MIDI or an unread
            // source uses no capacity.
            let input = events[ev.min(events.len())..]
                .iter()
                .find(|e| self.input_of(e).is_some())
                .map(|e| e.offset.index())
                .filter(|&i| i < len);
            let next = [sched, change, input].into_iter().flatten().min();
            match next {
                Some(p) => cur = Some((p, false)),
                None => return Ok(()),
            }
        }
    }

    /// Whether the program reads source `id`.
    fn reads(&self, id: SourceId) -> bool {
        self.program.inputs().iter().any(|l| l.source == id)
    }

    /// The segment `e` carries for a source the program reads, if any: the
    /// only events that are inputs (anything else is ignored, and is no
    /// point).
    fn input_of(&self, e: &ParamEvent) -> Option<(SourceId, ParamSegment)> {
        let ParamKind::Segment(seg) = e.kind else {
            return None;
        };
        let ParamAddr::Id(id) = seg.addr() else {
            return None;
        };
        let id = SourceId::new(id);
        self.reads(id).then_some((id, seg))
    }

    /// Records input `id`'s segment, received on frame `recv`, if the
    /// program reads that source (other events are ignored). Its slot is
    /// the source's own, else a free one: every slot holds a source the
    /// program reads (`process` frees the rest when it takes a program),
    /// and a program reads at most [`MAX_LANE_INPUTS`] sources, so a source
    /// it reads always finds one.
    fn take_input(&self, walk: &mut Walk, id: SourceId, seg: ParamSegment, recv: Frame) {
        if !self.reads(id) {
            return;
        }
        let slot = walk
            .inputs
            .iter()
            .position(|i| i.is_some_and(|i| i.source == id))
            .or_else(|| walk.inputs.iter().position(Option::is_none));
        if let Some(k) = slot {
            walk.inputs[k] = Some(Input {
                source: id,
                seg,
                recv,
            });
        }
    }
}

/// Whether any of `program`'s moving waves runs on the play clock (a
/// constant wave, a layer's or its `depth_by`'s, moves on no clock).
fn has_seconds(program: &ControlProgram) -> bool {
    let moves = |c: &LayerClock, w: &Waveform| {
        matches!(c, LayerClock::Seconds { .. }) && !matches!(w, Waveform::Const(_))
    };
    program.layers().iter().any(|l| {
        moves(&l.clock, &l.wave)
            || l.depth_by
                .as_ref()
                .is_some_and(|d| moves(&d.clock, &d.wave))
    })
}

impl<F: Float, K: LaneKind> Node<F> for ControlLaneNode<K> {
    /// No audio; one event input (signal sources), one event output
    /// (segments to the target).
    ///
    /// `Moderate`, from a measurement: 200 lanes of one
    /// moving layer each at 512-frame blocks spend about 3.7 µs per lane per
    /// block in `process` (a point's transport roll and the program's
    /// evaluation, sixteen grid points a block), about 7 ns a frame, where
    /// `Moderate`'s model is 200 + 8 a frame and `Light`'s 100 + 1.
    fn shape(&self) -> Shape {
        Self::SHAPE
    }

    fn prepare(&mut self, _: &Prepare) {
        self.scratch.reserve(SOURCE_EVENT_CAPACITY.get() as usize);
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let env = cx.env;
        let len = env.block_len.get();
        if len == 0 {
            return Status::Modified;
        }
        let s = &*self.shared;
        let loan = self.program.lend();
        let (seq, program) = (Some(loan.seq()), &*loan);
        let knob = s
            .knob
            .as_ref()
            .map(|k| k.load(Ordering::Acquire))
            .filter(|k| k.is_finite());
        let range = K::load(&mut self.range);
        let unit = K::unit(self.addr);
        let span = range.view(unit).span();
        let span = if span.is_finite() && span >= 0.0 {
            span
        } else {
            f64::INFINITY
        };
        let offsets = !program.layers().is_empty() || !program.inputs().is_empty();
        if offsets && !span.is_finite() {
            s.unranged.fetch_add(1, Ordering::Relaxed);
        }
        let range_bits = (range.min().to_bits(), range.max().to_bits());
        let depths = program.depth_snapshot();
        if seq != self.seen.seq {
            // A new program: forget every input it does not read, so a
            // source id a later program reuses starts from nothing, never from the
            // segment a removed source left.
            for slot in &mut self.walk.inputs {
                if slot.is_some_and(|i| !program.inputs().iter().any(|l| l.source == i.source)) {
                    *slot = None;
                }
            }
        }
        let changed = seq != self.seen.seq
            || knob.map(f64::to_bits) != self.seen.knob
            || range_bits != self.seen.range
            || depths != self.seen.depths;
        // A resync is a point at frame 0, never a ramp (`LaneControls::resync`).
        let resync = s.resync.swap(false, Ordering::AcqRel);
        let t0 = cx.transport_at(Offset::ZERO);
        let p0 = cx.play_at(Offset::ZERO);
        let moved = match self.seen.end {
            Some((frame, t, play)) => {
                frame != env.frame
                    || !continues(&roll_one(t, env.sample_rate), &t0)
                    || play.after(Samples::new(1)) != p0
            }
            None => true,
        };
        let due = self
            .walk
            .flight
            .is_none_or(|f| f.next.is_some_and(|n| n <= env.frame));
        let at_zero = (due || changed || moved || resync).then_some(changed && !moved && !resync);

        let events = io.events(Self::IN).params();
        let mut block = Block::<K> {
            cx,
            program,
            knob,
            range,
            span,
            addr: self.addr,
            unit,
            stride: u64::from(CONTROL_GRID.stride()),
            truncate: false,
            seconds: has_seconds(program),
            depths,
        };
        let mut thinned = false;
        loop {
            self.scratch.clear();
            let mut walk = self.walk;
            match block.run(&mut walk, &mut self.scratch, events, at_zero) {
                Ok(()) => {
                    self.walk = walk;
                    break;
                }
                Err(Overflow::Full) if !block.truncate => {
                    if block.stride < len as u64 {
                        block.stride *= 2;
                        thinned = true;
                    } else {
                        block.truncate = true;
                    }
                }
                Err(Overflow::Full) => {
                    self.walk = walk;
                    s.truncated.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
        if thinned {
            s.thinned.fetch_add(1, Ordering::Relaxed);
        }
        drop(loan);
        let out = io.event_out(Self::OUT);
        for e in &self.scratch {
            // Within the declared capacity: never refused.
            let _ = out.push_param(*e);
        }
        let last = Offset::new(Samples::new(len - 1), env.block_len).unwrap_or(Offset::ZERO);
        self.seen = Seen {
            seq,
            knob: knob.map(f64::to_bits),
            range: range_bits,
            depths,
            end: Some((
                env.frame + Samples::new(len),
                cx.transport_at(last),
                cx.play_at(last),
            )),
        };
        Status::Modified
    }

    fn reset(&mut self) {
        self.walk = fresh_walk();
        self.seen = fresh_seen();
    }
}

/// A lane's fork source: [`ControlLaneNode::fork_fresh`] at the gather,
/// from the lane's parts. It holds no lane, so no per-block read state.
struct LaneFork<K: LaneKind> {
    shared: Arc<Shared>,
    addr: K::Addr,
    source: K::Source,
}

impl<F: Float, K: LaneKind> ForkSource<F> for LaneFork<K> {
    fn fork(&self, _mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        Ok(Forked::new(Box::new(ControlLaneNode::<K>::fresh_over(
            &self.shared,
            self.addr,
            &self.source,
        ))))
    }
}

/// Inserted with its [`LaneControls`], and a fork that reads the last
/// published program and the knob and range values at the gather.
impl<F: Float, K: LaneKind> IntoNode<F> for ControlLaneNode<K> {
    type Controls = LaneControls;

    fn into_parts(self) -> NodeParts<LaneControls, F> {
        let controls = self.controls();
        let fork = LaneFork::<K> {
            shared: Arc::clone(&self.shared),
            addr: self.addr,
            source: self.source.clone(),
        };
        NodeParts::new(Box::new(self), controls, Some(Box::new(fork)), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program() -> Arc<ControlProgram> {
        Arc::new(ControlProgram::default())
    }

    /// A piece keeps its fractional start and length: any point within it
    /// names the same two ends.
    #[test]
    fn a_piece_keeps_its_fractional_span() {
        assert_eq!(
            exact_span(0.6, 10.4),
            Some((SamplePosition::new(0.6), SamplePosition::new(11.0)))
        );
        assert_eq!(
            exact_span(7.6, 3.4),
            Some((SamplePosition::new(7.6), SamplePosition::new(11.0)))
        );
    }

    /// A piece longer than a segment can count (`u32::MAX` frames) has no
    /// span: the lane sends the grid's shorter pieces instead.
    #[test]
    fn a_piece_past_u32_max_frames_is_not_exact() {
        let max = f64::from(u32::MAX);
        assert!(exact_span(0.0, max - 10.0).is_some());
        assert_eq!(exact_span(0.0, max + 10.0), None);
        // Counted from where it started, not from the point.
        assert_eq!(exact_span(max / 2.0 + 10.0, max / 2.0 + 10.0), None);
    }

    fn foreign(range: ParamRange) -> (ControlLaneNode<ForeignLane>, ForeignLaneControls) {
        ForeignLaneConfig::new(ParamId::new(0), ControlProgram::default(), range).build()
    }

    /// **A program a block still held when the lane was republished is freed
    /// by the next `reclaim`**, with no further publish.
    #[test]
    fn reclaim_frees_a_program_a_block_held_across_its_replacement() {
        let (mut node, foreign) = foreign(ParamRange::of(0.0, 1.0));
        let controls = foreign.lane();
        let first = program();
        let gone = Arc::downgrade(&first);
        controls.publish_arc(first);
        {
            // A block in progress across the republish.
            let _held = node.program.lend();
            controls.publish_arc(program());
        }
        assert!(gone.upgrade().is_some(), "retired, pinned until a scan");
        assert_eq!(controls.reclaim(), 0);
        assert!(gone.upgrade().is_none(), "freed by the reclaim");
    }

    /// **A foreign lane's range a block still held when it was set anew is
    /// freed by its controls' next `reclaim`**, with no further set; the
    /// program-only `LaneControls::reclaim` leaves it.
    #[test]
    fn reclaim_frees_a_range_a_block_held_across_its_replacement() {
        let (mut node, controls) = foreign(ParamRange::of(0.0, 1.0));
        {
            // A block in progress across the range change.
            let _held = node.range.lend();
            controls.publish_range(ParamRange::of(0.25, 0.75));
        }
        assert_eq!(controls.lane().reclaim(), 0);
        assert_eq!(
            controls.range.retired_len(),
            1,
            "retired, pinned until its owner's scan"
        );
        assert_eq!(controls.reclaim(), 0);
        assert_eq!(
            controls.range.retired_len(),
            0,
            "freed by the foreign controls' reclaim"
        );
    }

    /// **A plugin lane reads the range its controls set**, from its next
    /// block; **its fork reads the range at the fork**, and a later set does
    /// not reach the fork.
    #[test]
    fn a_plugin_lane_and_its_fork_read_the_range_set_before_them() {
        let (mut node, controls) = foreign(ParamRange::of(0.0, 1.0));
        controls.publish_range(ParamRange::of(0.1, 0.9));
        assert_eq!(*node.range.lend(), ParamRange::of(0.1, 0.9));
        let mut fork = node.fork_fresh();
        controls.publish_range(ParamRange::of(0.2, 0.3));
        assert_eq!(*node.range.lend(), ParamRange::of(0.2, 0.3));
        assert_eq!(*fork.range.lend(), ParamRange::of(0.1, 0.9));
    }
}
