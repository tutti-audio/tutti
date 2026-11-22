//! [`ControlProgram`]: what one target's control lane evaluates.
//!
//! A program is a description: a base (an automation curve, else the
//! target's knob) plus offsets (time-function [`Layer`]s and signal-driven
//! [`InputLayer`]s), each shaped by an [`Amount`] and scaled by the target's
//! span. It holds no clock and no state: equal programs evaluate to equal
//! bits at equal times, which is what lets a live lane and an export's fork of
//! it agree. Phase is a function of the time a lane hands in
//! ([`ProgramTime`]), never of the program's age, so no edit is a phase
//! reset. A route's depth may live in a cell ([`Param<Depth>`](tutti_rt::Param)) the program
//! reads at every evaluation, so moving it republishes nothing.
//!
//! Ungated: the floor, with [`Curve`]. The lane that evaluates it is
//! `tutti-nodes`' `ControlLaneNode`; the compiler that builds it from routes
//! is [`compile`](fn@crate::compile).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use tutti_curve::{Law, Piece};
use tutti_rt::{Param, SplitMix64};
use tutti_units::{
    Azimuth, Beat, BeatsPerCycle, Depth, Frame, Hz, Normalized, ParamId, Phase, PhaseIncrement,
    Real, SamplePosition, SampleRate, UnitParam,
};

use crate::curve::{Curve, CurveSegment};
use crate::shape::{LfoShape, Polarity, shape};

/// Time-function layers one program holds, at most: a lane keeps per-layer
/// state in arrays this long, so it never allocates on the audio thread.
/// [`ControlProgramBuilder::build`] refuses past it ([`ProgramError::TooManyLayers`]).
pub const MAX_LANE_LAYERS: usize = 16;

/// Event-input layers one program holds, at most; see [`MAX_LANE_LAYERS`].
/// [`ControlProgramBuilder::build`] refuses past it ([`ProgramError::TooManyInputs`]).
pub const MAX_LANE_INPUTS: usize = 8;

/// Depth cells one program holds, at most: one per layer and per input.
/// [`ControlProgram::bind_depths`] refuses past it ([`ProgramError::TooManyDepths`]).
pub const MAX_LANE_DEPTHS: usize = MAX_LANE_LAYERS + MAX_LANE_INPUTS;

/// The depths a program's cells held at one moment: what a lane loads once per
/// block ([`ControlProgram::depth_snapshot`]), compares with the last block's,
/// and evaluates from ([`ControlProgram::value_at_in`]), so one block never
/// evaluates at two depths. Fixed size: nothing allocates.
#[derive(Clone, Copy, Debug)]
pub struct DepthSnapshot {
    len: usize,
    depths: [Depth; MAX_LANE_DEPTHS],
}

impl DepthSnapshot {
    /// The snapshot of a program with no depth cells.
    pub const EMPTY: Self = Self {
        len: 0,
        depths: [Depth::NONE; MAX_LANE_DEPTHS],
    };
}

impl Default for DepthSnapshot {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Equal when every depth is the same bit for bit.
impl PartialEq for DepthSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len
            && self.depths[..self.len]
                .iter()
                .zip(&other.depths[..other.len])
                .all(|(a, b)| a.get().to_bits() == b.get().to_bits())
    }
}

impl Eq for DepthSnapshot {}

/// A layer's identity: what its sum order and its identity across a
/// republish are keyed by. A compiler derives it stably from what the layer
/// comes from (a route), so a republish keeps it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LayerId(pub u64);

/// A signal source's identity on a lane's event input: its events are
/// addressed `ParamAddr::Id(source.get())` and carry its raw value.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SourceId(ParamId);

impl SourceId {
    /// The source whose events are addressed `id`.
    #[must_use]
    pub const fn new(id: ParamId) -> Self {
        Self(id)
    }

    /// The id its events are addressed to.
    #[must_use]
    pub const fn get(self) -> ParamId {
        self.0
    }
}

/// The clock a layer's phase runs on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LayerClock {
    /// `cycles = beat / per_cycle + phase`: bar-locked; follows seeks and
    /// loop wraps; holds while the transport is stopped.
    Beats {
        /// Beats per cycle: a span, so larger is slower.
        per_cycle: BeatsPerCycle,
        /// Added to the cycle position.
        phase: PhaseIncrement,
    },
    /// `cycles = rate · play / sample rate + phase`, on the graph's play
    /// clock (frames since the last play start, running while stopped), or
    /// continued through `anchor` after a rate edit in the same play epoch.
    Seconds {
        /// Cycles per second. Negative runs the wave backwards.
        rate: Hz,
        /// Added to the cycle position (not to an anchored one: the anchor's
        /// phase already holds it).
        phase: PhaseIncrement,
        /// Where a rate edit takes effect, if one does.
        anchor: Option<RateAnchor>,
    },
}

/// Where a rate edit takes effect: in play epoch `epoch`, the
/// layer is at `phase` cycles on play frame `at`, and moves at `prev_rate`
/// before it and at the layer's rate from it. An anchor from another epoch
/// (a play start since, or a fork) is ignored.
///
/// `phase` is a bare `f64` of cycles, unwrapped: it runs past one cycle, where
/// a [`Phase`] stays in `[0, 1)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateAnchor {
    /// The play epoch the anchor is dated in.
    pub epoch: u32,
    /// The play frame it takes effect on.
    pub at: Frame,
    /// The layer's position there, in cycles (unwrapped).
    pub phase: f64,
    /// The rate before `at`.
    pub prev_rate: Hz,
}

