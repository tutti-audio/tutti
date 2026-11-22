//! Compiler-owned parameter ports.
//!
//! A node declares the params a graph may drive
//! ([`Shape::params`](crate::Shape::params), by [`UnitAddr`]: a [`UnitParam`]
//! kind and an index among the node's params of that kind). A
//! [`GraphSpec`](crate::GraphSpec) can then drive any of them from another
//! node's event output ([`GraphSpec::connect_param`](crate::GraphSpec::connect_param)),
//! and the compiler turns each driven param into one fused step of the
//! node's op:
//!
//! ```text
//!   value[i] = constrain(held[i], range)   (clamp; wrap for a circular param)
//!   held[i]  = the event source's value, from its first event on
//!            = base_ramp[i], before that
//! ```
//!
//! A circular param ([`UnitParam::is_circular`], `Azimuth`) is wrapped onto
//! `-180..180`, never clamped, and its moves take the short arc. **The port's
//! frames jump by 360 where a move crosses the seam**: a ramp from 170 to
//! -170 reads ..., 179.9, -179.9, ... a step of about 360 between neighbouring
//! frames though the bearing moved a fraction of a degree. A consumer that
//! smooths or interpolates the value must ramp on the arc
//! ([`UnitParam::arc`], [`Azimuth::shortest_arc_to`]) and wrap on read, not
//! subtract frames.
//!
//! An event source **replaces** the base: a port takes at
//! most one. Modulation (offsets, depth, LFOs) is not the graph's: a
//! control lane sums it and sends the value.
//!
//! The rules are documented on
//! [`GraphSpec::connect_param`](crate::GraphSpec::connect_param).
//!
//! The step is fused into the node op rather than being an op of its own:
//! its only output is the per-frame values the node reads in the same op, so
//! a separate op would need a slot, an ordering edge and a verifier rule for
//! a buffer nothing else can read. Its source is a read of the node op, so
//! the colouring and the verifier cover it like any input.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::NodeKey;
use crate::mode::NodeMix;
use tutti_buffers::Float;
use tutti_rt::{Publisher, Reader, RtScope};
use tutti_units::{
    ArcDegrees, Azimuth, Normalized, ParamAddr, Quantity, Samples, UnitAddr, UnitParam,
};

use crate::lane::{arc_in, control_in, fade_gains, ramp_in, wrap_in};
use crate::param_event::{ParamEvent, ParamKind, Timing, arrival, segment_progress};
use crate::spec::EventOut;
use tutti_curve::{FadeLaw, Law, Piece};

/// Most params one node can declare modulatable: 32 addresses.
///
/// Sized for a multiband dynamics node: four bands of six params (threshold,
/// knee, ratio, attack, release, make-up) and three crossover frequencies is
/// 27. Everything sized from it is stack or plan data of a fixed few words per
/// port: the [`ParamPorts`] list (two bytes an entry, so [`Shape`](crate::Shape)
/// stays `Copy`), the per-block input array, and the executor's view-holding
/// bound (`par.rs`). 32 is also the most `[bool; N]: Default` allows, which
/// `ParamState` derives.
pub const MAX_PARAM_PORTS: usize = 32;

/// The params a node lets the graph modulate, in **port order** — the index
/// [`Io::param`](crate::Io::param) and [`Node::param_base`](crate::Node::param_base)
/// take. A fixed-capacity list, so [`Shape`](crate::Shape) stays `Copy`.
///
/// Each is a [`UnitAddr`], so a node with several params of one kind (a
/// threshold per band) declares each at its own index. Two bytes per entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParamPorts {
    len: u8,
    ids: [UnitAddr; MAX_PARAM_PORTS],
}

impl ParamPorts {
    /// No modulatable params.
    pub const NONE: Self = Self {
        len: 0,
        ids: [UnitAddr::first(UnitParam::Cutoff); MAX_PARAM_PORTS],
    };

    /// The one port of a node's mix lane, [`NodeMix::ADDR`]: what only the
    /// graph may list (a node's own list refuses it).
    pub(crate) const MIX: Self = Self {
        len: 1,
        ids: [NodeMix::ADDR; MAX_PARAM_PORTS],
    };

    /// `ids`, each at index 0, in this order. A node with several params of a
    /// kind lists them with [`of`](Self::of).
    ///
    /// # Panics
    ///
    /// Where [`try_new`](Self::try_new) returns an error (in a `const`
    /// context, at compile time — where a node's list usually is).
    #[must_use]
    pub const fn new(ids: &[UnitParam]) -> Self {
        Self::unwrap(Self::try_new(ids))
    }

    /// `addrs`, in this order: [`new`](Self::new) for params at any index.
    ///
    /// # Panics
    ///
    /// Where [`try_of`](Self::try_of) returns an error.
    #[must_use]
    pub const fn of(addrs: &[UnitAddr]) -> Self {
        Self::unwrap(Self::try_of(addrs))
    }

    const fn unwrap(r: Result<Self, ParamPortsError>) -> Self {
        match r {
            Ok(p) => p,
            Err(ParamPortsError::TooMany { .. }) => {
                panic!("more modulatable params than MAX_PARAM_PORTS")
            }
            Err(ParamPortsError::Duplicate(_)) => panic!("a param is declared modulatable twice"),
            Err(ParamPortsError::Reserved(_)) => {
                panic!("a param is declared at an address the graph reserves")
            }
        }
    }

    /// [`new`](Self::new), without the panic. Widens each kind to its index-0
    /// address and calls [`try_of`](Self::try_of) (a `const fn` cannot map a
    /// slice).
    ///
    /// # Errors
    ///
    /// [`ParamPortsError::TooMany`] for a list of more than
    /// [`MAX_PARAM_PORTS`], [`ParamPortsError::Duplicate`] for one naming a
    /// param twice.
    pub const fn try_new(ids: &[UnitParam]) -> Result<Self, ParamPortsError> {
        if ids.len() > MAX_PARAM_PORTS {
            return Err(ParamPortsError::TooMany { count: ids.len() });
        }
        let mut addrs = Self::NONE.ids;
        let mut i = 0;
        while i < ids.len() {
            addrs[i] = UnitAddr::first(ids[i]);
            i += 1;
        }
        let (head, _) = addrs.split_at(ids.len());
        Self::try_of(head)
    }

