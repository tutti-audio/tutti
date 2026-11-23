//! Param events: the ramps and curve segments a param port reads.
//!
//! A param event is not an [`Event`](crate::Event). Its payload is the unit's
//! quantity, an `f64`, so it is larger than the 32 bytes every MIDI event is
//! held to, and it travels in its own buffer beside an event port's events: a
//! [`ParamEvent`] list, merged, delayed and read like the events, so a MIDI
//! event never pays for it.

use tutti_curve::{Law, Piece};
use tutti_units::{
    Normalized, ParamAddr, ParamId, ParamKey, SamplePosition, SampleSpan, Samples, UnitAddr,
    UnitParam,
};

use crate::node::declare::Scalar;
use crate::node::time::Offset;

/// A ramp of one parameter from the value it has at the event's frame to a
/// target, starting there: linear, or along a [`Law`]
/// ([`with_shape`](Self::with_shape)).
///
/// **Relative, unlike a [`ParamSegment`].** Its start value is whatever the
/// receiving port's source holds on the event's frame, so the shape is
/// evaluated exactly as a segment's is, with that value as `from`: frame `j`
/// after the event (`j = 0` its own) reads
/// [`Piece::value`] at `(j + 1) / duration` (`from + (target - from) · shape.eval(t)`), and `target`
/// from frame `duration - 1` on. Re-sending one restarts it from where the
/// source stands.
///
/// A shape that needs `from` to be chosen ([`Law::Exp`]'s
/// geometric `k = ln(to / from)`) wants a [`ParamSegment`], which names its
/// start value: a shape is a function of progress alone.
///
/// Addressed by [`ParamAddr`], so it reaches both tutti's own parameters and a
/// id-addressed target's (a hosted plugin's numeric id) without a later breaking
/// change:
///
/// - **Built-in params** are built from a typed [`ParamKey<U>`] and a `U`,
///   and read back the same way, so the unit is checked at both ends. In
///   between the value travels type-erased (the stable [`UnitAddr`] id and
///   the unit's `f64`), because one event port carries ramps for parameters of
///   different units and a [`ParamEvent`] has to be one `Copy` type. The raw
///   value is private: nothing reads a built-in ramp without naming its key.
/// - **Id-addressed params** ([`ParamRamp::by_id`]) carry a [`ParamId`] and
///   a raw value in the id-addressed target's own normalisation. That is the C ABI
///   boundary the units rule stops at: tutti does not know the unit.
///
/// ```
/// use tutti_graph::{Exponent, Law, ParamRamp};
/// use tutti_units::{Db, Hz, ParamId, ParamKey, Samples};
/// let r = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz::new(800.0), Samples::new(64));
/// assert_eq!(r.target(ParamKey::<Hz>::CUTOFF), Some(Hz::new(800.0)));
/// assert_eq!(r.target(ParamKey::<Db>::THRESHOLD), None); // another param
/// assert_eq!(r.target_by_id(ParamId::new(7)), None); // no id-addressed value
/// assert_eq!(r.shape(), Law::Linear);
/// let eased = r.with_shape(Law::PowerIn(Exponent::of(2.0)));
/// assert_eq!(eased.shape(), Law::PowerIn(Exponent::of(2.0)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamRamp {
    addr: ParamAddr,
    target: f64,
    // Frames, saturated to `u32` (what the param port counts in anyway).
    duration: u32,
    shape: Law,
}

/// A frame count as a `u32`, saturating.
fn saturate(n: Samples) -> u32 {
    u32::try_from(n.get()).unwrap_or(u32::MAX)
}

impl ParamRamp {
    /// Ramp built-in parameter `key` linearly to `target` over `duration`
    /// (zero is a step).
    pub fn new<U: Scalar>(key: ParamKey<U>, target: U, duration: Samples) -> Self {
        Self::packed(ParamAddr::unit(key.addr()), target.to_cell(), duration)
    }