/// One period of a layer's raw value, in `[-1, 1]`.
#[derive(Clone, Debug, PartialEq)]
pub enum Waveform {
    /// A closed-form shape. `Random` and `RandomSmooth` derive their steps
    /// from a hash of the cycle index and `seed`, so they replay after a
    /// seek and in a fork.
    Shape {
        /// The shape.
        shape: LfoShape,
        /// The random shapes' seed; ignored by the others.
        seed: u64,
    },
    /// One drawn period: breakpoints over `[0, 1)`, ascending, each with the
    /// curve of the piece it starts; the last piece runs to the first point
    /// of the next period.
    Drawn(Arc<[PeriodPoint]>),
    /// One value at every time, whatever the clock: a macro. It has no
    /// corners, and a layer of it does not move, so a lane whose layers are
    /// all constant is steady (one held value, no grid).
    Const(f64),
}

/// A breakpoint of a [`Waveform::Drawn`] period.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeriodPoint {
    /// Where in the period, in `[0, 1)` (a program with one outside is
    /// refused: `Phase::new(x)` does not wrap, and a point past the period's end
    /// is a mistake, not the next period's start).
    pub at: Phase,
    /// The raw value there, in `[-1, 1]`.
    pub value: f64,
    /// The curve of the piece from here to the next point.
    pub curve: Law,
}

/// How a raw value becomes an offset: `shape(raw, depth, polarity, curve) ×
/// span`, the span read from the target's live range by the lane each block
/// (so a range edit rescales without a republish).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Amount {
    /// A fraction of the target's range; negative inverts.
    pub depth: Depth,
    /// Whether the offset swings both ways or one.
    pub polarity: Polarity,
    /// The response curve on the magnitude.
    pub curve: Law,
}

impl Amount {
    /// The offset `raw` gives, in the target's units.
    #[inline]
    pub fn offset<R: Real + Into<f64>>(&self, raw: R, span: f64) -> f64 {
        shape(raw, self.depth, self.polarity, self.curve).into() * span
    }

    /// Whether the offset bends where the raw value crosses zero: a
    /// bipolar amount with any curve but `Linear` (`shape` curves `|x|`),
    /// so each zero crossing is a corner. Every curve has
    /// `f(0) = 0` (`Elastic` by a special case: its formula's limit is
    /// about −5e-4), so the offset `sign(x)·f(|x|)` is continuous there
    /// with the same slope `f'(0)` either side; where `f''(0) ≠ 0` its
    /// curvature flips sign, so the offset is not smooth across it and a
    /// piece ends there. A curve that is smooth through zero (`CubicIn` is
    /// `x³`) only gains a harmless extra corner.
    ///
    /// Not reported: `Stepped`'s jump at `|x| = 1` (at `raw = 1` when
    /// unipolar), a single point no piece can carry, and `Bounce`'s three
    /// slope breaks inside the range.
    #[inline]
    pub fn kinks_at_zero(&self) -> bool {
        self.polarity == Polarity::Bipolar && self.curve != Law::Linear
    }
}

/// A time-function layer: a wave on a clock, shaped into an offset.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// Its identity and sum order.
    pub id: LayerId,
    /// The clock its phase runs on.
    pub clock: LayerClock,
    /// Its raw value over one cycle.
    pub wave: Waveform,
    /// How the raw value becomes an offset.
    pub amount: Amount,
    /// The offset scaled by a second wave through its own [`Amount`] (depth
    /// moved by an LFO or a macro). `None`: unscaled.
    pub depth_by: Option<DepthBy>,
}

/// What scales a [`Layer`]'s offset: a second wave on its own clock, shaped
/// by its own amount, `m = shape(raw, amount)`. The offset is multiplied by
/// `m`.
///
/// [`DepthBy::UNIT`] (full depth, unipolar, linear) maps the wave's
/// `[-1, 1]` onto `[0, 1]`, computed as `(raw + 1) / 2` in `f64`; any other
/// amount goes through [`shape`]. A curved bipolar amount kinks at the
/// wave's zero crossings, which are corners as a layer's are.
/// Under `Back` or `Elastic` the scale dips below 0 (to about −0.10 and
/// −0.373 of depth) for small inputs, so there it inverts the offset
/// rather than shrinking it; so may a `Bezier` with control values
/// outside `[0, 1]`.
#[derive(Clone, Debug, PartialEq)]
pub struct DepthBy {
    /// The clock the scaling wave runs on.
    pub clock: LayerClock,
    /// The scaling wave.
    pub wave: Waveform,
    /// How its raw value becomes the scale.
    pub amount: Amount,
}

impl DepthBy {
    /// The amount that maps a wave's `[-1, 1]` onto `[0, 1]`.
    pub const UNIT: Amount = Amount {
        depth: Depth::FULL,
        polarity: Polarity::Unipolar,
        curve: Law::Linear,
    };

    /// The scale `raw` gives.
    #[inline]
    fn scale(&self, raw: f64) -> f64 {
        let a = self.amount;
        if a.polarity == Polarity::Unipolar && a.curve == Law::Linear {
            // `(raw + 1) / 2` in `f64`, and exactly that at full depth:
            // `1.0 ×` is exact.
            let u = (raw.clamp(-1.0, 1.0) + 1.0) * 0.5;
            a.depth.get() * u
        } else {
            a.offset(raw, 1.0)
        }
    }
}

