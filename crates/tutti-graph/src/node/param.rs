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
//! A source with nothing to say is not a source: a port delivers these frames
//! only through a connect or disconnect declick and from its source's first
//! event on. A source that has not spoken leaves the port on its base, as an
//! unconnected port reads.
//!
//! **The base is smoothed by the graph**, on every port that does not land
//! whole ([`ParamPorts::whole`]): a move of the node's cell ramps
//! linearly from the last frame delivered to the new value over
//! [`Prepare::declick`](crate::Prepare) frames, counted across blocks, and
//! reaches the node as [`ParamInput::Smoothing`], which is not a drive. A
//! retarget mid-ramp continues from the last frame. There is no ramp on a
//! unit's first block, after a prepare or a replace, or in a fork, and a port
//! leaving its source is carried by the source's declick alone. Before a
//! source's first event, a driven port's base is the same ramp.
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

use std::sync::Arc;

use crate::NodeKey;
use crate::model::mode::NodeMix;
use atomic_float::AtomicF64;
use std::sync::atomic::Ordering;
use tutti_buffers::Float;
use tutti_rt::{AtomicCell, Publisher, Reader, RtScope, atomic_word};
use tutti_units::{
    ArcDegrees, Azimuth, Normalized, ParamAddr, Quantity, Samples, UnitAddr, UnitParam,
};

use crate::exec::lane::{Progress, arc_in, control_in, declick_gains, ramp_in, wrap_in};
use crate::model::spec::EventOutputPort;
use crate::node::param_event::{ParamEvent, ParamKind, Timing, arrival, segment_progress};
use tutti_curve::{Law, Piece};

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
/// threshold per band) declares each at its own index. Two bytes per entry,
/// and one bit per port for the ports that land whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParamPorts {
    len: u8,
    /// Bit `k`: port `k` lands whole (see [`whole`](Self::whole)).
    whole: u32,
    ids: [UnitAddr; MAX_PARAM_PORTS],
}

impl ParamPorts {
    /// No modulatable params.
    pub const NONE: Self = Self {
        len: 0,
        whole: 0,
        ids: [UnitAddr::first(UnitParam::Cutoff); MAX_PARAM_PORTS],
    };