    /// Ramp a id-addressed target's parameter `id` linearly to the raw `target`
    /// over `duration`. The value is in the id-addressed target's own terms (see the
    /// type docs).
    ///
    /// A hosted plugin's node starts the ramp from where the parameter
    /// stood at the end of the frame before the ramp's (the segment it was
    /// on, else the value the node last gave it). On a parameter the node has
    /// never given a value it knows no start, and the ramp is a jump to
    /// `target` on its frame, whatever its duration and shape.
    pub fn by_id(id: ParamId, target: f64, duration: Samples) -> Self {
        Self::packed(ParamAddr::Id(id), target, duration)
    }

    fn packed(addr: ParamAddr, target: f64, duration: Samples) -> Self {
        Self {
            addr,
            target,
            duration: saturate(duration),
            shape: Law::Linear,
        }
    }

    /// This ramp along `shape` instead of a straight line: a ramp that
    /// eases in or out from wherever the parameter stands. A zero duration
    /// is still a step.
    #[must_use]
    pub fn with_shape(mut self, shape: Law) -> Self {
        self.shape = shape;
        self
    }

    /// The shape it moves along ([`Law::Linear`] unless
    /// [`with_shape`](Self::with_shape) set one).
    pub fn shape(&self) -> Law {
        self.shape
    }

    /// Which parameter this ramps — for routing on the address.
    pub fn addr(&self) -> ParamAddr {
        self.addr
    }

    /// The target, in `key`'s unit, if this ramp is for `key`.
    pub fn target<U: Scalar>(&self, key: ParamKey<U>) -> Option<U> {
        (self.addr == ParamAddr::unit(key.addr())).then(|| U::from_cell(self.target))
    }

    /// The raw target, if this ramp is for parameter `id`.
    pub fn target_by_id(&self, id: ParamId) -> Option<f64> {
        (self.addr == ParamAddr::Id(id)).then_some(self.target)
    }

    /// The target as the wire carries it, whatever the unit: for the
    /// graph's own param modulation, which is erased over the unit as the
    /// port it drives is.
    pub(crate) fn raw_target(&self) -> f64 {
        self.target
    }

    /// How long the ramp takes (saturated at `u32::MAX` frames, about a
    /// day at 48 kHz).
    pub fn duration(&self) -> Samples {
        Samples::new(self.duration as usize)
    }
}