/// A signal source's raw value (its events on the lane's event input,
/// addressed `ParamAddr::Id(source.get())`), shaped into an offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputLayer {
    /// Its identity and sum order (after every [`Layer`]).
    pub id: LayerId,
    /// The source whose events it reads.
    pub source: SourceId,
    /// How the source's raw value becomes an offset.
    pub amount: Amount,
}

/// The time a lane evaluates a program at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgramTime {
    /// The transport's beat (held while stopped).
    pub beat: Beat,
    /// The play clock, in frames since the last play start: fractional
    /// when the time is a corner between frames. A frame count rather than
    /// `Seconds`: a play position is exact in frames at any rate.
    pub play: SamplePosition,
    /// The play clock's epoch (a count of play starts).
    pub play_epoch: u32,
    /// The rate the play clock counts at.
    pub rate: SampleRate,
}

/// The next corner on each clock, strictly after a time
/// ([`ControlProgram::next_corner`]): where the sum's slope breaks or it
/// jumps.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Corners {
    /// The next corner of the beat-clocked layers and the automation. It
    /// arrives only while the beat moves.
    pub beat: Option<Beat>,
    /// The next corner of the seconds-clocked layers, on the play clock.
    pub play: Option<SamplePosition>,
}

/// Why [`ControlProgramBuilder::build`] refused a program.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProgramError {
    /// More than [`MAX_LANE_LAYERS`] layers.
    #[error("{count} layers, past MAX_LANE_LAYERS ({})", MAX_LANE_LAYERS)]
    TooManyLayers {
        /// How many there were.
        count: usize,
    },
    /// More than [`MAX_LANE_INPUTS`] event-input layers.
    #[error("{count} input layers, past MAX_LANE_INPUTS ({})", MAX_LANE_INPUTS)]
    TooManyInputs {
        /// How many there were.
        count: usize,
    },
    /// More than [`MAX_LANE_DEPTHS`] depth cells.
    #[error("{count} depth cells, past MAX_LANE_DEPTHS ({})", MAX_LANE_DEPTHS)]
    TooManyDepths {
        /// How many there were.
        count: usize,
    },
    /// Two layers (or two input layers) with one id.
    #[error("two layers share id {}", .0.0)]
    DuplicateLayer(LayerId),
    /// A drawn wave whose points are not ascending in `[0, 1)` with finite
    /// values, in this layer.
    #[error("layer {}: a drawn wave's points must ascend in [0, 1) with finite values", .0.0)]
    BadDrawnWave(LayerId),
    /// A NaN in this layer (a rate, a phase, a beat span, an
    /// anchor, a constant): a program is [`Eq`], and NaN equals nothing.
    #[error("layer {}: a NaN", .0.0)]
    NotANumber(LayerId),
}

/// What one target's control lane evaluates: a base, plus offsets.
///
/// ```text
/// value(t) = (automation(t) where it has a value, else knob) + Σ offsets(t)
/// ```
///
/// The clamp to the target's range is the target's (a native param port
/// clamps per frame) or the plugin lane's, never the program's. Values are
/// the target's raw `f64` (Hz, linear gain, a plugin's normalised 0..1): the
/// program is erased over the unit, as the lane's `ParamSegment`s are.
///
/// Built by [`ControlProgramBuilder`], which sorts the layers by id and refuses a
/// program past the lane's fixed per-layer state; the fields are private so
/// every program a lane reads holds those invariants.
///
/// ```
/// use tutti_modulation::{
///     Amount, LayerClock, ControlProgramBuilder, ControlProgram, Law, Layer, LayerId, LfoShape, Polarity,
///     ProgramTime, Waveform,
/// };
/// use tutti_units::{Beat, BeatsPerCycle, Depth, PhaseIncrement, SamplePosition, SampleRate};
///
/// // A bar-locked square, a quarter of the range deep, on a knob of 1000.
/// let square = Layer {
///     id: LayerId(1),
///     clock: LayerClock::Beats { per_cycle: BeatsPerCycle::of(4.0), phase: PhaseIncrement::new(0.0) },
///     wave: Waveform::Shape { shape: LfoShape::Square, seed: 0 },
///     amount: Amount { depth: Depth::of(0.25), polarity: Polarity::Bipolar, curve: Law::Linear },
///     depth_by: None,
/// };
/// let program = ControlProgramBuilder::new().layers([square]).build().unwrap();
/// let at = |beat| ProgramTime {
///     beat: Beat::new(beat),
///     play: SamplePosition::new(0.0),
///     play_epoch: 1,
///     rate: SampleRate::SR_48K,
/// };
/// // Range 0..800: a span of 800, so the offset is ±200.
/// assert_eq!(program.value_at(None, Some(1000.0), &at(1.0), 800.0, &[]), Some(1200.0));
/// assert_eq!(program.value_at(None, Some(1000.0), &at(3.0), 800.0, &[]), Some(800.0));
/// ```
#[derive(Clone, Default)]
pub struct ControlProgram {
    automation: Option<Arc<dyn Curve>>,
    layers: Box<[Layer]>,
    inputs: Box<[InputLayer]>,
    /// The depth cells of the layers and inputs that have one, by id.
    depths: Box<[(LayerId, Param<Depth>)]>,
}