    /// The one port of a node's mix lane, [`NodeMix::ADDR`]: what only the
    /// graph may list (a node's own list refuses it).
    pub(crate) const MIX: Self = Self {
        len: 1,
        whole: 0,
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

    /// These ports, with the ports whose bits are set in `mask` (bit `k`: port
    /// `k`) landing **whole**: the graph never smooths their base, so a move
    /// of the node's own cell reaches it on the block it happens in, as a
    /// step. A node marks a param whole when it smooths it itself or when its
    /// value cannot be interpolated (a choice, an encoded `None`). Bits past
    /// the last port are ignored.
    #[must_use]
    pub const fn whole(mut self, mask: u32) -> Self {
        let ports = if self.len as usize == MAX_PARAM_PORTS {
            u32::MAX
        } else {
            (1u32 << self.len) - 1
        };
        self.whole = mask & ports;
        self
    }

    /// Whether port `k` lands whole ([`whole`](Self::whole)).
    #[inline]
    #[must_use]
    pub(crate) const fn lands_whole(&self, k: usize) -> bool {
        self.whole >> k & 1 == 1
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
/// `ParamRamp` wire is. A bound is never NaN ([`new`](Self::new) refuses
/// one and the fields are private), so a clamp by it cannot panic or yield
/// NaN; an infinite bound is no clamp on that side. Crossed bounds are
/// ordered where they are applied, never a panic.
///
/// Sixteen bytes, so a port's live range is published whole through a
/// [`ParamRangeCell`] and a reader sees one publish's `min` and `max`
/// together.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamRange {
    min: f64,
    max: f64,
}

impl ParamRange {
    /// No clamp at all.
    pub const UNBOUNDED: Self = Self {
        min: f64::NEG_INFINITY,
        max: f64::INFINITY,
    };

    /// `min..=max`, or `None` when either bound is NaN. For a circular param
    /// ([`UnitParam::is_circular`]) the bounds read as an arc, `min` to `max`
    /// counter-clockwise, and `min` must not exceed `max` (a crossed range is
    /// refused, [`ParamRangeError::InvalidRange`]); [`arc`](Self::arc) writes
    /// one by its centre.
    ///
    /// ```
    /// use tutti_graph::ParamRange;
    ///
    /// assert_eq!(ParamRange::new(0.0, 1.0), Some(ParamRange::of(0.0, 1.0)));
    /// assert_eq!(ParamRange::new(f64::NAN, 1.0), None);
    /// assert!(ParamRange::new(f64::NEG_INFINITY, 1.0).is_some());
    /// ```
    #[must_use]
    pub const fn new(min: f64, max: f64) -> Option<Self> {
        if min.is_nan() || max.is_nan() {
            None
        } else {
            Some(Self { min, max })
        }
    }

    /// [`new`](Self::new) for bounds known not to be NaN, such as literals.
    ///
    /// # Panics
    ///
    /// If either bound is NaN (a compile error in a `const`).
    #[must_use]
    pub const fn of(min: f64, max: f64) -> Self {
        match Self::new(min, max) {
            Some(range) => range,
            None => panic!("a range bound is not NaN"),
        }
    }

    /// The lowest bound, never NaN.
    #[inline]
    #[must_use]
    pub const fn min(self) -> f64 {
        self.min
    }

    /// The highest bound, never NaN.
    #[inline]
    #[must_use]
    pub const fn max(self) -> f64 {
        self.max
    }

    /// The arc of the circle `half_span` either side of `centre`: the range
    /// of a circular param ([`UnitParam::is_circular`], `Azimuth`). It is the
    /// only way to write a bounded range of one: `of(170.0, -170.0)` would
    /// be ordered into `-170..170`, the opposite arc, and a circular param's
    /// port refuses a crossed range
    /// ([`ParamRangeError::InvalidRange`]).
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

/// Why [`Editor::set_param_range`](crate::Editor::set_param_range) refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ParamRangeError {
    /// The spec has no node at the key.
    #[error("no node {} to set a param range on", .node.0)]
    NoSuchNode {
        /// The key.
        node: crate::NodeKey,
    },
    /// The range has crossed bounds on a circular param (an arc is written
    /// [`ParamRange::arc`]; ordering `170..-170` would read the opposite arc).
    /// A line orders its bounds, so any range fits it.
    #[error("the range of {at:?} is a crossed arc")]
    InvalidRange {
        /// The param port.
        at: ParamIn,
    },
}

impl Default for ParamRange {
    fn default() -> Self {
        Self::UNBOUNDED
    }
}

/// What a node reads for one of its params this block
/// ([`Io::param`](crate::Io::param)).
///
/// Values are `f64`, the unit's own width, at any graph float; a reader that
/// runs per sample in the graph's float reads [`narrowed`](Self::narrowed),
/// which the graph fills once per block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamInput<'a, F: Float = f32> {
    /// Nothing modulates it and its base is at rest: use the param's own
    /// control (the base), as the node would with no graph around it. The
    /// fast path.
    Base,
    /// Nothing drives it, and the graph is moving its base: the base's value
    /// at each frame of the block, a linear ramp from where the last block
    /// left it to the cell's value over the declick. Not a drive: it has no
    /// corners, [`is_driven`](Self::is_driven) is false, and a param that
    /// lands whole is never smoothed.
    Smoothing {
        /// The value at each frame.
        values: &'a [f64],
        /// `values` in the graph's float.
        narrow: &'a [F],
    },
    /// The param's value at each frame of the block, and its corners. A
    /// circular param's values are wrapped, so they step by about 360 across
    /// the seam (see the module docs).
    Frames {
        /// The value at each frame.
        values: &'a [f64],
        /// `values` in the graph's float.
        narrow: &'a [F],
        /// The block offsets where the delivered trajectory changes piece,
        /// ascending, at most one per frame (see [`corners`](Self::corners)).
        corners: &'a [u32],
    },
}

impl<'a, F: Float> ParamInput<'a, F> {
    /// The value on the block's **last** frame, when it has frames: what a
    /// block-rate control reads, and where a block-rate cell's own ramp ends.
    /// `None`: nothing modulates or smooths it, read the cell.
    #[inline]
    #[must_use]
    pub fn last_frame(self) -> Option<f64> {
        self.frames().and_then(|v| v.last().copied())
    }

    /// The per-frame values, when the param is driven or its base is being
    /// smoothed.
    #[inline]
    #[must_use]
    pub fn frames(self) -> Option<&'a [f64]> {
        match self {
            Self::Base => None,
            Self::Smoothing { values, .. } | Self::Frames { values, .. } => Some(values),
        }
    }