/// A curve segment of one parameter: from a start value to an end value
/// along a [`Law`], over a length in frames, evaluated by the param
/// port **per frame inside the block** — sample-accurate non-linear
/// automation, with no sub-chunking at breakpoints.
///
/// **Absolute, unlike a [`ParamRamp`].** A ramp moves from wherever its
/// source's value stands; a segment says both its endpoints and where along
/// itself it is at the event's frame. Re-sending the segment a producer is
/// on (each block, after a seek, a loop wrap, a tempo change) is therefore
/// idempotent: a producer can be a pure function of the transport, and a
/// segment entered part-way renders what one started at its beginning
/// would have there. The value at frame `j` after the event (`j = 0` is the
/// event's own frame) is
///
/// ```text
/// t = (position + j) / length
/// t < 1:  Piece { from, to, shape }.value(t)   (from + (to - from) · shape.eval(t))
/// t >= 1: to, held until the source's next event
/// ```
///
/// A segment of a circular param ([`UnitParam::is_circular`], `Azimuth`) takes
/// the short arc: the value for `t < 1` is `wrap(from + arc(from, to) ·
/// shape.eval(t))`, so one from 170 to -170 passes through 180, never 0. The
/// param's unit decides, not the segment.
///
/// **Position and length are frame counts**, `u32` (what the port counts in
/// anyway), so a segment's progress is exact however far along it is: a
/// ten-minute segment (2.88e7 frames at 48 kHz) advances one frame per frame
/// at its end. A segment longer than `u32::MAX` frames (6.2 hours at 192 kHz)
/// is not one a producer can state: it sends the span as shorter ones. A piece
/// that starts or ends between frames ([`resumed_exactly`](Self::resumed_exactly))
/// keeps the fraction as a phase, so the law is read at each frame's exact place.
///
/// [`new`](Self::new) builds the ramp convention (`position = 1`: the
/// event's frame is one step in, `to` lands on frame `duration - 1`);
/// [`resumed`](Self::resumed) sets a position and length, for a producer
/// that enters a segment part-way. A length of zero is a step to `to` on the
/// event's frame.
///
/// **Interrupted** by any later ramp or segment event for the same
/// parameter on the same source, from that event's frame: a segment jumps to
/// its own value there (it is absolute), a ramp moves on from the value the
/// interrupted segment had reached.
///
/// Addressed and typed as [`ParamRamp`] is (built-in params by
/// [`ParamKey<U>`], id-addressed ones by a `ParamId`). A param port's range
/// is applied when the port reads the value, never to the endpoints.
///
/// ```
/// use tutti_graph::{Exponent, Law, ParamSegment};
/// use tutti_units::{Hz, ParamKey, Samples};
/// let s = ParamSegment::new(
///     ParamKey::<Hz>::CUTOFF,
///     Hz::new(200.0),
///     Hz::new(800.0),
///     Samples::new(480),
/// )
/// .law(Law::PowerIn(Exponent::of(2.0)));
/// assert_eq!(s.target(ParamKey::<Hz>::CUTOFF), Some(Hz::new(800.0)));
/// assert_eq!(s.start(ParamKey::<Hz>::CUTOFF), Some(Hz::new(200.0)));
/// assert_eq!(s.shape(), Law::PowerIn(Exponent::of(2.0)));
/// // Frame 239 is 240/480 of the way: a quadratic ease-in is a quarter up.
/// assert_eq!(s.raw_value_after(Samples::new(239)), 200.0 + 600.0 * 0.25);
/// assert_eq!(s.raw_value_after(Samples::new(479)), 800.0);
/// assert_eq!(s.arrives_after(), Samples::new(479));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSegment {
    addr: ParamAddr,
    from: f64,
    to: f64,
    // Whole frames along itself at the event's frame, the fraction of a frame
    // past them (`lead`, in `[0, 1)`), and its exact length in frames.
    position: u32,
    lead: SampleSpan,
    span: SampleSpan,
    shape: Law,
}

impl ParamSegment {
    /// A segment of built-in parameter `key` from `from` to `to` along
    /// [`Law::default`] (see [`law`](Self::law)) over `duration` frames,
    /// in the ramp convention: the event's frame is one step in, and `to`
    /// lands on its last frame (a zero duration is a step to `to`).
    pub fn new<U: Scalar>(key: ParamKey<U>, from: U, to: U, duration: Samples) -> Self {
        Self::unit(key.addr(), from.to_cell(), to.to_cell(), duration)
    }

    /// A segment of built-in parameter `param` (a [`UnitParam`],
    /// which is its index-0 address, or a [`UnitAddr`]) in its raw `f64`
    /// domain, as [`new`](Self::new) otherwise: for a producer that is erased
    /// over the unit (a control lane built from a `ParamAddr`),
    /// and for a param with no [`ParamKey`] yet. The raw value is exactly what
    /// [`new`](Self::new) sends for a key with this id.
    pub fn unit(param: impl Into<UnitAddr>, from: f64, to: f64, duration: Samples) -> Self {
        Self::packed(ParamAddr::unit(param.into()), from, to, duration)
    }

    /// A segment of a id-addressed target's parameter `id`, in its own
    /// normalisation, as [`new`](Self::new) otherwise.
    pub fn by_id(id: ParamId, from: f64, to: f64, duration: Samples) -> Self {
        Self::packed(ParamAddr::Id(id), from, to, duration)
    }

    /// This segment along `shape`.
    #[must_use]
    pub fn law(mut self, shape: Law) -> Self {
        self.shape = shape;
        self
    }