impl fmt::Debug for ControlProgram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlProgram")
            .field("automation", &self.automation.as_ref().map(|_| "<curve>"))
            .field("layers", &self.layers)
            .field("inputs", &self.inputs)
            .field("depths", &self.depths.len())
            .finish()
    }
}

/// Equal when the layers are equal and the automation is the same curve
/// (`Arc` identity: a curve has no equality of its own). [`Eq`]:
/// [`build`](ControlProgramBuilder::build) refuses a NaN anywhere in a program, so
/// every program equals itself.
impl PartialEq for ControlProgram {
    fn eq(&self, other: &Self) -> bool {
        let same_curve = match (&self.automation, &other.automation) {
            (None, None) => true,
            (Some(a), Some(b)) => core::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)),
            _ => false,
        };
        same_curve
            && self.layers == other.layers
            && self.inputs == other.inputs
            && self.depths.len() == other.depths.len()
            && self
                .depths
                .iter()
                .zip(other.depths.iter())
                .all(|(a, b)| a.0 == b.0 && a.1.cell().ptr_eq(&b.1.cell()))
    }
}

impl Eq for ControlProgram {}

/// A tolerance in a clock's units (cycles, beats) at `x`: what "strictly
/// after" a corner and "just before" one mean, wide enough that a corner
/// computed from a time and that time recomputed from the corner land on
/// the same side, and far below a frame.
#[inline]
fn tol(x: f64) -> f64 {
    (x.abs() * 1e-12).max(1e-9)
}

/// A [`ControlProgram`] in the making: its automation, layers and inputs, checked together by
/// [`build`](Self::build).
///
/// Finishing sorts the layers and the inputs by id and refuses what no lane can run, so it is
/// a terminal call and not a constructor.
#[derive(Default)]
pub struct ControlProgramBuilder {
    automation: Option<Arc<dyn Curve>>,
    layers: Vec<Layer>,
    inputs: Vec<InputLayer>,
}

impl core::fmt::Debug for ControlProgramBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ControlProgramBuilder")
            .field("automation", &self.automation.is_some())
            .field("layers", &self.layers.len())
            .field("inputs", &self.inputs.len())
            .finish()
    }
}

impl ControlProgramBuilder {
    /// A builder of a program with no automation, no layers and no inputs.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The automation curve, taken as its [`frozen`](Curve::frozen) copy when it has one, so
    /// the lane never reads a cell through it.
    #[must_use]
    pub fn automation(mut self, automation: Arc<dyn Curve>) -> Self {
        self.automation = Some(automation);
        self
    }

    /// Adds `layers`.
    #[must_use]
    pub fn layers(mut self, layers: impl IntoIterator<Item = Layer>) -> Self {
        self.layers.extend(layers);
        self
    }

    /// Adds `inputs`.
    #[must_use]
    pub fn inputs(mut self, inputs: impl IntoIterator<Item = InputLayer>) -> Self {
        self.inputs.extend(inputs);
        self
    }

    /// The program, its layers and inputs each sorted by id.
    ///
    /// # Errors
    ///
    /// Past [`MAX_LANE_LAYERS`] or [`MAX_LANE_INPUTS`], two layers with one
    /// id, a drawn wave whose points do not ascend in `[0, 1)` with finite
    /// values, or a NaN in a layer.
    pub fn build(self) -> Result<ControlProgram, ProgramError> {
        let Self {
            automation,
            mut layers,
            mut inputs,
        } = self;
        if layers.len() > MAX_LANE_LAYERS {
            return Err(ProgramError::TooManyLayers {
                count: layers.len(),
            });
        }
        if inputs.len() > MAX_LANE_INPUTS {
            return Err(ProgramError::TooManyInputs {
                count: inputs.len(),
            });
        }
        layers.sort_by_key(|l| l.id);
        inputs.sort_by_key(|l| l.id);
        if let Some(w) = layers.windows(2).find(|w| w[0].id == w[1].id) {
            return Err(ProgramError::DuplicateLayer(w[0].id));
        }
        if let Some(w) = inputs.windows(2).find(|w| w[0].id == w[1].id) {
            return Err(ProgramError::DuplicateLayer(w[0].id));
        }
        for l in &layers {
            if l.has_nan() {
                return Err(ProgramError::NotANumber(l.id));
            }
            let waves = core::iter::once(&l.wave).chain(l.depth_by.as_ref().map(|d| &d.wave));
            for w in waves {
                if let Waveform::Drawn(points) = w {
                    let in_range = points
                        .iter()
                        .all(|p| (0.0..1.0).contains(&p.at.get()) && p.value.is_finite());
                    let ascending = points.windows(2).all(|w| w[0].at < w[1].at);
                    if !(in_range && ascending) {
                        return Err(ProgramError::BadDrawnWave(l.id));
                    }
                }
            }
        }
        Ok(ControlProgram {
            automation: automation.map(|c| c.frozen().unwrap_or(c)),
            layers: layers.into_boxed_slice(),
            inputs: inputs.into_boxed_slice(),
            depths: Box::new([]),
        })
    }
}