    /// [`of`](Self::of), without the panic.
    ///
    /// # Errors
    ///
    /// [`ParamPortsError::TooMany`] for a list of more than
    /// [`MAX_PARAM_PORTS`], [`ParamPortsError::Duplicate`] for one naming an
    /// address (kind **and** index) twice: the same kind at another index is
    /// another param, and [`ParamPortsError::Reserved`] for
    /// [`NodeMix::ADDR`], the address a node's mix is driven at.
    pub const fn try_of(addrs: &[UnitAddr]) -> Result<Self, ParamPortsError> {
        if addrs.len() > MAX_PARAM_PORTS {
            return Err(ParamPortsError::TooMany { count: addrs.len() });
        }
        let mut out = Self::NONE;
        let mut i = 0;
        while i < addrs.len() {
            let mut j = 0;
            while j < i {
                if addrs[j].to_u16() == addrs[i].to_u16() {
                    return Err(ParamPortsError::Duplicate(addrs[i]));
                }
                j += 1;
            }
            if addrs[i].to_u16() == NodeMix::ADDR.to_u16() {
                return Err(ParamPortsError::Reserved(addrs[i]));
            }
            out.ids[i] = addrs[i];
            i += 1;
        }
        out.len = addrs.len() as u8;
        Ok(out)
    }

    /// The params, in port order.
    #[must_use]
    pub fn as_slice(&self) -> &[UnitAddr] {
        &self.ids[..self.len as usize]
    }

    /// How many.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether there are none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The port index of `param`, if it is declared. A bare [`UnitParam`] is
    /// its index-0 address.
    pub fn index_of(&self, param: impl Into<UnitAddr>) -> Option<usize> {
        let param = param.into();
        self.as_slice().iter().position(|&p| p == param)
    }
}

/// Why a list cannot be a node's [`ParamPorts`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParamPortsError {
    /// More params than [`MAX_PARAM_PORTS`].
    #[error("{count} modulatable params past a node's {}", MAX_PARAM_PORTS)]
    TooMany {
        /// How many were listed.
        count: usize,
    },
    /// An address (kind and index) listed twice.
    #[error("{0:?} is declared modulatable twice")]
    Duplicate(UnitAddr),
    /// [`NodeMix::ADDR`]: the graph's own, which a node's mix is driven at.
    #[error("{0:?} is reserved for a node's mix")]
    Reserved(UnitAddr),
}

impl Default for ParamPorts {
    fn default() -> Self {
        Self::NONE
    }
}

/// A modulatable param of a node: the sink of a param edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ParamIn {
    /// The node.
    pub node: NodeKey,
    /// Which of its declared params: a bare [`UnitParam`] converts to its
    /// index-0 address (`param.into()`).
    pub param: UnitAddr,
}

/// A param cell's value with its unit erased, as a quantity: what
/// [`Node::param_base`](crate::Node::param_base) says, which crosses into a
/// lane's data with `F::from_unit`. The crossing for a unit-erased cell value,
/// inside this crate only.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CellValue(pub(crate) f64);

impl Quantity for CellValue {
    fn value(self) -> f64 {
        self.0
    }
}

/// The range a modulated param is held to, in the param's own units: clamped
/// for a line, an arc of the circle for a circular param
/// ([`arc`](Self::arc)).
///
/// Bare `f64` bounds, the unit's own width, not a unit type: a param port is
/// erased over its unit (a cutoff is `Hz`, a threshold `Db`), as the
/// `ParamRamp` wire is. Crossed bounds
/// are ordered where they are applied, never a panic; a NaN bound never
/// reaches the audio thread
/// ([`GraphInvalid::BadParamRange`](crate::GraphInvalid::BadParamRange)).
/// Compared and hashed by bits, so a spec holding one stays `Eq + Hash`.
/// Sixteen bytes, so a port's live range is published whole through a
/// [`ParamRangeCell`] and a reader sees one publish's `min` and `max`
/// together.
#[derive(Clone, Copy, Debug)]
pub struct ParamRange {
    /// The lowest value.
    pub min: f64,
    /// The highest value.
    pub max: f64,
}

impl ParamRange {
    /// No clamp at all.
    pub const UNBOUNDED: Self = Self {
        min: f64::NEG_INFINITY,
        max: f64::INFINITY,
    };

    /// `min..=max`. For a circular param ([`UnitParam::is_circular`]) the
    /// bounds read as an arc, `min` to `max` counter-clockwise, and `min` must
    /// not exceed `max` (a crossed range is refused,
    /// [`GraphInvalid::BadParamRange`](crate::GraphInvalid::BadParamRange));
    /// [`arc`](Self::arc) writes one by its centre.
    #[must_use]
    pub const fn new(min: f64, max: f64) -> Self {
        Self { min, max }
    }

    /// The arc of the circle `half_span` either side of `centre`: the range
    /// of a circular param ([`UnitParam::is_circular`], `Azimuth`). It is the
    /// only way to write a bounded range of one: `new(170.0, -170.0)` would
    /// be ordered into `-170..170`, the opposite arc, and a circular param's
    /// port refuses a crossed range
    /// ([`GraphInvalid::BadParamRange`](crate::GraphInvalid::BadParamRange)).
    ///
    /// `half_span` is taken as its magnitude. The stored bounds are
    /// `centre ∓ half_span`, not wrapped, so an arc across ±180° has
    /// `min <= max` like any range; a half-span of 180° or more is the whole
    /// circle.
    ///
    /// ```
    /// use tutti_graph::ParamRange;
    /// use tutti_units::{ArcDegrees, Azimuth, UnitParam};
    ///
    /// // 20 degrees either side of straight behind.
    /// let behind = ParamRange::arc(Azimuth::new(180.0), ArcDegrees::new(20.0));
    /// let view = behind.view(Some(UnitParam::Azimuth));
    /// assert!(view.contains(180.0) && view.contains(-170.0));
    /// assert!(!view.contains(0.0));
    /// ```
    #[must_use]
    pub fn arc(centre: Azimuth, half_span: ArcDegrees) -> Self {
        let half = half_span.get().abs();
        Self {
            min: centre.get() - half,
            max: centre.get() + half,
        }
    }

    /// The bounds, ordered so `clamp` cannot panic.
    #[inline]
    #[must_use]
    pub fn ordered(self) -> (f64, f64) {
        (self.min.min(self.max), self.max.max(self.min))
    }

    /// The range as `param` reads it, resolved once: the form a per-block
    /// loop constrains by ([`RangeView::constrain`]). `None` is a param of
    /// no tutti unit (a plugin's), which is a line.
    #[must_use]
    pub fn view(self, param: Option<UnitParam>) -> RangeView {
        let (lo, hi) = self.ordered();
        if !param.is_some_and(UnitParam::is_circular) {
            return RangeView::Line { lo, hi };
        }
        let half = (hi - lo) * 0.5;
        if !(lo.is_finite() && hi.is_finite()) || half >= 180.0 {
            return RangeView::Circle;
        }
        RangeView::Arc {
            centre: Azimuth::new((lo + hi) * 0.5),
            half: ArcDegrees::new(half),
        }
    }