    /// [`frames`](Self::frames) in the graph's float, narrowed once per block
    /// by the graph: what a per-sample reader that runs in `F` reads.
    #[inline]
    #[must_use]
    pub fn narrowed(self) -> Option<&'a [F]> {
        match self {
            Self::Base => None,
            Self::Smoothing { narrow, .. } | Self::Frames { narrow, .. } => Some(narrow),
        }
    }

    /// Whether a source drives it this block: [`Frames`](Self::Frames), not a
    /// base the graph is smoothing. A node that steps or reports a tail for a
    /// driven param asks this, not [`frames`](Self::frames).
    #[inline]
    #[must_use]
    pub fn is_driven(self) -> bool {
        matches!(self, Self::Frames { .. })
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
    /// drives it. At most one per frame, so never more than the block's
    /// length.
    #[inline]
    #[must_use]
    pub fn corners(self) -> &'a [u32] {
        match self {
            Self::Base | Self::Smoothing { .. } => &[],
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

/// Whether `event` moves `param`: a ramp or curve segment addressed to it.
fn speaks_for(event: &ParamEvent, param: UnitAddr) -> bool {
    let want = ParamAddr::unit(param);
    match event.kind {
        ParamKind::Ramp(r) => r.addr() == want,
        ParamKind::Segment(s) => s.is_for(want),
    }
}

/// Render one event source's values for a block into `out`: the ramps and
/// curve segments its events for `param` start, frame by frame, and
/// `base(i)` on each frame before its first event ever. A first ramp starts
/// from `base` at its own frame, whether or not a segment came before it
/// on that frame.
pub(crate) fn render_ramps(
    ramp: &mut Ramp,
    events: &[ParamEvent],
    param: UnitAddr,
    out: &mut [f64],
    base: impl Fn(usize) -> f64,
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
                        ramp.value = base(i);
                    }
                    ramp.start(r.raw_target(), r.duration().get(), r.shape());
                    corners.mark(i);
                }
                ParamKind::Segment(s) if s.is_for(want) => {
                    // A segment never reads the value it interrupts, but a
                    // ramp that replaces it on this frame does: a first one
                    // starts from the base, as it would alone.
                    if !ramp.started {
                        ramp.value = base(i);
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
            v
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
/// commit from the editor's table), read by the audio thread at the start of
/// the port's step, so a move lands on the next block and costs a publish, not
/// a plan. The table's range is its initial value; the plan carries no range.
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
    /// What each port's lane last delivered, in port order.
    lanes: Box<[EffectiveCell]>,
}

impl ParamRanges {
    /// Unbounded cells for `declared`. Control side: allocates.
    pub(crate) fn new(declared: ParamPorts) -> Arc<Self> {
        Arc::new(Self {
            declared,
            cells: (0..declared.len())
                .map(|_| ParamRangeCell(Arc::new(Publisher::new(ParamRange::UNBOUNDED))))
                .collect(),
            lanes: (0..declared.len()).map(|_| EffectiveCell::new()).collect(),
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

    /// A handle on `param`'s delivered value, if it is one of these ports.
    /// Control side.
    pub(crate) fn lane(&self, param: UnitAddr) -> Option<EffectiveCell> {
        self.lanes.get(self.declared.index_of(param)?).cloned()
    }

    /// One handle per port, in port order, for a unit's param state to write
    /// its lane's last value into. Control side.
    pub(crate) fn lanes(&self) -> Vec<EffectiveCell> {
        self.lanes.to_vec()
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

/// Whether a port's lane delivered frames at the end of the last block that
/// ran it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum LaneState {
    /// No lane drives the port and none is fading: its value is the base.
    Idle,
    /// A lane drives the port, or its declick is still fading: the value cell
    /// holds the lane's last frame.
    Driven,
}
atomic_word!(LaneState { Idle, Driven });

/// A port's lane state and the value it last delivered. "No lane" is the
/// `Idle` state, never a value, so no value stands for it.
#[derive(Debug)]
struct Delivered {
    state: AtomicCell<LaneState>,
    value: AtomicF64,
}

/// What a param port's lane delivered on the last block that ran it: the
/// effective value, which the session reads beside the base
/// ([`Editor::param_effective_cell`](crate::Editor::param_effective_cell)).
///
/// The audio thread writes it once per block, and only while the port is
/// driven or fading: the value first, then the `Driven` state, both
/// `Release`, and the reader loads the state then the value, both `Acquire`.
/// A read that races a block's write sees the value of that block or of the
/// block before, so it is at most one block old; the value is the lane's last
/// frame, after the port's range, in the unit's own width.
///
/// A lane that delivers a NaN or an infinity is not latched: the cell goes
/// back to `Idle`, and the read answers the base.
#[derive(Clone, Debug)]
pub struct EffectiveCell(Arc<Delivered>);

impl EffectiveCell {
    /// An idle cell. Control side: allocates.
    fn new() -> Self {
        Self(Arc::new(Delivered {
            state: AtomicCell::new(LaneState::Idle),
            value: AtomicF64::new(0.0),
        }))
    }

    /// The lane's last frame, as the raw of the param's unit, or `None` while
    /// no lane drives the port (the effective value is then the base).
    /// Wait-free.
    #[must_use]
    pub fn current(&self) -> Option<f64> {
        (self.0.state.load() == LaneState::Driven).then(|| self.0.value.load(Ordering::Acquire))
    }

    /// Whether `self` and `other` are the same cell.
    #[must_use]
    pub fn same_cell(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Delivers `value`, a finite lane value; `fresh` when the cell was
    /// `Idle`. Audio side, wait-free.
    #[inline]
    fn deliver(&self, value: f64, fresh: bool) {
        self.0.value.store(value, Ordering::Release);
        if fresh {
            self.0.state.store(LaneState::Driven);
        }
    }

    /// Goes back to no lane. Audio side, wait-free.
    #[inline]
    fn release(&self) {
        self.0.state.store(LaneState::Idle);
    }
}

/// A read-only handle on one param port's live range cell, for the control
/// side: a control lane's source for the span it scales by and, through
/// [`reader`](Self::reader), the reader its node lends once per block. Handed
/// out by [`Editor::param_range_cell`](crate::Editor::param_range_cell).
///
/// Read-only on purpose: the editor's table is the range's one writer
/// ([`Editor::set_param_range`](crate::Editor::set_param_range) stores into
/// the table and this cell together, and every commit rewrites the cell from
/// the table), so a publish that went to the cell alone would be undone by the
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

    /// A cell of its own holding the range now, in no editor's table: what a
    /// fork reads, so a range set after the fork does not reach it. Control
    /// side: it locks and allocates.
    #[must_use]
    pub fn detached(&self) -> Self {
        Self(Arc::new(Publisher::new(self.current())))
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
pub(crate) struct PortState {
    /// The event source it last ran with (`None`: none).
    source: Option<EventOutputPort>,
    /// Whether the last block delivered frames from a source or a declick.
    modulated: bool,
    /// What the port's [`EffectiveCell`] reads, as far as this state knows.
    /// A state starts `Unknown`: the cell is the key's, and a unit placed
    /// without a crossfade replaced a state that may have left it `Driven`.
    shown: Shown,
    /// The last frame delivered.
    last: f64,
    /// Where the declick fades from; meaningful while `progress` runs.
    hold: f64,
    /// The declick's progress; idle when none runs.
    progress: Progress,
    /// Its event source's ramp: the source's own, so it is reset when the
    /// source changes and kept otherwise.
    ramp: Ramp,
    /// Its base, as the graph smooths it.
    smooth: Smooth,
}

/// What a port state knows of its [`EffectiveCell`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    /// Not written by this state yet: its first block stores either way.
    Unknown,
    Idle,
    Driven,
}

impl PortState {
    /// Whether the port is undriven with nothing running: no source, no frames
    /// last block, no declick.
    fn at_rest(&self) -> bool {
        self.source.is_none() && !self.modulated && !self.progress.running()
    }

    /// A port that has not run.
    fn new() -> Self {
        Self {
            source: None,
            modulated: false,
            shown: Shown::Unknown,
            last: 0.0,
            hold: 0.0,
            progress: Progress::default(),
            ramp: Ramp::default(),
            smooth: Smooth::new(),
        }
    }
}

/// A port's base as the graph smooths it: the value on the last frame
/// delivered, and the linear ramp from there to the base's value now, run over
/// [`Prepare::declick`](crate::Prepare) frames across blocks, so its length
/// does not depend on the block size. A base that moves mid-ramp starts a new
/// ramp from the last frame delivered.
#[derive(Clone, Copy, Debug)]
struct Smooth {
    /// Where the ramp started, and what it ends on.
    from: f64,
    to: f64,
    /// The value on the last frame delivered; `to` once the ramp is done.
    last: f64,
    /// The ramp's progress; idle when none runs.
    progress: Progress,
    /// Whether `last` is a value delivered, not yet seeded: the first base
    /// lands whole.
    seeded: bool,
}

impl Smooth {
    fn new() -> Self {
        Self {
            from: 0.0,
            to: 0.0,
            last: 0.0,
            progress: Progress::default(),
            seeded: false,
        }
    }

    /// A block begins with the base at `base`: it lands whole when `whole`
    /// (a port that lands whole, a unit's first block, a port leaving its
    /// source), when nothing was seeded, and when it is not finite; it
    /// starts a ramp from the last frame when it moved.
    fn begin(&mut self, base: f64, whole: bool, declick: Samples) -> BaseLine {
        if whole || !self.seeded || !base.is_finite() {
            *self = Self {
                from: base,
                to: base,
                last: base,
                progress: Progress::default(),
                seeded: true,
            };
        } else if base.to_bits() != self.to.to_bits() {
            self.from = self.last;
            self.to = base;
            self.progress.restart(declick);
        }
        BaseLine {
            from: self.from,
            to: self.to,
            done: self.progress.done(declick),
            len: declick,
            idle: self.last,
        }
    }

    /// A lane's block of `frames` frames at `base`: it moves over the block
    /// from where the last block left it (when that block delivered), and
    /// the next block starts from `base`.
    fn lane(&mut self, delivered: bool, base: f64, frames: usize) -> BaseLine {
        let line = BaseLine {
            from: if delivered { self.last } else { base },
            to: base,
            done: Samples::ZERO,
            len: Samples::new(frames),
            idle: base,
        };
        self.last = base;
        line
    }

    /// Whether the base is seeded, at rest on `base`: this block changes
    /// nothing.
    fn holds(&self, base: f64) -> bool {
        self.seeded && !self.progress.running() && base.to_bits() == self.to.to_bits()
    }

    /// Whether a ramp has frames left to run.
    fn running(&self) -> bool {
        self.progress.running()
    }

    /// The block has run its `frames` frames of `line`.
    fn end(&mut self, line: BaseLine, param: UnitParam, frames: usize, declick: Samples) {
        if self.progress.running() {
            self.last = line.at(param, frames - 1);
            self.progress.advance(Samples::new(frames), declick);
        } else {
            self.last = self.to;
        }
    }
}

/// A base's frames over one block: frame `i` of a linear ramp from `from` to
/// `to` of `len` frames, of which `done` have run before the block, landing
/// exactly on `to` on its last frame and holding it. A circular param takes
/// the short arc.
#[derive(Clone, Copy, Debug)]
struct BaseLine {
    from: f64,
    to: f64,
    done: Samples,
    len: Samples,
    /// The value the port reads undriven now: where it fades from when a
    /// source connects.
    idle: f64,
}

impl BaseLine {
    #[inline]
    fn at(self, param: UnitParam, i: usize) -> f64 {
        ramp_in(
            Some(param),
            self.from,
            self.to,
            self.done + Samples::new(i),
            self.len,
        )
    }
}

/// What a unit keeps for its declared param ports: their state and the
/// buffer their per-frame values are written into. Built on the control side
/// with the unit's commit, so the audio thread never allocates one.
#[derive(Debug)]
pub(crate) struct ParamState<F: Float> {
    ports: Vec<PortState>,
    /// What the ports delivered this block.
    delivered: PortFrames<F>,
    /// Whether any port delivered frames this block, is mid-declick, or has
    /// yet to run: a unit for which this is false and whose op lists no
    /// driven port skips the whole step, if every port lands whole.
    busy: bool,
    /// Where the unit is in its blocks: its first takes every port's source
    /// without a declick (see the module docs), and steps every port.
    run: Run,
    /// Whether a port the graph smooths has stepped: the unit steps its
    /// params every block, to see their bases move.
    smooths: bool,
    /// Each port's live range reader, in port order (see [`ParamRanges`]);
    /// empty for a unit with no declared params and for one whose ports are
    /// unbounded.
    ranges: Vec<ParamRangeReader>,
    /// Each port's delivered-value cell, in port order; empty where `ranges`
    /// is.
    lanes: Vec<EffectiveCell>,
}

/// Where a [`ParamState`] is in its blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    /// No block begun.
    New,
    /// The unit's first block: every port runs, and takes its source with
    /// nothing to fade from.
    First,
    /// Every later block.
    Later,
}

/// What a port delivered this block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivery {
    /// Nothing: the node reads the base.
    Base,
    /// Its base's ramp ([`ParamInput::Smoothing`]).
    Smoothing,
    /// A source or a declick's frames ([`ParamInput::Frames`]).
    Driven,
}

/// The per-frame values the declared ports delivered this block, and the
/// corners of their ramps: `max` slots a port. A port's frames are read only
/// when its step delivered them.
#[derive(Debug)]
struct PortFrames<F: Float> {
    /// The values, in the unit's own width.
    scratch: Vec<f64>,
    /// `scratch` in the graph's float, narrowed once per block for each port
    /// that delivered.
    narrow: Vec<F>,
    /// Each port's corners this block, `max` slots a port: at most one a
    /// frame, so a port's slots never run out.
    corners: Vec<u32>,
    /// How many of each port's corner slots this block wrote.
    corner_count: [usize; MAX_PARAM_PORTS],
    max: usize,
    /// Per port: what this block's step delivered.
    framed: [Delivery; MAX_PARAM_PORTS],
}

impl<F: Float> PortFrames<F> {
    /// Room for `ports` ports at blocks of up to `max` frames. Allocates.
    fn new(ports: usize, max: usize) -> Self {
        Self {
            scratch: vec![0.0; ports * max],
            narrow: vec![F::ZERO; ports * max],
            corners: vec![0; ports * max],
            corner_count: [0; MAX_PARAM_PORTS],
            max,
            framed: [Delivery::Base; MAX_PARAM_PORTS],
        }
    }

    /// Start a block: no port delivered frames.
    fn begin(&mut self) {
        self.framed = [Delivery::Base; MAX_PARAM_PORTS];
        self.corner_count = [0; MAX_PARAM_PORTS];
    }

    /// Port `k`'s values in the graph's float, once its block is final.
    fn narrowed(&self, k: usize, frames: usize) -> Option<&[F]> {
        (self.framed[k] != Delivery::Base)
            .then(|| &self.narrow[k * self.max..k * self.max + frames])
    }

    /// Narrows port `k`'s `frames` values (of `param`), once, after they are
    /// final. A circular param is wrapped again after the narrowing, which can
    /// round a value just under the seam onto it.
    fn narrow_port(&mut self, k: usize, frames: usize, param: UnitParam) {
        let at = k * self.max;
        let values = self.narrow[at..at + frames]
            .iter_mut()
            .zip(&self.scratch[at..at + frames]);
        for (n, &v) in values {
            *n = wrap_in(param, control_in(v));
        }
    }

    /// What each of `ports` ports reads this block.
    fn inputs(&self, ports: usize, frames: usize) -> [ParamInput<'_, F>; MAX_PARAM_PORTS] {
        let mut out = [ParamInput::Base; MAX_PARAM_PORTS];
        for (k, o) in out.iter_mut().enumerate().take(ports) {
            let at = k * self.max;
            let (values, narrow) = (
                &self.scratch[at..at + frames],
                &self.narrow[at..at + frames],
            );
            match self.framed[k] {
                Delivery::Base => {}
                Delivery::Smoothing => *o = ParamInput::Smoothing { values, narrow },
                Delivery::Driven => {
                    *o = ParamInput::Frames {
                        values,
                        narrow,
                        corners: &self.corners[at..at + self.corner_count[k]],
                    };
                }
            }
        }
        out
    }
}

impl<F: Float> Default for ParamState<F> {
    fn default() -> Self {
        Self {
            ports: Vec::new(),
            delivered: PortFrames::new(0, 0),
            busy: false,
            run: Run::New,
            smooths: false,
            ranges: Vec::new(),
            lanes: Vec::new(),
        }
    }
}

impl<F: Float> ParamState<F> {
    /// State for `ports` declared params at blocks of up to `max` frames,
    /// clamping to `ranges` (a reader per port; none: unbounded) and
    /// delivering each lane's last frame to `lanes` (a cell per port; none:
    /// nowhere). Allocates nothing when `ports` is 0.
    pub(crate) fn new(
        ports: usize,
        max: usize,
        ranges: Vec<ParamRangeReader>,
        lanes: Vec<EffectiveCell>,
    ) -> Self {
        if ports == 0 {
            return Self::default();
        }
        Self {
            ports: vec![PortState::new(); ports],
            delivered: PortFrames::new(ports, max),
            // Its first block runs the step whatever the plan says, to
            // seed each port (see `Run::First`).
            busy: true,
            run: Run::New,
            smooths: false,
            ranges,
            lanes,
        }
    }

    /// Whether the step must run although the plan drives nothing here:
    /// a declick to base after a disconnect, or a port that has not run yet.
    /// After the step, whether any port delivered frames this block.
    #[inline]
    pub(crate) fn busy(&self) -> bool {
        self.busy
    }

    /// Whether a port of the unit has its base smoothed by the graph, which
    /// is read every block ([`node_port`](Self::node_port)).
    #[inline]
    pub(crate) fn smooths(&self) -> bool {
        self.smooths
    }

    /// State for **one** port at blocks of up to `max` frames, clamping to
    /// `ranges`: an edge gain's lane, which runs the
    /// param port's step on its own.
    pub(crate) fn lane(max: usize, ranges: &ParamRanges) -> Self {
        Self::new(1, max, ranges.readers(), ranges.lanes())
    }

    /// Reads no lane from the cells: for a state that is dropped while its
    /// ports may still read `Driven`. Audio side, wait-free.
    pub(crate) fn release_lanes(&mut self) {
        for (st, lane) in self.ports.iter_mut().zip(&self.lanes) {
            if st.shown != Shown::Idle {
                lane.release();
                st.shown = Shown::Idle;
            }
        }
    }

    /// The lane has run a block undriven: its first block under a source is
    /// a connection to a running port, not a first block.
    pub(crate) fn resume(&mut self) {
        self.run = Run::Later;
    }

    /// A new node takes the key's state over (a replace): every base lands
    /// whole on its next block, as a unit's first does, since the state's
    /// last frame belongs to the node it replaces.
    pub(crate) fn reseat(&mut self) {
        for st in &mut self.ports {
            st.smooth = Smooth::new();
        }
    }

    /// Whether the state can serve blocks of up to `max` frames.
    pub(crate) fn fits(&self, max: usize) -> bool {
        self.delivered.max >= max
    }

    /// Port `k`'s values this block in the graph's float, after
    /// [`port`](Self::port): `None` when it reads its base.
    #[inline]
    pub(crate) fn narrowed(&self, k: usize, frames: usize) -> Option<&[F]> {
        self.delivered.narrowed(k, frames)
    }

    /// What each port reads this block, after [`port`](Self::port).
    pub(crate) fn inputs(&self, frames: usize) -> [ParamInput<'_, F>; MAX_PARAM_PORTS] {
        self.delivered.inputs(self.ports.len(), frames)
    }

    /// How many params it keeps state for.
    pub(crate) fn port_count(&self) -> usize {
        self.ports.len()
    }

    /// Start a block: no port delivers frames until [`port`](Self::port)
    /// says so.
    #[inline]
    pub(crate) fn begin(&mut self) {
        // Only a port that delivered last block left anything to clear.
        if self.busy {
            self.delivered.begin();
            self.busy = false;
        }
        self.run = match self.run {
            Run::New => Run::First,
            Run::First | Run::Later => Run::Later,
        };
    }

    /// Whether port `k`, which lands whole and is undriven this block, has
    /// nothing to do: its step would change no state and deliver nothing.
    /// Never in the unit's first block. A port is not at rest while its last
    /// source still stands (it changes this block), while a declick runs, or
    /// while its last block delivered (the lane is released on the block
    /// after). A port the graph smooths is never at rest: its base is read
    /// every block to see it move.
    #[inline]
    pub(crate) fn at_rest(&self, k: usize, whole: bool) -> bool {
        whole && self.run != Run::First && self.ports[k].at_rest()
    }

    /// A node that cannot say its base is never driven or smoothed: it reads
    /// its own control. No state to carry.
    fn unsaid(&mut self, k: usize) {
        let st = &mut self.ports[k];
        if st.shown != Shown::Idle
            && let Some(lane) = self.lanes.get(k)
        {
            lane.release();
        }
        *st = PortState::new();
        st.shown = Shown::Idle;
    }

    /// Run the fused step for a lane's one port over one block: its event
    /// source this plan and the source's sorted events (`None` when nothing
    /// drives it), and its base `base`, which moves over the block from where
    /// the last block left it ([`deliver`](Self::deliver)).
    pub(crate) fn port(
        &mut self,
        k: usize,
        param: UnitAddr,
        frames: usize,
        declick: Samples,
        m: Option<(EventOutputPort, &[ParamEvent])>,
        base: Option<f64>,
    ) {
        let Some(b1) = base else {
            self.unsaid(k);
            return;
        };
        let st = &mut self.ports[k];
        let line = st.smooth.lane(st.modulated, b1, frames);
        self.deliver(k, param, frames, declick, m, line);
    }

    /// Run the fused step for a node's declared port `k` (param `param`) over
    /// one block: its event source this plan and the source's sorted events
    /// (`None` when nothing drives it), and its base (`None` for a node that
    /// cannot say, which then reads its own control, undriven).
    ///
    /// The base is smoothed here, on the port, and nowhere else: a base that
    /// moved ramps linearly from the last frame delivered to its value over
    /// `declick` frames ([`Smooth`]), delivered as [`ParamInput::Smoothing`]
    /// while it runs. The port lands whole, with no ramp, when `whole` (the
    /// node's [`ParamPorts::whole`]), on the unit's first block, and
    /// while it leaves a source: the source's declick alone carries that.
    /// With a source, the ramp is what the port reads before the source's
    /// first event.
    #[inline]
    pub(crate) fn node_port(
        &mut self,
        k: usize,
        param: UnitAddr,
        frames: usize,
        declick: Samples,
        m: Option<(EventOutputPort, &[ParamEvent])>,
        base: Option<f64>,
        whole: bool,
    ) {
        let Some(b1) = base else {
            self.unsaid(k);
            return;
        };
        let st = &self.ports[k];
        // The common block: nothing drives it, nothing runs, the base held.
        if m.is_none()
            && self.run != Run::First
            && st.shown == Shown::Idle
            && st.at_rest()
            && st.smooth.holds(b1)
        {
            return;
        }
        self.step_port(k, param, frames, declick, m, b1, whole);
    }

    /// [`node_port`](Self::node_port) for a port with something to do.
    #[inline(never)]
    fn step_port(
        &mut self,
        k: usize,
        param: UnitAddr,
        frames: usize,
        declick: Samples,
        m: Option<(EventOutputPort, &[ParamEvent])>,
        b1: f64,
        whole: bool,
    ) {
        self.smooths |= !whole;
        let first = self.run == Run::First;
        let st = &mut self.ports[k];
        let leaving = m.is_none() && (st.source.is_some() || st.progress.running());
        let line = st.smooth.begin(b1, whole || first || leaving, declick);
        let driven = self.deliver(k, param, frames, declick, m, line);
        if !driven && self.ports[k].smooth.running() {
            let PortFrames {
                scratch,
                max,
                framed,
                ..
            } = &mut self.delivered;
            for (i, o) in scratch[k * *max..k * *max + frames].iter_mut().enumerate() {
                *o = line.at(param.param, i);
            }
            framed[k] = Delivery::Smoothing;
            self.delivered.narrow_port(k, frames, param.param);
            self.busy = true;
        }
        self.ports[k].smooth.end(line, param.param, frames, declick);
    }

    /// The step both kinds of port share: the port's source (or the declick
    /// off it), its range and its [`EffectiveCell`], with `line` the base
    /// the port reads before its source's first event and while it fades to
    /// no source (`line.idle`: the value it reads undriven now). A circular
    /// param's frames jump by 360 across the seam (see the module docs): a
    /// consumer smooths on the arc, not on the frames. A change of source
    /// fades over `declick` frames ([`Prepare::declick`](crate::Prepare)).
    /// The value is held to the port's live range as it stands at this
    /// block's start ([`ParamRanges`]): clamped, or for a circular param
    /// wrapped (and held to its arc).
    ///
    /// Whether the port delivered [`Delivery::Driven`] frames.
    fn deliver(
        &mut self,
        k: usize,
        param: UnitAddr,
        frames: usize,
        declick: Samples,
        m: Option<(EventOutputPort, &[ParamEvent])>,
        line: BaseLine,
    ) -> bool {
        let st = &mut self.ports[k];
        let source = m.map(|(from, _)| from);
        if self.run == Run::First {
            // The unit's first block: nothing to fade from (module docs).
            st.source = source;
        } else if source != st.source {
            // The source changed: fade from where the port was, and start
            // the new source's ramp at rest.
            st.hold = if st.modulated { st.last } else { line.idle };
            st.progress.restart(declick);
            st.source = source;
            st.ramp = Ramp::default();
        }
        let fading = st.progress.running();
        // A source with nothing to say is not a source: until its first event
        // for this param, outside a declick, the port reads its base.
        let silent = !st.ramp.started
            && m.is_none_or(|(_, events)| !events.iter().any(|e| speaks_for(e, param)));
        if (source.is_none() || silent) && !fading {
            if st.shown != Shown::Idle
                && let Some(lane) = self.lanes.get(k)
            {
                lane.release();
                st.shown = Shown::Idle;
            }
            st.modulated = false;
            return false;
        }
        let PortFrames {
            scratch,
            corners: corner_slots,
            corner_count,
            max: slot,
            framed,
            ..
        } = &mut self.delivered;
        let out = &mut scratch[k * *slot..k * *slot + frames];
        match m {
            Some((_, events)) => {
                let mut corners = Corners::new(&mut corner_slots[k * *slot..(k + 1) * *slot]);
                render_ramps(
                    &mut st.ramp,
                    events,
                    param,
                    out,
                    |i| line.at(param.param, i),
                    &mut corners,
                );
                corner_count[k] = corners.len();
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
                    *o = line.at(param.param, i);
                }
            }
        }
        if fading {
            let circular = param.param.is_circular();
            for (i, o) in out.iter_mut().enumerate() {
                let j = st.progress.done(declick) + Samples::new(i);
                if j >= declick {
                    break;
                }
                let (out, inn) = declick_gains::<f64>(j, declick);
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
            st.progress.advance(Samples::new(frames), declick);
        }
        st.last = out[frames - 1];
        st.modulated = true;
        // One store a block, only while the port is driven or fading: the
        // lane's last frame, unless it is not finite, which is never latched
        // (the read then answers the base).
        if let Some(lane) = self.lanes.get(k) {
            let last = st.last;
            if last.is_finite() {
                lane.deliver(last, st.shown != Shown::Driven);
                st.shown = Shown::Driven;
            } else if st.shown != Shown::Idle {
                lane.release();
                st.shown = Shown::Idle;
            }
        }
        framed[k] = Delivery::Driven;
        self.delivered.narrow_port(k, frames, param.param);
        self.busy = true;
        true
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
mod tests;