impl ControlProgram {
    /// Makes `depths` the depth of the layers and inputs they
    /// name, each read from its cell when a lane takes a
    /// [snapshot](Self::depth_snapshot); an id no layer or input has is
    /// ignored.
    ///
    /// # Errors
    ///
    /// More than [`MAX_LANE_DEPTHS`] cells, or two cells for one id.
    pub fn bind_depths(
        &mut self,
        depths: impl IntoIterator<Item = (LayerId, Param<Depth>)>,
    ) -> Result<(), ProgramError> {
        let mut depths: Vec<_> = depths
            .into_iter()
            .filter(|(id, _)| {
                self.layers.iter().any(|l| l.id == *id) || self.inputs.iter().any(|i| i.id == *id)
            })
            .collect();
        if depths.len() > MAX_LANE_DEPTHS {
            return Err(ProgramError::TooManyDepths {
                count: depths.len(),
            });
        }
        depths.sort_by_key(|(id, _)| *id);
        if let Some(w) = depths.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(ProgramError::DuplicateLayer(w[0].0));
        }
        self.depths = depths.into_boxed_slice();
        Ok(())
    }

    /// The program with a cell of its own for every depth cell it holds, at
    /// the depth now: what a fork evaluates.
    #[must_use]
    pub fn detached(&self) -> Self {
        let mut program = self.clone();
        program.depths = self
            .depths
            .iter()
            .map(|(id, cell)| {
                let mut cell = cell.handle();
                cell.detach();
                (*id, cell)
            })
            .collect();
        program
    }

    /// Each depth cell loaded once, in id order: a lane takes one per block,
    /// compares it with the last block's, and evaluates from it.
    #[must_use]
    pub fn depth_snapshot(&self) -> DepthSnapshot {
        let mut snapshot = DepthSnapshot::EMPTY;
        for (at, (_, cell)) in self.depths.iter().enumerate() {
            snapshot.depths[at] = cell.load();
        }
        snapshot.len = self.depths.len();
        snapshot
    }

    /// `id`'s amount with the depth `depths` holds for it, if it has a cell.
    #[inline]
    fn amount_of(&self, id: LayerId, amount: Amount, depths: &DepthSnapshot) -> Amount {
        match self.depths.binary_search_by_key(&id, |(id, _)| *id) {
            Ok(at) if at < depths.len => Amount {
                depth: depths.depths[at],
                ..amount
            },
            _ => amount,
        }
    }

    /// A program of automation alone: the curve where it has a value, the
    /// knob elsewhere.
    pub fn from_curve(curve: Arc<dyn Curve>) -> Self {
        Self {
            automation: Some(curve.frozen().unwrap_or(curve)),
            layers: Box::new([]),
            inputs: Box::new([]),
            depths: Box::new([]),
        }
    }

    /// The automation curve, if any.
    pub fn automation(&self) -> Option<&Arc<dyn Curve>> {
        self.automation.as_ref()
    }

    /// The time-function layers, in id order.
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// The event-input layers, in id order.
    pub fn inputs(&self) -> &[InputLayer] {
        &self.inputs
    }

    /// Exactly one curve's segments: automation, no layers, no inputs. A
    /// native target's lane sends such a program's shaped pieces as they
    /// are.
    #[must_use]
    pub fn is_pure_automation(&self) -> bool {
        self.automation.is_some() && self.layers.is_empty() && self.inputs.is_empty()
    }

    /// The value at `at` of the program driving `param` (`None`: a target of
    /// no tutti unit, a line), before any clamp: the automation's value where it
    /// has one (and it is finite), else `knob`, plus every layer's offset
    /// and every input's (`inputs[k]` is the raw value of
    /// [`inputs()`](Self::inputs)`[k]`'s source, `None` while it has not
    /// sent, which adds nothing).
    ///
    /// `span` is the target's `max − min`, which every offset scales by. A
    /// span that is not finite leaves the base alone (an unranged target
    /// takes no modulation). `None` only with no automation value and no
    /// knob.
    ///
    /// Pure and allocation-free. Offsets are summed in `f64` from `-0.0`, in
    /// id order (layers, then inputs): the value is the sum at `f64`, so a
    /// base with no offsets passes through to the bit.
    ///
    /// A circular `param` ([`UnitParam::is_circular`], `Azimuth`) moves along
    /// the short arc between an automation curve's breakpoints and takes its
    /// offsets as `wrap(base + offsets)`, never a clamp; `span` is then the
    /// arc's length (`ParamRange::span`). A line is unchanged,
    /// to the bit.
    pub fn value_at(
        &self,
        param: Option<UnitParam>,
        knob: Option<f64>,
        at: &ProgramTime,
        span: f64,
        inputs: &[Option<f64>],
    ) -> Option<f64> {
        self.value_at_in(param, knob, at, span, inputs, &self.depth_snapshot())
    }

    /// [`value_at`](Self::value_at) at the depths of `depths`, which a lane
    /// took once for its block.
    pub fn value_at_in(
        &self,
        param: Option<UnitParam>,
        knob: Option<f64>,
        at: &ProgramTime,
        span: f64,
        inputs: &[Option<f64>],
        depths: &DepthSnapshot,
    ) -> Option<f64> {
        self.evaluate(param, knob, at, span, inputs, depths, Limit::At)
    }

    /// The left limit at `at`: [`value_at`](Self::value_at) just before it,
    /// on every clock. It differs from `value_at` only where something
    /// jumps exactly at `at` (a square's edge, a random step, a breakpoint
    /// pair): what a piece ending at a corner aims at.
    pub fn value_before(
        &self,
        param: Option<UnitParam>,
        knob: Option<f64>,
        at: &ProgramTime,
        span: f64,
        inputs: &[Option<f64>],
    ) -> Option<f64> {
        self.value_before_in(param, knob, at, span, inputs, &self.depth_snapshot())
    }

    /// [`value_before`](Self::value_before) at the depths of `depths`.
    pub fn value_before_in(
        &self,
        param: Option<UnitParam>,
        knob: Option<f64>,
        at: &ProgramTime,
        span: f64,
        inputs: &[Option<f64>],
        depths: &DepthSnapshot,
    ) -> Option<f64> {
        self.evaluate(param, knob, at, span, inputs, depths, Limit::Before)
    }

    fn evaluate(
        &self,
        param: Option<UnitParam>,
        knob: Option<f64>,
        at: &ProgramTime,
        span: f64,
        inputs: &[Option<f64>],
        depths: &DepthSnapshot,
        limit: Limit,
    ) -> Option<f64> {
        let circular = param.filter(|p| p.is_circular());
        let base = self
            .automation
            .as_ref()
            .and_then(|c| automation_at(c.as_ref(), at.beat.get(), limit, circular))
            .filter(|v| v.is_finite())
            .or(knob)?;
        if !span.is_finite() {
            return Some(base);
        }
        let mut acc = -0.0f64;
        for l in self.layers.iter() {
            if let Some(o) = l.offset(at, span, limit, self.amount_of(l.id, l.amount, depths)) {
                acc += o;
            }
        }
        for (k, l) in self.inputs.iter().enumerate() {
            if let Some(x) = inputs.get(k).copied().flatten() {
                acc += self.amount_of(l.id, l.amount, depths).offset(x, span);
            }
        }
        let sum = base + acc;
        Some(circular.map_or(sum, |_| Azimuth::new(sum).wrap().get()))
    }

    /// The next corner strictly after `at` on each clock: the earliest of
    /// the automation's next breakpoint (where [`Curve::segment_at`] can
    /// say it), each layer's slope breaks and jumps (a square's edges, a
    /// saw's wrap, a triangle's peaks, a random step, a drawn point), each
    /// curved bipolar layer's zero crossings, each
    /// `depth_by` wave's, and a rate anchor's frame. Inputs' corners are the
    /// lane's: it holds their segments.
    pub fn next_corner(&self, at: &ProgramTime) -> Corners {
        let mut out = Corners::default();
        let mut beat = |b: f64| {
            if b.is_finite() && out.beat.is_none_or(|o| b < o.get()) {
                out.beat = Some(Beat::new(b));
            }
        };
        if let Some(c) = &self.automation {
            let b = at.beat.get();
            let next = match c.segment_at(at.beat) {
                Some(CurveSegment::Move { end, .. }) => Some(end.get()),
                Some(CurveSegment::Hold { until, .. }) => until.map(Beat::get),
                None => None,
            };
            if let Some(n) = next.filter(|&n| n > b + tol(b)) {
                beat(n);
            }
        }
        let mut corners = [Corners::default(); 2];
        for l in self.layers.iter() {
            corners[0] = l.clock.next_corner(at, &l.wave, l.amount.kinks_at_zero());
            corners[1] = l.depth_by.as_ref().map_or(Corners::default(), |d| {
                d.clock.next_corner(at, &d.wave, d.amount.kinks_at_zero())
            });
            for c in corners {
                if let Some(b) = c.beat {
                    beat(b.get());
                }
                if let Some(p) = c.play
                    && p.get().is_finite()
                    && out.play.is_none_or(|o| p.get() < o.get())
                {
                    out.play = Some(p);
                }
            }
        }
        out
    }
}