    /// Whether `param` may hold this range: a circular param's bounds are in
    /// order (see [`arc`](Self::arc)); a line orders them, so any range fits.
    #[must_use]
    pub(crate) fn fits(self, param: UnitParam) -> bool {
        !param.is_circular() || self.min <= self.max
    }
}

/// A [`ParamRange`] as a param reads it ([`ParamRange::view`]): the one form
/// a block's loop constrains values by, with the arc's centre and half-span
/// worked out once.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum RangeView {
    /// A line: values are clamped to `lo..=hi`.
    Line {
        /// The lowest value.
        lo: f64,
        /// The highest value.
        hi: f64,
    },
    /// The whole circle: values are wrapped onto `-180..180`.
    Circle,
    /// An arc of the circle: values are wrapped, then held to the arc.
    Arc {
        /// The arc's middle (not wrapped).
        centre: Azimuth,
        /// Its half-span, in `0..180` degrees (0 included: a single bearing).
        half: ArcDegrees,
    },
}

impl RangeView {
    /// How far the range reaches: `hi - lo` for a line (infinite when
    /// unbounded), the arc's length for an arc, 360 for the whole circle. What
    /// a modulation depth scales by.
    #[must_use]
    pub fn span(self) -> f64 {
        match self {
            Self::Line { lo, hi } => hi - lo,
            Self::Circle => 360.0,
            Self::Arc { half, .. } => (half * 2.0).get(),
        }
    }

    /// `value` held to the range: clamped for a line; wrapped onto the circle
    /// (`-180..180`) for the whole circle; wrapped and held to the nearer end
    /// of an arc, by the short way round.
    #[inline]
    #[must_use]
    pub fn constrain<F: Float>(self, value: F) -> F {
        match self {
            Self::Line { lo, hi } => {
                value.clamp(F::from_unit(CellValue(lo)), F::from_unit(CellValue(hi)))
            }
            Self::Circle => wrap_in(UnitParam::Azimuth, value),
            Self::Arc { centre, half } => {
                // The arc is in the unit's `f64`; the lane value widens to it
                // and the result narrows once.
                let d = centre.shortest_arc_to(Azimuth::new(value.into()));
                F::from_unit(centre.rotate_by(d.clamp(-half, half)))
            }
        }
    }

    /// [`constrain`](Self::constrain) over a lane: the line's bounds are
    /// read into `F` once, not per value.
    #[inline]
    pub(crate) fn constrain_lane<F: Float>(self, lane: &mut [F]) {
        if let Self::Line { lo, hi } = self {
            let (lo, hi) = (F::from_unit(CellValue(lo)), F::from_unit(CellValue(hi)));
            for v in lane {
                *v = v.clamp(lo, hi);
            }
        } else {
            for v in lane {
                *v = self.constrain(*v);
            }
        }
    }

    /// Whether `value` is within the range: between the bounds for a line, on
    /// the arc for an arc, and any finite bearing for the whole circle.
    #[must_use]
    pub fn contains(self, value: f64) -> bool {
        match self {
            Self::Line { lo, hi } => (lo..=hi).contains(&value),
            Self::Circle => value.is_finite(),
            Self::Arc { centre, half } => {
                let d = centre.shortest_arc_to(Azimuth::new(value));
                d >= -half && d <= half
            }
        }
    }
}

impl Default for ParamRange {
    fn default() -> Self {
        Self::UNBOUNDED
    }
}

impl PartialEq for ParamRange {
    fn eq(&self, other: &Self) -> bool {
        self.min.to_bits() == other.min.to_bits() && self.max.to_bits() == other.max.to_bits()
    }
}

impl Eq for ParamRange {}

impl Hash for ParamRange {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.min.to_bits().hash(state);
        self.max.to_bits().hash(state);
    }
}

/// How one param port is driven: its range, and its event source. No
/// source is the same as no entry: the port reads its base.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ParamMod {
    /// What the value is clamped to.
    pub range: ParamRange,
    /// The event output whose value replaces the base.
    pub source: Option<EventOut>,
}

/// What a node reads for one of its params this block
/// ([`Io::param`](crate::Io::param)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamInput<'a, F: Float = f32> {
    /// Nothing modulates it: use the param's own control (the base), as the
    /// node would with no graph around it. The fast path.
    Base,
    /// The param's value at each frame of the block, and its corners. A
    /// circular param's values are wrapped, so they step by about 360 across
    /// the seam (see the module docs).
    Frames {
        /// The value at each frame.
        values: &'a [F],
        /// The block offsets where the delivered trajectory changes piece,
        /// ascending, at most one per frame (see [`corners`](Self::corners)).
        corners: &'a [u32],
    },
}

impl<'a, F: Float> ParamInput<'a, F> {
    /// The value on the block's **last** frame, when modulated: what a
    /// block-rate control reads, and where a block-rate cell's own ramp ends.
    /// `None`: nothing modulates it, read the cell.
    #[inline]
    #[must_use]
    pub fn last_frame(self) -> Option<F> {
        self.frames().and_then(|v| v.last().copied())
    }

    /// The per-frame values, when modulated.
    #[inline]
    #[must_use]
    pub fn frames(self) -> Option<&'a [F]> {
        match self {
            Self::Base => None,
            Self::Frames { values, .. } => Some(values),
        }
    }

    /// The block offsets, ascending, where the trajectory its event source
    /// delivered **changes piece**:
    ///
    /// - a ramp event's frame;
    /// - a segment event's frame, unless it continues the piece in flight:
    ///   the same `from`, `to` and law, bit for bit, arriving on the same
    ///   frame (a source's resumed re-send, a resync), which is no corner, so
    ///   corners do not depend on where blocks or commits fell;
    /// - the frame a ramp or segment in flight arrives on (from there it
    ///   holds its end).
    ///
    /// Read after the port's delay, as the values are. A base's ramp, a
    /// connection's declick and the range clamp make none. Empty when nothing
    /// modulates it. At most one per frame, so never more than the block's
    /// length.
    #[inline]
    #[must_use]
    pub fn corners(self) -> &'a [u32] {
        match self {
            Self::Base => &[],
            Self::Frames { corners, .. } => corners,
        }
    }
}

/// A block's corners as the port writes them: at most one per frame, so a
/// buffer the block's length never overflows.
pub(crate) struct Corners<'a> {
    buf: &'a mut [u32],
    len: usize,
}

impl<'a> Corners<'a> {
    /// Writes into `buf`, at least as long as the block.
    pub(crate) fn new(buf: &'a mut [u32]) -> Self {
        Self { buf, len: 0 }
    }