    fn packed(addr: ParamAddr, from: f64, to: f64, duration: Samples) -> Self {
        Self {
            addr,
            from,
            to,
            position: 1,
            lead: SampleSpan::ZERO,
            span: SampleSpan::from_samples(Samples::new(saturate(duration) as usize)),
            shape: Law::default(),
        }
    }

    /// This segment, `position` frames along a `length`-frame span at the
    /// event's frame: for a producer that enters a segment part-way (a seek,
    /// a loop wrap, each block's re-send). See the type docs for the value
    /// each frame gets. A zero length is a step to `to`; either count
    /// saturates at `u32::MAX`.
    #[must_use]
    pub fn resumed(mut self, position: Samples, length: Samples) -> Self {
        self.position = saturate(position);
        self.lead = SampleSpan::ZERO;
        self.span = SampleSpan::from_samples(Samples::new(saturate(length) as usize));
        self
    }

    /// This segment, `since` frames along a `span`-frame piece at the event's
    /// frame, both fractional: for a producer whose piece starts and ends
    /// between frames. Only the whole frames of `since` are a count (a `u32`);
    /// the fraction is a phase, so every frame reads the law at its exact
    /// place on the piece: `t = (since + j) / span` at frame `j`. A negative
    /// `since` is 0, and a `span` past `u32::MAX` frames saturates.
    #[must_use]
    pub fn resumed_exactly(mut self, since: SamplePosition, span: SamplePosition) -> Self {
        let whole = since.to_samples_floor();
        self.position = saturate(whole);
        self.lead = SampleSpan::clamped(since.get() - whole.get() as f64);
        self.span = SampleSpan::clamped(span.get().min(f64::from(u32::MAX)));
        self
    }

    /// Which parameter this moves — for routing on the address.
    pub fn addr(&self) -> ParamAddr {
        self.addr
    }

    /// Whether this segment is for `addr`. No conversion: what the param
    /// port asks of every event, per event.
    #[inline]
    pub(crate) fn is_for(&self, addr: ParamAddr) -> bool {
        self.addr == addr
    }

    /// The end value, in `key`'s unit, if this segment is for `key`.
    pub fn target<U: Scalar>(&self, key: ParamKey<U>) -> Option<U> {
        self.is_for(ParamAddr::unit(key.addr()))
            .then(|| U::from_cell(self.to))
    }

    /// The start value, in `key`'s unit, if this segment is for `key`.
    pub fn start<U: Scalar>(&self, key: ParamKey<U>) -> Option<U> {
        self.is_for(ParamAddr::unit(key.addr()))
            .then(|| U::from_cell(self.from))
    }

    /// The raw end value, if this segment is for parameter `id`.
    pub fn target_by_id(&self, id: ParamId) -> Option<f64> {
        self.is_for(ParamAddr::Id(id)).then_some(self.to)
    }

    /// The raw start value, if this segment is for parameter `id`.
    pub fn start_by_id(&self, id: ParamId) -> Option<f64> {
        self.is_for(ParamAddr::Id(id)).then_some(self.from)
    }

    /// The shape it moves along.
    pub fn shape(&self) -> Law {
        self.shape
    }

    /// Its position along itself at the event's frame, in frames.
    pub fn position(&self) -> Samples {
        Samples::new(self.position as usize)
    }

    /// Its length, in frames.
    pub fn length(&self) -> Samples {
        SamplePosition::new(self.span.get()).to_samples_ceil()
    }

    /// The raw value `j` frames after the event's frame (`Samples::new(0)`: its
    /// own), whatever the unit: the value a param port gives this source
    /// there, for a node that takes point values (a hosted plugin's
    /// parameter) or a test. Past `u32::MAX` frames it reads the value
    /// there.
    pub fn raw_value_after(&self, j: Samples) -> f64 {
        segment_value(self.unit_param(), self.piece(), self.timing(), saturate(j))
    }

    /// How many frames after the event's frame the segment arrives: the
    /// first `j` at which [`raw_value_after`](Self::raw_value_after) is the
    /// end value (`Samples::new(0)` for a step). For a node that takes point values
    /// and must land the end value on its frame.
    pub fn arrives_after(&self) -> Samples {
        Samples::new(arrival(self.timing()) as usize)
    }