/// Which side of a time a value is read from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Limit {
    /// The value at the time.
    At,
    /// The left limit: the value just before the time.
    Before,
}

/// `curve`'s value at beat `b`, or just before it for [`Limit::Before`].
///
/// A left limit inside a piece the curve can describe is evaluated from
/// the piece, a nanobeat before its end, so its progress is below 1 and a
/// stepped piece reads the level before the step.
fn automation_at(
    curve: &dyn Curve,
    b: f64,
    limit: Limit,
    circular: Option<UnitParam>,
) -> Option<f64> {
    if limit == Limit::At {
        // A circular param's move is the short arc, which the curve's own
        // `value_at` (a straight line between values) does not know.
        if let Some(p) = circular
            && let Some(CurveSegment::Move {
                start,
                end,
                from,
                to,
                shape,
            }) = curve.segment_at(Beat::new(b))
            && end.get() > start.get()
        {
            let t = Normalized::clamped((Beat::new(b) - start) / (end - start));
            return Some(Piece::new(from, to).law(shape).value(Some(p), t));
        }
        return curve.value_at(Beat::new(b));
    }
    let x = b - tol(b);
    if let Some(CurveSegment::Move {
        start,
        end,
        from,
        to,
        shape,
    }) = curve.segment_at(Beat::new(x))
        && end > start
    {
        let t = Normalized::clamped((Beat::new(x) - start) / (end - start));
        return Some(Piece::new(from, to).law(shape).value(circular, t));
    }
    curve.value_at(Beat::new(x))
}

impl Layer {
    /// Whether this layer's offset is the same at every time: a constant
    /// wave, scaled by nothing or by another constant. A lane whose layers
    /// are all constant (and with no automation) does not walk the grid.
    #[must_use]
    pub fn is_constant(&self) -> bool {
        matches!(self.wave, Waveform::Const(_))
            && self
                .depth_by
                .as_ref()
                .is_none_or(|d| matches!(d.wave, Waveform::Const(_)))
    }