    /// A corner at block offset `i`, at or after the last one.
    #[inline]
    fn mark(&mut self, i: usize) {
        // A block offset is below the block's length, at most `MaxBlock::MAX`
        // (`u32::MAX` frames), so it fits.
        let i = u32::try_from(i).expect("a block offset is below MaxBlock::MAX, a u32");
        if self.len == 0 || self.buf[self.len - 1] != i {
            self.buf[self.len] = i;
            self.len += 1;
        }
    }

    /// How many were written.
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

/// A curve segment's timing, as a source's running state keeps it (see
/// [`ParamSegment`](crate::ParamSegment)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Curve {
    /// Its place along itself at its event's frame, and its length.
    pub(crate) timing: Timing,
}

/// One event source's ramp: its value, and the move it is on — a
/// [`ParamRamp`](crate::ParamRamp) along its shape from the value it had, or
/// a curve segment between its own endpoints. The move is the unit's `f64`
/// quantity throughout; a value crosses to the lane's float once, where it is
/// written (`lane::cross`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Ramp {
    value: f64,
    from: f64,
    target: f64,
    /// A ramp's length, in frames.
    len: u32,
    /// A ramp's shape.
    shape: Law,
    /// Frames done: a ramp's steps taken, a segment's frames since its
    /// event's.
    done: u32,
    active: bool,
    /// `Some` while the move is a curve segment.
    curve: Option<Curve>,
    /// Whether the source has sent its first event: from then on its value
    /// replaces the port's base.
    started: bool,
    /// Frames after its event the move arrives on.
    arrive: u32,
    /// The param it moves, set by [`render_ramps`]: a circular one takes the
    /// short arc.
    param: Option<UnitParam>,
}

impl Ramp {
    /// Whether a segment of `piece` and `curve` on this frame continues the
    /// move in flight: the same piece, bit for bit, arriving on the same
    /// frame (or both at rest on it).
    fn continues(&self, piece: Piece, curve: Curve) -> bool {
        let bits = |from: f64, to: f64, law: Law| (from.to_bits(), to.to_bits(), law);
        if !self.started
            || self.curve.is_none()
            || bits(self.from, self.target, self.shape) != bits(piece.from, piece.to, piece.law)
        {
            return false;
        }
        let arrive = arrival(curve.timing);
        if self.active {
            self.arrive.checked_sub(self.done) == Some(arrive)
        } else {
            arrive == 0
        }
    }

    /// Start a ramp to `target` along `shape` over `len` frames from this
    /// frame on, from the value the last frame had. A second ramp on the
    /// same frame replaces the first, from the same value.
    fn start(&mut self, target: f64, len: usize, shape: Law) {
        self.from = self.value;
        self.target = target;
        self.len = u32::try_from(len).unwrap_or(u32::MAX);
        self.shape = shape;
        self.done = 0;
        self.active = true;
        self.curve = None;
        self.started = true;
        self.arrive = self.len.saturating_sub(1);
    }

    /// Start a curve segment from this frame on: its own endpoints, so the
    /// value it interrupts is not read. A second event on the same frame
    /// replaces it.
    fn start_segment(&mut self, piece: Piece, curve: Curve) {
        self.from = piece.from;
        self.target = piece.to;
        self.shape = piece.law;
        self.len = 0;
        self.done = 0;
        self.active = true;
        self.curve = Some(curve);
        self.started = true;
        self.arrive = arrival(curve.timing);
    }

    /// The move's value at progress `t`: along the short arc for a circular
    /// param.
    #[inline]
    fn at(&self, t: Normalized) -> f64 {
        Piece::new(self.from, self.target)
            .law(self.shape)
            .value(self.param, t)
    }

    /// The value at this frame, after any move started on it. A ramp: its
    /// shape at `done / len` (`done` counting this frame), the target from
    /// its last step (a zero-length ramp reaches it on its first frame). A segment: its shape at its
    /// progress this frame, the target once it has arrived.
    #[inline]
    fn step(&mut self) -> f64 {
        if self.active {
            if let Some(c) = self.curve {
                // `segment_value`'s arithmetic, stepped: arrived, it holds
                // the target as a finished ramp does.
                match segment_progress(c.timing, self.done) {
                    Some(t) => {
                        self.value = self.at(t);
                    }
                    None => {
                        self.value = self.target;
                        self.active = false;
                    }
                }
                self.done = self.done.saturating_add(1);
            } else {
                self.done += 1;
                self.value = if self.done >= self.len {
                    self.active = false;
                    self.target
                } else {
                    // `done < len`, so `t` is in `(0, 1)`, where
                    // `Linear` evaluates to `t` itself.
                    let t = Normalized::progress(
                        Samples::new(self.done as usize),
                        Samples::new(self.len as usize),
                    );
                    self.at(t)
                };
            }
        }
        self.value
    }
}

/// Render one event source's values for a block into `out`: the ramps and
/// curve segments its events for `param` start, frame by frame, and
/// `base(i)` on each frame before its first event ever. A first ramp starts
/// from `base` at its own frame, whether or not a segment came before it
/// on that frame.
pub(crate) fn render_ramps<F: Float>(
    ramp: &mut Ramp,
    events: &[ParamEvent],
    param: UnitAddr,
    out: &mut [F],
    base: impl Fn(usize) -> F,
    corners: &mut Corners<'_>,
) {
    let want = ParamAddr::unit(param);
    ramp.param = Some(param.param);
    let mut next = events.iter().peekable();
    for (i, o) in out.iter_mut().enumerate() {
        // A re-send of a piece already at rest arrives again on its own
        // frame, which is no corner.
        let mut at_rest = false;
        while let Some(e) = next.next_if(|e| e.offset.index() <= i) {
            match e.kind {
                ParamKind::Ramp(r) if r.addr() == want => {
                    if !ramp.started {
                        ramp.value = base(i).into();
                    }
                    ramp.start(r.raw_target(), r.duration().get(), r.shape());
                    corners.mark(i);
                }
                ParamKind::Segment(s) if s.is_for(want) => {
                    // A segment never reads the value it interrupts, but a
                    // ramp that replaces it on this frame does: a first one
                    // starts from the base, as it would alone.
                    if !ramp.started {
                        ramp.value = base(i).into();
                    }
                    let curve = Curve { timing: s.timing() };
                    // A re-send of the piece in flight is no corner; the
                    // port still takes it, as it always has.
                    let continues = ramp.continues(s.piece(), curve);
                    at_rest = continues && !ramp.active;
                    ramp.start_segment(s.piece(), curve);
                    if !continues {
                        corners.mark(i);
                    }
                }
                // Another param's: a port re-sends nothing.
                ParamKind::Ramp(_) | ParamKind::Segment(_) => {}
            }
        }
        *o = if ramp.started {
            let moving = ramp.active;
            let v = ramp.step();
            if moving && !ramp.active && !at_rest {
                // It arrived on this frame and holds its end from here.
                corners.mark(i);
            }
            control_in(v)
        } else {
            base(i)
        };
    }
}