    /// The pure part of the segment: its endpoints and its law, as a
    /// [`Piece`]. A segment is this plus an address and a position
    /// ([`position`](Self::position), [`length`](Self::length)).
    pub fn piece(&self) -> Piece {
        Piece::new(self.from, self.to).law(self.shape)
    }

    /// The built-in param this moves, `None` for an id-addressed one: what decides
    /// whether it turns the short arc ([`UnitParam::is_circular`]). The
    /// reading of a segment between frames (`Piece::value(param, t)` at its
    /// progress) takes it; a reader of many frames takes it once.
    pub fn unit_param(&self) -> Option<UnitParam> {
        self.addr.as_unit().map(|a| a.param)
    }

    /// The raw position and length, for the graph's own param port, which is
    /// erased over the unit as the port it drives is.
    pub(crate) fn timing(&self) -> Timing {
        Timing {
            position: self.position,
            lead: self.lead,
            span: self.span,
        }
    }
}

/// A segment's place on itself at its event's frame: whole frames and a
/// fraction of one along, and its exact length in frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Timing {
    pub(crate) position: u32,
    pub(crate) lead: SampleSpan,
    pub(crate) span: SampleSpan,
}

/// A segment's value `j` frames after its event: `to` exactly once it
/// has arrived (see [`segment_progress`]), so a segment always lands
/// on its end value. The one definition the param port and
/// [`ParamSegment::raw_value_after`] share, as they share a shaping LUT.
#[inline]
pub(crate) fn segment_value(param: Option<UnitParam>, piece: Piece, timing: Timing, j: u32) -> f64 {
    match segment_progress(timing, j) {
        Some(t) => piece.value(param, t),
        None => piece.to,
    }
}

/// How many frames after its event a segment arrives: the first `j` at which
/// [`segment_progress`] is `None` (0 for a step). The one count the port's
/// corners and [`ParamSegment::arrives_after`] share.
pub(crate) fn arrival(t: Timing) -> u32 {
    let left = t.span.get() - (f64::from(t.position) + t.lead.get());
    if left <= 0.0 {
        return 0;
    }
    // At most `u32::MAX`: a span is.
    let j = SamplePosition::new(left).to_samples_ceil().get();
    u32::try_from(j).unwrap_or(u32::MAX)
}

/// A segment's progress `j` frames after its event, `None` once it has
/// arrived: at 1 or past, or a zero span (a step).
#[inline]
pub(crate) fn segment_progress(t: Timing, j: u32) -> Option<Normalized> {
    let (done, span) = (
        f64::from(t.position) + f64::from(j) + t.lead.get(),
        t.span.get(),
    );
    (done < span).then(|| Normalized::clamped(done / span))
}

/// What a [`ParamEvent`] says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamKind {
    /// A parameter automation ramp.
    Ramp(ParamRamp),
    /// A parameter curve segment: a shaped, absolute move a param port
    /// evaluates per frame (see [`ParamSegment`]).
    Segment(ParamSegment),
}

/// One param event on an event port: an offset into the current block, and
/// the ramp or segment that starts there.
///
/// It travels in its own buffer beside the port's [`Event`](crate::Event)s
/// (see the module docs): a node reads them with
/// [`SortedEvents::params`](crate::SortedEvents::params) and writes them with
/// [`EventWriter::push_param`](crate::EventWriter::push_param), sorted by
/// offset as events are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamEvent {
    /// Where in the block.
    pub offset: Offset,
    /// The payload.
    pub kind: ParamKind,
}

impl ParamEvent {
    /// A parameter ramp starting at `offset`.
    pub const fn ramp(offset: Offset, ramp: ParamRamp) -> Self {
        Self {
            offset,
            kind: ParamKind::Ramp(ramp),
        }
    }