    /// This layer's offset at `at` (just before it for [`Limit::Before`]),
    /// `None` when its clock gives no position.
    fn offset(&self, at: &ProgramTime, span: f64, limit: Limit, amount: Amount) -> Option<f64> {
        let raw = self.wave.raw_on(&self.clock, at, limit)?;
        let mut off = amount.offset(raw, span);
        if let Some(d) = &self.depth_by {
            let m = d
                .wave
                .raw_on(&d.clock, at, limit)
                .map_or(1.0, |r| d.scale(r));
            off *= m;
        }
        Some(off)
    }

    /// Whether a NaN is anywhere in the layer.
    fn has_nan(&self) -> bool {
        let clock = |c: &LayerClock| match *c {
            LayerClock::Beats { phase, .. } => phase.get().is_nan(),
            LayerClock::Seconds {
                rate,
                phase,
                anchor,
            } => {
                rate.get().is_nan()
                    || phase.get().is_nan()
                    || anchor.is_some_and(|a| a.phase.is_nan() || a.prev_rate.get().is_nan())
            }
        };
        let wave = |w: &Waveform| matches!(w, Waveform::Const(v) if v.is_nan());
        clock(&self.clock)
            || wave(&self.wave)
            || self
                .depth_by
                .as_ref()
                .is_some_and(|d| clock(&d.clock) || wave(&d.wave))
    }
}

impl LayerClock {
    /// The position at `at`, in unwrapped cycles; `None` when the clock
    /// gives none (a rate that is not finite).
    pub fn cycles(&self, at: &ProgramTime) -> Option<f64> {
        let c = match *self {
            Self::Beats { per_cycle, phase } => at.beat.cycles_in(per_cycle) + phase.get(),
            Self::Seconds {
                rate,
                phase,
                anchor,
            } => {
                let sr = at.rate.get();
                if !(sr > 0.0 && sr.is_finite()) {
                    return None;
                }
                let play = at.play.get();
                match anchor.filter(|a| a.epoch == at.play_epoch) {
                    Some(a) => {
                        let d = play - a.at.get() as f64;
                        let r = if d < 0.0 { a.prev_rate } else { rate };
                        a.phase + r.get() * d / sr
                    }
                    None => rate.get() * play / sr + phase.get(),
                }
            }
        };
        c.is_finite().then_some(c)
    }

    /// The next corner of `wave` on this clock strictly after `at` (with
    /// zero crossings when `kinks`), and an anchor's own frame, where the
    /// rate changes.
    fn next_corner(&self, at: &ProgramTime, wave: &Waveform, kinks: bool) -> Corners {
        let Some(c) = self.cycles(at) else {
            return Corners::default();
        };
        match *self {
            Self::Beats { per_cycle, phase } => Corners {
                beat: wave
                    .next_corner(c, true, kinks)
                    .map(|n| Beat::new((n - phase.get()) * per_cycle.get())),
                play: None,
            },
            Self::Seconds {
                rate,
                phase,
                anchor,
            } => {
                let sr = at.rate.get();
                let play = at.play.get();
                // The play frame at which a rate `r` from (`p0`, `c0`)
                // reaches the wave's next corner.
                let along = |r: Hz, p0: f64, c0: f64| {
                    let r = r.get();
                    if r == 0.0 || !r.is_finite() {
                        return None;
                    }
                    wave.next_corner(c, r > 0.0, kinks)
                        .map(|n| p0 + (n - c0) * sr / r)
                };
                let p = match anchor.filter(|a| a.epoch == at.play_epoch) {
                    Some(a) => {
                        let t = a.at.get() as f64;
                        if play < t - tol(t) {
                            // The anchor's frame is a corner of its own.
                            Some(along(a.prev_rate, t, a.phase).map_or(t, |p| p.min(t)))
                        } else {
                            along(rate, t, a.phase)
                        }
                    }
                    None => along(rate, 0.0, phase.get()),
                };
                Corners {
                    beat: None,
                    play: p.map(SamplePosition::new),
                }
            }
        }
    }
}

/// `hash_bipolar` of cycle `n` under `seed`: seed 0 is the plain sequence.
#[inline]
fn hash_cycle(n: f64, seed: u64) -> f64 {
    hash_bipolar((n as i64).wrapping_add(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) as i64))
}