/// The live range of each of one key's declared param ports: what the fused
/// step clamps a modulated value to, lent once per block.
///
/// A range is a control, as a base is: written on the control side
/// ([`Editor::set_param_range`](crate::Editor::set_param_range), and every
/// commit from the spec), read by the audio thread at the start of the
/// port's step, so a move lands on the next block and costs a publish, not a
/// plan. The spec's range is its initial value; the plan carries no range.
///
/// One [`Publisher`] per port: both bounds are published whole, so a reader
/// never pairs one publish's `min` with another's `max` (two atomics could).
/// Each is an `Arc`, so a reader outside the graph (a control lane) reads
/// the very cell the port clamps to through a [`ParamRangeCell`].
///
/// Kept by the editor per key, and read by the state of each unit placed
/// there that declares the same params (through readers registered on the
/// control side, [`readers`](Self::readers)): a crossfade keeps the key's
/// running state, and that state reads these cells. A unit declaring a
/// different list gets fresh cells, so the unit it replaces never reads a
/// range meant for another param. The editor sweeps them with
/// [`reclaim`](Self::reclaim).
#[derive(Debug)]
pub(crate) struct ParamRanges {
    declared: ParamPorts,
    cells: Box<[ParamRangeCell]>,
}

impl ParamRanges {
    /// Unbounded cells for `declared`. Control side: allocates.
    pub(crate) fn new(declared: ParamPorts) -> Arc<Self> {
        Arc::new(Self {
            declared,
            cells: (0..declared.len())
                .map(|_| ParamRangeCell(Arc::new(Publisher::new(ParamRange::UNBOUNDED))))
                .collect(),
        })
    }

    /// The params the cells are for, in port order.
    pub(crate) fn declared(&self) -> ParamPorts {
        self.declared
    }

    /// A read-only handle on `param`'s cell, if it is one of these ports.
    /// Control side.
    pub(crate) fn cell(&self, param: UnitAddr) -> Option<ParamRangeCell> {
        self.cells.get(self.declared.index_of(param)?).cloned()
    }

    /// One reader per port, in port order, for a unit's param state.
    /// Control side: registers and allocates.
    pub(crate) fn readers(&self) -> Vec<ParamRangeReader> {
        self.cells.iter().map(ParamRangeCell::reader).collect()
    }

    /// Publishes `range` as `param`'s range when it differs from the one
    /// published; `false` if `param` is not one of these ports. Control
    /// side.
    pub(crate) fn store(&self, param: UnitAddr, range: ParamRange) -> bool {
        let Some(cell) = self
            .declared
            .index_of(param)
            .and_then(|k| self.cells.get(k))
        else {
            return false;
        };
        if cell.current() != range {
            cell.0.publish(range);
        }
        true
    }

    /// Frees the ranges the audio thread has let go of; how many are still
    /// held by a block. Control side.
    pub(crate) fn reclaim(&self) -> usize {
        self.cells.iter().map(|c| c.0.reclaim()).sum()
    }
}

/// A read-only handle on one param port's live range cell, for the control
/// side: a control lane's source for the span it scales by and, through
/// [`reader`](Self::reader), the reader its node lends once per block. Handed
/// out by [`Editor::param_range_cell`](crate::Editor::param_range_cell).
///
/// Read-only on purpose: the spec is the range's one writer
/// ([`Editor::set_param_range`](crate::Editor::set_param_range) stores into
/// the spec and this cell together, and every commit rewrites the cell from
/// the spec), so a publish that went to the cell alone would be undone by the
/// next commit.
#[derive(Clone, Debug)]
pub struct ParamRangeCell(Arc<Publisher<ParamRange>>);

impl ParamRangeCell {
    /// The range now: both bounds as one publish left them. Control side
    /// (it locks); the audio thread reads through a
    /// [`ParamRangeReader`].
    #[must_use]
    pub fn current(&self) -> ParamRange {
        *self.0.current()
    }

    /// A reader of the cell, registered here for the thread that will lend
    /// it. Control side: allocates.
    pub fn reader(&self) -> ParamRangeReader {
        ParamRangeReader(self.0.reader())
    }

    /// Whether `self` and `other` are the same cell.
    #[must_use]
    pub fn same_cell(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A param port's live range as the audio thread reads it: a [`Reader`] on
/// the port's cell, registered on the control side
/// ([`ParamRangeCell::reader`]) and kept by the node that clamps. It lends
/// once per block and never keeps the loan across blocks. It keeps its cell
/// alive, so it is dropped where its node is: on the control side, when a
/// commit hands the retired unit back.
#[derive(Debug)]
pub struct ParamRangeReader(Reader<ParamRange>);

impl ParamRangeReader {
    /// The range this block: both bounds as one publish left them. Wait-free
    /// and free-free.
    #[inline]
    pub fn load(&mut self) -> ParamRange {
        *self.0.lend()
    }
}

/// One declared param port's running state, kept with its unit across
/// blocks and plans.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PortState<F: Float> {
    /// Whether the port has not run yet: its first block takes its source
    /// without a declick (see the module docs).
    fresh: bool,
    /// The event source it last ran with (`None`: none).
    source: Option<EventOut>,
    /// Whether the last block delivered frames.
    modulated: bool,
    /// The last frame delivered.
    last: F,
    /// The base at the end of the last block.
    last_base: F,
    /// Where the declick fades from.
    hold: F,
    /// Frames of the declick done; the declick's length or more: none running.
    fade: usize,
    /// Its event source's ramp: the source's own, so it is reset when the
    /// source changes and kept otherwise.
    ramp: Ramp,
}

impl<F: Float> Default for PortState<F> {
    fn default() -> Self {
        Self {
            fresh: true,
            source: None,
            modulated: false,
            last: F::ZERO,
            last_base: F::ZERO,
            hold: F::ZERO,
            fade: usize::MAX,
            ramp: Ramp::default(),
        }
    }
}

/// What a unit keeps for its declared param ports: their state and the
/// buffer their per-frame values are written into. Built on the control side
/// with the unit's commit, so the audio thread never allocates one.
#[derive(Debug)]
pub(crate) struct ParamState<F: Float> {
    ports: Vec<PortState<F>>,
    scratch: Vec<F>,
    /// Each port's corners this block, `max` slots a port: at most one a
    /// frame, so a port's slots never run out.
    corners: Vec<u32>,
    /// How many of each port's corner slots this block wrote.
    corner_count: [usize; MAX_PARAM_PORTS],
    max: usize,
    /// The declick's length in frames: [`DECLICK`](crate::DECLICK) at the
    /// graph's rate.
    declick: usize,
    /// Per port: whether this block's step delivered frames.
    framed: [bool; MAX_PARAM_PORTS],
    /// Whether any port delivered frames this block, is mid-declick, or has
    /// yet to run: a unit for which this is false and whose op lists no
    /// driven port skips the whole step.
    busy: bool,
    /// Each port's live range reader, in port order (see [`ParamRanges`]);
    /// empty for a unit with no declared params and for one whose ports are
    /// unbounded.
    ranges: Vec<ParamRangeReader>,
}

impl<F: Float> Default for ParamState<F> {
    fn default() -> Self {
        Self {
            ports: Vec::new(),
            scratch: Vec::new(),
            corners: Vec::new(),
            corner_count: [0; MAX_PARAM_PORTS],
            max: 0,
            declick: 0,
            framed: [false; MAX_PARAM_PORTS],
            busy: false,
            ranges: Vec::new(),
        }
    }
}

impl<F: Float> ParamState<F> {
    /// State for `ports` declared params at blocks of up to `max` frames,
    /// clamping to `ranges` (a reader per port; none: unbounded). Allocates
    /// nothing when `ports` is 0.
    pub(crate) fn new(
        ports: usize,
        max: usize,
        declick: usize,
        ranges: Vec<ParamRangeReader>,
    ) -> Self {
        if ports == 0 {
            return Self::default();
        }
        Self {
            ports: vec![PortState::default(); ports],
            scratch: vec![F::ZERO; ports * max],
            corners: vec![0; ports * max],
            corner_count: [0; MAX_PARAM_PORTS],
            max,
            declick,
            framed: [false; MAX_PARAM_PORTS],
            // Its first block runs the step whatever the plan says, to
            // seed each port (see `PortState::fresh`).
            busy: true,
            ranges,
        }
    }