    /// A parameter curve segment, its event frame at `offset`.
    pub const fn segment(offset: Offset, segment: ParamSegment) -> Self {
        Self {
            offset,
            kind: ParamKind::Segment(segment),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_curve::{Curvature, Exponent};
    use tutti_units::{Hz, ParamKey};

    /// An id-addressed ramp and a built-in ramp never answer for each other, even
    /// when the id equals the built-in's discriminant.
    #[test]
    fn id_addressed_and_builtin_ramps_are_distinct_addresses() {
        let by_id = ParamRamp::by_id(
            ParamId::new(ParamKey::<Hz>::CUTOFF.addr().to_u16() as u32),
            0.5,
            Samples::new(0),
        );
        assert_eq!(by_id.target(ParamKey::<Hz>::CUTOFF), None);
        assert_eq!(by_id.target_by_id(ParamId::new(0)), Some(0.5));
        let builtin = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz::new(100.0), Samples::new(0));
        assert_eq!(builtin.target_by_id(ParamId::new(0)), None);
    }

    /// A segment's address round-trips, built-in and id-addressed never answer
    /// for each other, and its shape and timing read back as built.
    #[test]
    fn a_segment_reads_back_as_built() {
        let key = ParamKey::<Hz>::CUTOFF;
        let id = ParamId::new(u32::from(key.addr().to_u16()));
        let by_id =
            ParamSegment::by_id(id, 0.25, 0.75, Samples::new(8)).law(Law::Exp(Curvature::of(2.0)));
        assert_eq!(by_id.target(key), None);
        assert_eq!(by_id.target_by_id(id), Some(0.75));
        assert_eq!(by_id.start_by_id(id), Some(0.25));
        assert_eq!(by_id.addr(), ParamAddr::Id(id));
        let s = ParamSegment::new(key, Hz::new(1.0), Hz::new(2.0), Samples::new(8))
            .law(Law::PowerOut(Exponent::of(3.0)))
            .resumed(Samples::new(2), Samples::new(10));
        assert_eq!(s.addr(), ParamAddr::unit(key.addr()));
        assert_eq!(s.target_by_id(id), None);
        assert_eq!(s.start(key), Some(Hz::new(1.0)));
        assert_eq!(s.shape(), Law::PowerOut(Exponent::of(3.0)));
        assert_eq!(s.position(), Samples::new(2));
        assert_eq!(s.length(), Samples::new(10));
        // 2 frames along at its event, 8 more to go: the end value lands on
        // frame 8, held after.
        assert!(s.raw_value_after(Samples::new(7)) < 2.0);
        assert_eq!(s.raw_value_after(Samples::new(8)), 2.0);
        assert_eq!(s.raw_value_after(Samples::new(1_000)), 2.0);
        assert_eq!(s.arrives_after(), Samples::new(8));
        // A zero length is a step.
        let step =
            ParamSegment::new(key, Hz::new(1.0), Hz::new(2.0), Samples::new(0)).law(Law::Hold);
        assert_eq!(step.raw_value_after(Samples::new(0)), 2.0);
        assert_eq!(step.arrives_after(), Samples::new(0));
        // The pure part: endpoints and law (a law with no parameter,
        // `Smootherstep`, among them).
        let smooth = ParamSegment::new(key, Hz::new(1.0), Hz::new(2.0), Samples::new(8))
            .law(Law::Smootherstep);
        assert_eq!(smooth.piece(), Piece::new(1.0, 2.0).law(Law::Smootherstep));
        assert_eq!(
            s.piece(),
            Piece::new(1.0, 2.0).law(Law::PowerOut(Exponent::of(3.0)))
        );
    }

    /// Counts past `u32::MAX` saturate, never wrap: the segment stays as long
    /// as the type can say.
    #[test]
    fn counts_saturate_at_u32_max() {
        let key = ParamKey::<Hz>::CUTOFF;
        let s = ParamSegment::new(key, Hz::new(0.0), Hz::new(1.0), Samples::new(usize::MAX));
        assert_eq!(s.length(), Samples::new(u32::MAX as usize));
        let r = ParamRamp::new(key, Hz::new(1.0), Samples::new(usize::MAX));
        assert_eq!(r.duration(), Samples::new(u32::MAX as usize));
    }

    /// A ten-minute segment (2.88e7 frames at 48 kHz, past the 2^24 frames an
    /// `f32` count holds) advances exactly one frame per frame at its end: the
    /// slope of a ramp of one unit a frame is 1 at every frame. A progress in
    /// `f32` would quantise to steps of 2 there.
    #[test]
    fn a_ten_minute_segment_progresses_one_frame_per_frame_at_its_end() {
        const FRAMES: usize = 28_800_000;
        let key = ParamKey::<Hz>::CUTOFF;
        // One Hz a frame, so the value after `j` frames from `position` 0 is
        // `j`, exact in `f64`.
        let s = ParamSegment::new(
            key,
            Hz::new(0.0),
            Hz::new(FRAMES as f64),
            Samples::new(FRAMES),
        )
        .resumed(Samples::new(0), Samples::new(FRAMES));
        // The loss `f32` would take, so the test cannot pass vacuously.
        let odd = FRAMES - 1;
        assert_ne!(f64::from(odd as f32), odd as f64);
        for j in FRAMES - 8..FRAMES - 1 {
            let step = s.raw_value_after(Samples::new(j + 1)) - s.raw_value_after(Samples::new(j));
            assert!((step - 1.0).abs() < 1e-7, "frame {j}: step {step}");
        }
        assert_eq!(s.raw_value_after(Samples::new(FRAMES)), FRAMES as f64);
        assert_eq!(s.arrives_after(), Samples::new(FRAMES));
    }

    /// **A piece that starts between frames evaluates its law at each frame's
    /// exact place.** An exponential piece entered 0.6 of a frame along, 10.4
    /// frames long: frame `j` reads the law at `(0.6 + j) / 11`, which a phase
    /// dropped (entered at frame 0 of a span of 11) does not.
    #[test]
    fn a_fractional_start_reads_the_law_at_the_exact_place() {
        let law = Law::Exp(Curvature::of(4.0));
        let key = ParamKey::<Hz>::CUTOFF;
        let base = ParamSegment::new(key, Hz::new(0.0), Hz::new(8.0), Samples::new(11)).law(law);
        let exact = base.resumed_exactly(SamplePosition::new(0.6), SamplePosition::new(11.0));
        let flat = base.resumed(Samples::new(0), Samples::new(11));
        let mut apart = 0.0f64;
        for j in 0..10 {
            let t = (0.6 + j as f64) / 11.0;
            let want = 8.0 * law.eval(Normalized::of(t)).get();
            let got = exact.raw_value_after(Samples::new(j));
            assert!((got - want).abs() < 1e-12, "frame {j}: {got} vs {want}");
            apart = apart.max((flat.raw_value_after(Samples::new(j)) - want).abs());
        }
        assert!(apart > 1e-2, "the phase matters here: {apart}");
        // It arrives on the first frame at or past its end, 10.4 on.
        assert_eq!(exact.arrives_after(), Samples::new(11));
    }

    /// The endpoints are held as `f64`: a value `f32` cannot hold reads back
    /// whole, and renders at its own width.
    #[test]
    fn endpoints_keep_their_f64_width() {
        let key = ParamKey::<Hz>::CUTOFF;
        let from = 1.0 + 1e-9;
        let to = 3.0 - 1e-9;
        // Setup: both round in `f32`.
        assert_eq!(f64::from(from as f32), 1.0);
        assert_eq!(f64::from(to as f32), 3.0);
        let s = ParamSegment::new(key, Hz::new(from), Hz::new(to), Samples::new(4));
        assert_eq!(s.start(key), Some(Hz::new(from)));
        assert_eq!(s.target(key), Some(Hz::new(to)));
        assert_eq!(s.raw_value_after(Samples::new(3)), to);
    }
}