/// Map an integer index to a stable pseudo-random value in `[-1, 1)`
/// ([`SplitMix64::mix`]).
///
/// Addressable rather than sequential: the value for index `n` is
/// computable without having produced `n-1`, which is what lets a random
/// wave be a pure function of its clock, replayed after a seek and in a
/// fork.
#[inline]
pub(crate) fn hash_bipolar(n: i64) -> f64 {
    let z = SplitMix64::mix(n as u64);
    // Top 53 bits → [0, 1) → [-1, 1).
    ((z >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
}

impl Waveform {
    /// The raw value on `clock` at `at` (just before it for [`Limit::Before`]);
    /// `None` when the clock gives no position. A constant needs none.
    fn raw_on(&self, clock: &LayerClock, at: &ProgramTime, limit: Limit) -> Option<f64> {
        match self {
            Self::Const(v) => Some(*v),
            _ => Some(self.raw_limit(clock.cycles(at)?, limit)),
        }
    }

    /// The raw value at `cycles` (just before it when `left`), an unwrapped
    /// position: the integer part is the cycle index a random shape hashes,
    /// the `floor`-based fraction the phase. A pure function of its address,
    /// so a seek or a fork reads the same value; the control lane and the
    /// audio-rate `tutti_nodes::LfoNode` both evaluate through it.
    pub fn raw_at(&self, cycles: f64, left: bool) -> f64 {
        let limit = if left { Limit::Before } else { Limit::At };
        self.raw_limit(cycles, limit)
    }

    fn raw_limit(&self, cycles: f64, limit: Limit) -> f64 {
        let before = limit == Limit::Before;
        let c = if before { cycles - tol(cycles) } else { cycles };
        let n = libm::floor(c);
        let frac = c - n;
        match self {
            Self::Shape { shape, seed } => match shape {
                LfoShape::Random => hash_cycle(n, *seed),
                LfoShape::RandomSmooth => {
                    let (a, b) = (hash_cycle(n - 1.0, *seed), hash_cycle(n, *seed));
                    a + (b - a) * frac
                }
                periodic => periodic.evaluate_periodic(Phase::new(frac.min(1.0))),
            },
            Self::Drawn(points) => drawn_value(points, frac),
            Self::Const(v) => *v,
        }
    }

    /// The next corner strictly after `c` (`forward`) or before it, in
    /// cycles, with zero crossings when `kinks`. `None` for a wave with
    /// none (a sine without kinks, an empty drawing).
    fn next_corner(&self, c: f64, forward: bool, kinks: bool) -> Option<f64> {
        let n = libm::floor(c);
        let t = tol(c);
        let mut best: Option<f64> = None;
        let mut consider = |x: f64| {
            if forward {
                if x > c + t && best.is_none_or(|b| x < b) {
                    best = Some(x);
                }
            } else if x < c - t && best.is_none_or(|b| x > b) {
                best = Some(x);
            }
        };
        let cycles = if forward {
            [n, n + 1.0, n + 2.0]
        } else {
            [n - 2.0, n - 1.0, n]
        };
        for k in cycles {
            self.corners_in(k, kinks, &mut consider);
        }
        best
    }

    /// Every corner of cycle `k`, in cycles.
    fn corners_in(&self, k: f64, kinks: bool, f: &mut impl FnMut(f64)) {
        match self {
            Self::Shape { shape, seed } => match shape {
                LfoShape::Sine => {
                    if kinks {
                        f(k);
                        f(k + 0.5);
                    }
                }
                LfoShape::Triangle => {
                    f(k + 0.25);
                    f(k + 0.75);
                    if kinks {
                        f(k);
                        f(k + 0.5);
                    }
                }
                LfoShape::Square => {
                    f(k);
                    f(k + 0.5);
                }
                LfoShape::Sawtooth | LfoShape::SawtoothDown => {
                    f(k);
                    if kinks {
                        f(k + 0.5);
                    }
                }
                LfoShape::Random => f(k),
                LfoShape::RandomSmooth => {
                    f(k);
                    if kinks {
                        let (a, b) = (hash_cycle(k - 1.0, *seed), hash_cycle(k, *seed));
                        if (a < 0.0 && b > 0.0) || (a > 0.0 && b < 0.0) {
                            f(k + a / (a - b));
                        }
                    }
                }
            },
            Self::Drawn(points) => {
                let len = points.len();
                for (i, p) in points.iter().enumerate() {
                    let start = k + p.at.get();
                    f(start);
                    if kinks && len > 1 {
                        let (q, end) = if i + 1 < len {
                            (points[i + 1], k + points[i + 1].at.get())
                        } else {
                            (points[0], k + 1.0 + points[0].at.get())
                        };
                        if let Some(x) = crossing(p.curve, p.value, q.value) {
                            f(start + x * (end - start));
                        }
                    }
                }
            }
            Self::Const(_) => {}
        }
    }
}

/// The drawn period's value at `frac` in `[0, 1)`.
fn drawn_value(points: &[PeriodPoint], frac: f64) -> f64 {
    let Some(first) = points.first() else {
        return 0.0;
    };
    let last = points[points.len() - 1];
    if points.len() == 1 {
        return first.value;
    }
    let after = points.partition_point(|p| p.at.get() <= frac);
    let (p, start, q, end) = if after == 0 {
        // Before the first point: the last piece, wrapped.
        (last, last.at.get() - 1.0, *first, first.at.get())
    } else if after == points.len() {
        (last, last.at.get(), *first, 1.0 + first.at.get())
    } else {
        let p = points[after - 1];
        let q = points[after];
        (p, p.at.get(), q, q.at.get())
    };
    let t = Normalized::clamped((frac - start) / (end - start)).get();
    p.value + (q.value - p.value) * p.curve.eval_in(t)
}

/// Where in `(0, 1)` a `curve` piece from `a` to `b` crosses zero, if its
/// ends have opposite signs: in closed form for a line, by bisection on the
/// curve otherwise (deterministic: a fixed number of halvings).
fn crossing(curve: Law, a: f64, b: f64) -> Option<f64> {
    if !((a < 0.0 && b > 0.0) || (a > 0.0 && b < 0.0)) {
        return None;
    }
    if curve == Law::Linear {
        return Some(a / (a - b));
    }
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    let below = a < 0.0;
    for _ in 0..30 {
        let mid = 0.5 * (lo + hi);
        let v = Piece::new(a, b)
            .law(curve)
            .value(None, Normalized::clamped(mid));
        if (v < 0.0) == below {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(0.5 * (lo + hi))
}

#[cfg(test)]
mod tests;