    /// Whether the step must run although the plan drives nothing here:
    /// a declick to base after a disconnect, or a port that has not run yet.
    /// After the step, whether any port delivered frames this block.
    #[inline]
    pub(crate) fn busy(&self) -> bool {
        self.busy
    }

    /// State for **one** port at blocks of up to `max` frames, clamping to
    /// `ranges`: an edge gain's lane, which runs the
    /// param port's step on its own.
    pub(crate) fn lane(max: usize, declick: usize, ranges: &ParamRanges) -> Self {
        Self::new(1, max, declick, ranges.readers())
    }

    /// The lane has run a block undriven: its first block under a source is
    /// a connection to a running port, not a first block.
    pub(crate) fn resume(&mut self) {
        self.ports[0].fresh = false;
    }

    /// Whether the state can serve blocks of up to `max` frames, declicking
    /// over `declick` frames.
    pub(crate) fn fits(&self, max: usize, declick: usize) -> bool {
        self.max >= max && self.declick == declick
    }

    /// Port `k`'s values this block, after [`port`](Self::port): `None` when
    /// it reads its base.
    #[inline]
    pub(crate) fn frames(&self, k: usize, frames: usize) -> Option<&[F]> {
        self.framed[k].then(|| &self.scratch[k * self.max..k * self.max + frames])
    }

    /// What each port reads this block, after [`port`](Self::port).
    pub(crate) fn inputs(&self, frames: usize) -> [ParamInput<'_, F>; MAX_PARAM_PORTS] {
        let mut out = [ParamInput::Base; MAX_PARAM_PORTS];
        for (k, o) in out.iter_mut().enumerate().take(self.ports.len()) {
            if self.framed[k] {
                let at = k * self.max;
                *o = ParamInput::Frames {
                    values: &self.scratch[at..at + frames],
                    corners: &self.corners[at..at + self.corner_count[k]],
                };
            }
        }
        out
    }

    /// How many params it keeps state for.
    pub(crate) fn port_count(&self) -> usize {
        self.ports.len()
    }

    /// Start a block: no port delivers frames until [`port`](Self::port)
    /// says so.
    #[inline]
    pub(crate) fn begin(&mut self) {
        self.framed = [false; MAX_PARAM_PORTS];
        self.corner_count = [0; MAX_PARAM_PORTS];
        self.busy = false;
    }

    /// Run the fused step for declared port `k` (param `param`) over one
    /// block: its event source this plan and the source's sorted events
    /// (`None` when nothing drives it), and its base (`None` for a node that
    /// cannot say, which then reads its own control, undriven). The value
    /// is held to the port's live range as it stands at this block's start
    /// ([`ParamRanges`]): clamped, or for a circular param wrapped (and held
    /// to its arc). A circular param's frames jump by 360 across the seam
    /// (see the module docs): a consumer smooths on the arc, not on the
    /// frames.
    pub(crate) fn port(
        &mut self,
        k: usize,
        param: UnitAddr,
        frames: usize,
        m: Option<(EventOut, &[ParamEvent])>,
        base: Option<F>,
    ) {
        let d = self.declick;
        let st = &mut self.ports[k];
        let Some(b1) = base else {
            // A node that cannot say its base is never driven: it reads
            // its own control. No state to carry.
            *st = PortState::default();
            return;
        };
        let source = m.map(|(from, _)| from);
        if st.fresh {
            // The first block: nothing to fade from (module docs).
            st.fresh = false;
            st.source = source;
        } else if source != st.source {
            // The source changed: fade from where the port was, and start
            // the new source's ramp at rest.
            st.hold = if st.modulated { st.last } else { b1 };
            st.fade = 0;
            st.source = source;
            st.ramp = Ramp::default();
        }
        let fading = st.fade < d;
        if source.is_none() && !fading {
            st.modulated = false;
            return;
        }
        let from = if st.modulated { st.last_base } else { b1 };
        let out = &mut self.scratch[k * self.max..k * self.max + frames];
        match m {
            Some((_, events)) => {
                let mut corners = Corners::new(&mut self.corners[k * self.max..(k + 1) * self.max]);
                render_ramps(
                    &mut st.ramp,
                    events,
                    param,
                    out,
                    |i| {
                        ramp_in(
                            Some(param.param),
                            from,
                            b1,
                            Samples::new(i),
                            Samples::new(frames),
                        )
                    },
                    &mut corners,
                );
                self.corner_count[k] = corners.len();
                // The live range, read once per block: a move lands on the
                // next block, and the stored base and the source's ramp are
                // never rewritten, so a range that widens again releases
                // the value to where they put it.
                let range = self
                    .ranges
                    .get_mut(k)
                    .map_or(ParamRange::UNBOUNDED, ParamRangeReader::load);
                let view = range.view(Some(param.param));
                view.constrain_lane(out);
            }
            None => {
                for (i, o) in out.iter_mut().enumerate() {
                    *o = ramp_in(
                        Some(param.param),
                        from,
                        b1,
                        Samples::new(i),
                        Samples::new(frames),
                    );
                }
            }
        }
        if fading {
            let circular = param.param.is_circular();
            for (i, o) in out.iter_mut().enumerate() {
                let j = st.fade + i;
                if j >= d {
                    break;
                }
                let (out, inn) =
                    fade_gains::<F>(FadeLaw::EqualAmplitude, Samples::new(j), Samples::new(d));
                *o = if circular {
                    // A crossfade of two bearings would sweep through the
                    // mean; the fade turns the short arc by the incoming gain.
                    wrap_in(
                        param.param,
                        st.hold + arc_in(param.param, st.hold, *o) * inn,
                    )
                } else {
                    st.hold * out + *o * inn
                };
            }
            st.fade = (st.fade + frames).min(d);
        }
        st.last = out[frames - 1];
        st.last_base = b1;
        st.modulated = true;
        self.framed[k] = true;
        self.busy = true;
    }
}

impl<F: Float> Drop for ParamState<F> {
    /// A unit's param state is freed on the control side: a retired unit's
    /// comes back in its commit box (`Commit::spare_params`). One that
    /// allocated nothing (no declared params) may go anywhere.
    fn drop(&mut self) {
        if !self.ports.is_empty() {
            RtScope::check_outside("a unit's param state");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param_event::{ParamEvent, ParamRamp};
    use crate::time::Offset;
    use tutti_units::{Hz, ParamKey, Real, Samples};

    const AZ: Option<UnitParam> = Some(UnitParam::Azimuth);
    const EL: Option<UnitParam> = Some(UnitParam::Elevation);

    /// An arc across ±180 contains 180 (and its neighbours either side) and
    /// not 0, where `new(170, -170)` ordered would read the opposite arc.
    #[test]
    fn an_arc_across_180_contains_180_and_not_0() {
        let behind = ParamRange::arc(Azimuth::new(180.0), ArcDegrees::new(20.0)).view(AZ);
        for v in [180.0, -180.0, 165.0, -165.0, 160.0, -160.0] {
            assert!(behind.contains(v), "{v}");
        }
        for v in [0.0, 90.0, -90.0, 159.0, -159.0] {
            assert!(!behind.contains(v), "{v}");
        }
        // The same bounds as a line are the opposite, wide range: the trap.
        let line = ParamRange::new(160.0, 200.0).view(EL);
        assert!(line.contains(180.0) && !line.contains(-180.0));
        assert_eq!(behind.span(), 40.0);
    }

    /// A circular range's span is its arc's length, the whole circle for an
    /// unbounded or wide one; a line's is `max - min`.
    #[test]
    fn a_circular_ranges_span_is_its_arcs_length() {
        assert_eq!(ParamRange::UNBOUNDED.view(AZ).span(), 360.0);
        assert_eq!(ParamRange::new(-180.0, 180.0).view(AZ).span(), 360.0);
        let arc = |half| ParamRange::arc(Azimuth::new(0.0), ArcDegrees::new(half)).view(AZ);
        assert_eq!(arc(90.0).span(), 180.0);
        assert_eq!(arc(400.0).span(), 360.0);
        assert_eq!(ParamRange::new(-90.0, 90.0).view(EL).span(), 180.0);
        assert!(ParamRange::UNBOUNDED.view(EL).span().is_infinite());
        assert!(ParamRange::UNBOUNDED.view(None).span().is_infinite());
    }

    /// `constrain` wraps for a circle (190 is -170, not 180) and clamps for a
    /// line; an arc holds to its nearer end.
    #[test]
    fn constrain_wraps_a_circle_and_clamps_a_line() {
        let r = ParamRange::new(-180.0, 180.0);
        assert_eq!(r.view(AZ).constrain(190.0), -170.0);
        assert_eq!(r.view(EL).constrain(190.0), 180.0);
        let arc = ParamRange::arc(Azimuth::new(90.0), ArcDegrees::new(30.0)).view(AZ);
        assert_eq!(arc.constrain(100.0), 100.0);
        assert_eq!(arc.constrain(170.0), 120.0);
        assert_eq!(arc.constrain(10.0), 60.0);
        assert_eq!(
            arc.constrain(-170.0),
            120.0,
            "the near end is the short way round"
        );
    }

    /// A ramp event starts on its frame and lands on its target on its last
    /// frame; a zero-length one is a step on its frame.
    #[test]
    fn a_ramp_lands_on_its_last_frame() {
        let ev = |at: usize, t: f64, len: usize| {
            ParamEvent::ramp(
                Offset::of(Samples::new(at), Samples::new(16)),
                ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz::new(t), Samples::new(len)),
            )
        };
        let events = [ev(4, 100.0, 4), ev(12, -5.0, 0)];
        let mut r = Ramp::default();
        let mut out = [0.0f32; 16];
        render_ramps(
            &mut r,
            &events,
            UnitParam::Cutoff.into(),
            &mut out,
            |_| 0.0,
            &mut Corners::new(&mut [0; 4096]),
        );
        assert_eq!(&out[..4], &[0.0; 4], "nothing before the event");
        assert_eq!(out[4], 25.0);
        assert_eq!(out[7], 100.0, "on target at the ramp's last frame");
        assert_eq!(out[11], 100.0, "held");
        assert_eq!(out[12], -5.0, "a step lands on its frame");
        // Another param's ramp is not this port's.
        let mut r = Ramp::default();
        render_ramps(
            &mut r,
            &events,
            UnitParam::Q.into(),
            &mut out,
            |_| 0.0,
            &mut Corners::new(&mut [0; 4096]),
        );
        assert!(out.iter().all(|&x| x == 0.0));
    }

    /// A segment and then a ramp on one frame, as a source's first events:
    /// the ramp replaces the segment from the base at that frame, as it
    /// would alone, under a base that moves every frame.
    #[test]
    fn a_ramp_after_a_first_segment_on_its_frame_starts_from_the_base() {
        use crate::param_event::ParamSegment;
        let at = Offset::of(Samples::new(4), Samples::new(8));
        let seg = ParamSegment::new(
            ParamKey::<Hz>::CUTOFF,
            Hz::new(5.0),
            Hz::new(9.0),
            Samples::new(20),
        );
        let ramp = ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz::new(0.0), Samples::new(2));
        let base = |i: usize| 1.0 + i as f32;
        let mut after_segment = [0.0f32; 8];
        let events = [ParamEvent::segment(at, seg), ParamEvent::ramp(at, ramp)];
        render_ramps(
            &mut Ramp::default(),
            &events,
            UnitParam::Cutoff.into(),
            &mut after_segment,
            base,
            &mut Corners::new(&mut [0; 4096]),
        );
        let mut alone = [0.0f32; 8];
        let events = [ParamEvent::ramp(at, ramp)];
        render_ramps(
            &mut Ramp::default(),
            &events,
            UnitParam::Cutoff.into(),
            &mut alone,
            base,
            &mut Corners::new(&mut [0; 4096]),
        );
        assert_eq!(after_segment, [1.0, 2.0, 3.0, 4.0, 2.5, 0.0, 0.0, 0.0]);
        assert_eq!(after_segment, alone, "as the ramp alone");
    }

    /// A linear ramp renders what the port rendered before ramps had a
    /// shape, bit for bit: `from + (target - from) · (done / len)`, written
    /// out here, at every frame of ramps of many lengths and spans.
    #[test]
    fn a_linear_ramp_renders_the_unshaped_bits() {
        for len in [1usize, 2, 3, 7, 10, 64, 97, 1000] {
            for (from, target) in [(0.0f64, 1.0f64), (0.3, -7.9), (440.0, 1234.5)] {
                // The port renders in `f64` and crosses to `f32` once a frame:
                // the oracle is the same line, from the base the lane gives it.
                let from32 = f32::from_unit(Hz::new(from));
                let start = f64::from(from32);
                let frames = len + 4;
                let events = [ParamEvent::ramp(
                    Offset::of(Samples::new(1), Samples::new(frames)),
                    ParamRamp::new(ParamKey::<Hz>::CUTOFF, Hz::new(target), Samples::new(len)),
                )];
                let mut r = Ramp::default();
                let mut out = vec![0.0f32; frames];
                render_ramps(
                    &mut r,
                    &events,
                    UnitParam::Cutoff.into(),
                    &mut out,
                    |_| from32,
                    &mut Corners::new(&mut vec![0; frames]),
                );
                for (j, &v) in out[1..].iter().enumerate() {
                    let done = j + 1;
                    let want = f32::from_unit(Hz::new(if done >= len {
                        target
                    } else {
                        start + (target - start) * (done as f64 / len as f64)
                    }));
                    assert_eq!(v.to_bits(), want.to_bits(), "len {len}, frame {j}");
                }
            }
        }
    }

    /// A list a node cannot declare is a named error, not a panic.
    #[test]
    fn an_undeclarable_list_is_a_named_error() {
        use UnitParam::*;
        let kinds = [
            Cutoff, Q, Drive, Wet, Threshold, Ratio, Attack, Release, Volume,
        ];
        assert_eq!(
            ParamPorts::try_new(&kinds).map(|p| p.len()),
            Ok(kinds.len())
        );
        let over = [Cutoff; MAX_PARAM_PORTS + 1];
        assert_eq!(
            ParamPorts::try_new(&over),
            Err(ParamPortsError::TooMany {
                count: MAX_PARAM_PORTS + 1
            })
        );
        assert_eq!(
            ParamPorts::try_new(&[Q, Cutoff, Q]),
            Err(ParamPortsError::Duplicate(Q.into()))
        );
    }

    /// The port's corners follow `ParamInput::corners`' rule: a new
    /// segment's frame and its arrival are corners; the piece in flight sent
    /// again, resumed (a source's re-send, a resync), is none; a step is one
    /// corner; a ramp's frame and the frame it lands on are corners.
    #[test]
    fn corners_are_where_the_trajectory_changes_piece() {
        use crate::param_event::ParamSegment;
        let key = ParamKey::<Hz>::CUTOFF;
        let at = |i: usize| Offset::of(Samples::new(i), Samples::new(64));
        let seg = |from: f64, to: f64, len: usize| {
            ParamSegment::new(key, Hz::new(from), Hz::new(to), Samples::new(len))
        };
        let corners = |events: &[ParamEvent]| {
            let mut buf = [0u32; 64];
            let mut c = Corners::new(&mut buf);
            render_ramps(
                &mut Ramp::default(),
                events,
                UnitParam::Cutoff.into(),
                &mut [0.0; 64],
                |_| 0.0,
                &mut c,
            );
            let n = c.len();
            buf[..n].to_vec()
        };
        // A segment at 4 over 10 frames, in the ramp convention (its frame
        // one step in): its frame and the frame it lands on, 13.
        let a = seg(1.0, 2.0, 10);
        assert_eq!(corners(&[ParamEvent::segment(at(4), a)]), [4, 13]);
        // The same piece re-sent resumed at 9, five frames on: still 4, 13.
        let resent = a.resumed(a.position() + Samples::new(5), a.length());
        assert_eq!(
            corners(&[
                ParamEvent::segment(at(4), a),
                ParamEvent::segment(at(9), resent)
            ]),
            [4, 13]
        );
        // Re-sent after it arrived, at rest on its end: no corner either.
        let rested = a.resumed(a.length(), a.length());
        assert_eq!(
            corners(&[
                ParamEvent::segment(at(4), a),
                ParamEvent::segment(at(30), rested)
            ]),
            [4, 13]
        );
        // A different piece at 9 is a corner, and so is its own arrival.
        let b = seg(1.5, 3.0, 4);
        assert_eq!(
            corners(&[ParamEvent::segment(at(4), a), ParamEvent::segment(at(9), b)]),
            [4, 9, 12]
        );
        // A step is one corner on its frame.
        assert_eq!(
            corners(&[ParamEvent::segment(at(7), seg(0.0, 5.0, 0))]),
            [7]
        );
        // A ramp of 3 frames at 20: its frame and the frame it lands on, 22.
        let ramp = ParamRamp::new(key, Hz::new(8.0), Samples::new(3));
        assert_eq!(corners(&[ParamEvent::ramp(at(20), ramp)]), [20, 22]);
    }

    /// **A range's two bounds are one publish's.** While another thread
    /// publishes `-k..k` as fast as it can, a reader that lends once per
    /// block never reads a `min` that is not the negation of its `max` (a
    /// torn pair would), and it ends on the last publish; the cell has
    /// nothing retired once the reader is idle.
    #[test]
    fn a_range_is_read_whole_while_another_thread_publishes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        const LAST: u32 = 20_000;
        let addr: UnitAddr = UnitParam::Cutoff.into();
        let ranges = ParamRanges::new(ParamPorts::of(&[addr]));
        let mut reader = ranges.readers().remove(0);
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                for k in 1..=LAST {
                    let k = f64::from(k);
                    assert!(ranges.store(addr, ParamRange::new(-k, k)));
                }
                done.store(true, Ordering::Release);
            });
            loop {
                let finished = done.load(Ordering::Acquire);
                let r = reader.load();
                assert_eq!(r.min, -r.max, "one publish's bounds: {r:?}");
                if finished {
                    break;
                }
            }
        });
        let last = f64::from(LAST);
        assert_eq!(reader.load(), ParamRange::new(-last, last));
        assert_eq!(ranges.reclaim(), 0, "nothing is held once the reader idles");
    }
}
