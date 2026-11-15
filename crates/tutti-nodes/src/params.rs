//! One reader for the cells of every [`FamilyNode`](crate::FamilyNode) family.
//!
//! A family writes a [`ControlSet`]: its addresses, and the conversion between its
//! `Controls` and the `f32`s the ports carry. [`Params`] is the one reader over it,
//! with one rule for every control: the lane's value at
//! the solve frame when a lane drives it, else a glide from the controls last
//! designed to the cell's value, each read through [`LastGood`]. Sanitising
//! (floors, clamps, whole-set validation) is the design's, never the reader's.
//!
//! A control the family reads **per frame** is [`ControlRead::Fast`]: the
//! recurrence takes its port's frames from the [`Inputs`](tutti_dsp::Inputs) lanes
//! (`Params::lanes`), and the reader sees only its cell, the base the member is
//! designed at.

use std::sync::Arc;
use std::sync::atomic::Ordering::{Acquire, Relaxed};

use tutti_dsp::{GAIN_GLIDE, Glide, MAX_LANES};
use tutti_graph::{Io, ParamSetBuilder};
use tutti_types::{AtomicF32, SampleRate, UnitAddr};

use crate::ramp::LastGood;

/// The most controls a family has: the compressor's six, the gate's five and the
/// limiter's three fit with room.
const MAX_CONTROLS: usize = 16;

/// A family's controls as a list of `f32` cells.
pub trait ControlSet: Clone + core::fmt::Debug + Send + 'static {
    /// The design's own `Controls`.
    type Controls: Copy + PartialEq + Send;

    /// The addresses of the cells, in port order after the blend's; its length is
    /// the number of controls [`flatten`](Self::flatten) writes.
    fn addrs(&self) -> &'static [UnitAddr];

    /// The controls a non-finite cell falls back to.
    fn defaults(&self) -> Self::Controls;

    /// `c` as one `f32` per address (`out.len() == addrs().len()`).
    fn flatten(&self, c: &Self::Controls, out: &mut [f32]);

    /// The controls of one `f32` per address: the inverse of
    /// [`flatten`](Self::flatten).
    fn assemble(&self, v: &[f32]) -> Self::Controls;

    /// The controls that ring longest, given each one's lowest and highest value
    /// in the block. Only a design that parks asks; the default is the highest.
    fn longest(&self, _lowest: &Self::Controls, highest: &Self::Controls) -> Self::Controls {
        *highest
    }

    /// How the reader takes control `k` (`k < addrs().len()`): whether it glides, lands
    /// whole or is read per frame, and which values it accepts. [`Control::GLIDE`] by
    /// default; `control_set!` writes it from each row's flags, so a control is
    /// declared once.
    fn control(&self, _k: usize) -> Control {
        Control::GLIDE
    }

    /// Whether `target` is far enough from `applied`, the controls the model was last
    /// designed from, to design again: the dead-band. Exact inequality by default.
    fn moved(&self, applied: &Self::Controls, target: &Self::Controls) -> bool {
        applied != target
    }
}

/// How the reader takes a control from its cell and its port.
#[derive(Clone, Copy, Debug, PartialEq, Hash, Eq)]
pub enum ControlRead {
    /// A moved cell glides on the stream clock over [`GAIN_GLIDE`], from where the
    /// control stood on the first frame of the block that first sees the write, or its
    /// port is read at the solve frame when a lane drives it.
    Glide,
    /// As [`Glide`](Self::Glide), but a moved cell lands whole: a control whose glide
    /// is the recurrence's own (a detector's) is not ramped by the reader.
    Whole,
    /// Read **per frame** by the recurrence, from its port, into a
    /// [`Inputs`](tutti_dsp::Inputs) lane (the fast controls fill the lanes in row order).
    /// The reader takes only its cell, the base the member is designed at, and a
    /// frame nobody drives is clamped at that member. A fast control lands whole.
    Fast,
}

/// The values a control accepts: what it does not is read as the last value it
/// accepted (`LastGood`).
#[derive(Clone, Copy, Debug, PartialEq, Hash, Eq)]
pub enum ControlValid {
    /// Finite values.
    Finite,
    /// Everything but NaN: a ceiling in dB is valid at `+-inf`.
    NotNan,
}

/// How the reader takes one control: what a `control_set!` row declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Control {
    /// How it is read.
    pub read: ControlRead,
    /// Which values it accepts.
    pub valid: ControlValid,
}

impl Control {
    /// A control that glides and accepts finite values: a row with no flags.
    pub const GLIDE: Self = Self {
        read: ControlRead::Glide,
        valid: ControlValid::Finite,
    };

    /// The row flag `whole`: a moved cell lands whole.
    #[must_use]
    pub(crate) const fn whole(self) -> Self {
        Self {
            read: ControlRead::Whole,
            ..self
        }
    }

    /// The row flag `fast`: read per frame from an [`Inputs`](tutti_dsp::Inputs) lane.
    #[must_use]
    pub(crate) const fn fast(self) -> Self {
        Self {
            read: ControlRead::Fast,
            ..self
        }
    }

    /// The row flag `not_nan`: accepts everything but NaN.
    #[must_use]
    pub(crate) const fn not_nan(self) -> Self {
        Self {
            valid: ControlValid::NotNan,
            ..self
        }
    }

    /// Whether the control takes `v`.
    #[must_use]
    pub(crate) fn accepts(self, v: f32) -> bool {
        match self.valid {
            ControlValid::Finite => v.is_finite(),
            ControlValid::NotNan => !v.is_nan(),
        }
    }

    /// How many of `controls` are [`ControlRead::Fast`]; `const`, for the macro's check
    /// against [`MAX_LANES`].
    #[must_use]
    pub(crate) const fn fast_count(controls: &[Self]) -> usize {
        let (mut i, mut n) = (0, 0);
        while i < controls.len() {
            if matches!(controls[i].read, ControlRead::Fast) {
                n += 1;
            }
            i += 1;
        }
        n
    }
}

/// That a [`ControlSet`] has a control of the kind `P` (a packed
/// [`UnitAddr`] at index 0), in the unit `Unit`: what
/// `control_set!` writes from the family's own `Controls` fields, and what
/// the typed key of a [`FamilyNode`](crate::FamilyNode) reads
/// ([`HasParam`](tutti_graph::HasParam)).
pub trait HasControl<const P: u16> {
    /// The unit the control's value is in.
    type Unit: tutti_graph::ParamUnit;
}

/// That a [`ControlSet`] has several controls of the kind `P`, numbered from 0.
pub trait HasControls<const P: u16>: HasControl<P> {}

/// Writes a family's [`ControlSet`] from its `Controls` fields, once: the
/// addresses, the conversion to the `f32`s the ports carry and back, and the
/// unit of each control ([`HasControl`]). The unit a row states is checked
/// against the field's own type (`flatten` reads it as that unit), so a
/// mismatch does not compile.
///
/// ```text
/// control_set! {
///     SvfSet(s): SvfControls = SvfControls { ty: s.0, ..SvfControls::DEFAULT };
///     live: (all);
///     Cutoff => Hz = freq,
///     Q => Q = q,
///     GainDb => Db = gain;
///     // the rest of the `ControlSet` impl:
///     fn defaults(&self) -> SvfControls { SvfControls::DEFAULT }
/// }
/// ```
///
/// The first line names the set, its binding (`s`, for the next expressions)
/// and its `Controls`, with the controls [`assemble`](ControlSet::assemble)
/// starts from (the parts the ports do not carry). `live` is how many of the
/// rows the set has (`all`, or an expression: a design that cannot freeze
/// leaves off the freeze). A row is `Kind => Unit = field`, a single control;
/// `Kind[0] => Unit = field[0]` the first of several of a kind (`HasControls`),
/// and `Kind[i]` with `i` above 0 the others. A row may end in flags, `{ .. }`, that
/// say how the reader takes it ([`Control`]): `whole` (lands whole, no glide), `fast`
/// (read per frame into an [`Inputs`](tutti_dsp::Inputs) lane, in row order; more fast rows
/// than [`MAX_LANES`] do not compile) and `not_nan` (accepts `+-inf`). A flag that is
/// not one of these does not compile.
macro_rules! control_set {
    (
        $set:ident($s:ident): $controls:ty = $base:expr;
        live: ($($live:tt)+);
        $($kind:ident $([$ix:tt])? => $unit:ty = $f:ident $([$pi:literal])?
            $({ $($flag:ident),* })?),+ ;
        $($items:tt)*
    ) => {
        impl $crate::params::ControlSet for $set {
            type Controls = $controls;

            #[allow(unused_variables)]
            fn addrs(&self) -> &'static [tutti_types::UnitAddr] {
                const ALL: &[tutti_types::UnitAddr] = &[$(
                    control_set!(@addr $kind $([$ix])?)
                ),+];
                let $s = self;
                &ALL[..control_set!(@live ALL, $($live)+)]
            }

            fn control(&self, k: usize) -> $crate::params::Control {
                const ALL: &[$crate::params::Control] = &[$(
                    $crate::params::Control::GLIDE $($(.$flag())*)?
                ),+];
                const _: () = assert!(
                    $crate::params::Control::fast_count(ALL) <= tutti_dsp::MAX_LANES,
                    "more fast controls than an Inputs has lanes",
                );
                ALL[k]
            }

            fn flatten(&self, c: &$controls, out: &mut [f32]) {
                let all = [$(
                    <$unit as tutti_graph::ParamUnit>::to_cell(c.$f $([$pi])?)
                ),+];
                out.copy_from_slice(&all[..out.len()]);
            }

            #[allow(unused_variables)]
            fn assemble(&self, v: &[f32]) -> $controls {
                let $s = self;
                let mut c: $controls = $base;
                let mut cells = v.iter().copied();
                $(
                    if let Some(x) = cells.next() {
                        c.$f $([$pi])? = <$unit as tutti_graph::ParamUnit>::from_cell(x);
                    }
                )+
                c
            }

            $($items)*
        }
        $( control_set!(@has $set; $kind $([$ix])? => $unit); )+
    };
    (@addr $kind:ident) => { tutti_types::UnitAddr::first(tutti_types::UnitParam::$kind) };
    (@addr $kind:ident [$ix:tt]) => {
        tutti_types::UnitAddr::new(tutti_types::UnitParam::$kind, $ix)
    };
    (@live $all:ident, all) => { $all.len() };
    (@live $all:ident, $($live:tt)+) => { $($live)+ };
    (@has $set:ident; $kind:ident => $unit:ty) => {
        impl $crate::params::HasControl<{ tutti_types::UnitParam::$kind as u16 }> for $set {
            type Unit = $unit;
        }
    };
    (@has $set:ident; $kind:ident [0] => $unit:ty) => {
        impl $crate::params::HasControl<{ tutti_types::UnitParam::$kind as u16 }> for $set {
            type Unit = $unit;
        }
        impl $crate::params::HasControls<{ tutti_types::UnitParam::$kind as u16 }> for $set {}
    };
    (@has $set:ident; $kind:ident [$ix:tt] => $unit:ty) => {};
}

/// Where in a block a node reads its controls.
#[derive(Clone, Copy, Debug)]
pub struct At {
    /// The stream frame the block begins on, what a cell's glide is counted on.
    pub position: u64,
    /// The frame of the block to read at.
    pub frame: usize,
    /// The rate the node runs at, for a control that is only valid against it.
    pub rate: SampleRate,
}

/// A [`ControlRead::Glide`] cell's glide on the stream clock: from where the control
/// stood to the cell's value, over the frames from `start`.
#[derive(Clone, Copy, Debug)]
struct CellGlide {
    glide: Glide<f32>,
    /// The stream frame of the glide's first frame, which reads step 1.
    start: u64,
}

impl CellGlide {
    /// Landed on `value` at stream frame `start`.
    fn landed(value: f32, start: u64) -> Self {
        Self {
            glide: Glide::landed(value),
            start,
        }
    }

    /// The control's value at stream frame `position`.
    fn at(&self, position: u64) -> f32 {
        let i = position.saturating_sub(self.start);
        self.glide
            .at(u32::try_from(i).map_or(u32::MAX, |i| i.saturating_add(1)))
    }
}

/// The cells of a [`ControlSet`] and how a block reads them.
#[derive(Clone, Debug)]
pub struct Params<S: ControlSet> {
    set: S,
    cells: Vec<Arc<AtomicF32>>,
    /// The last finite value each control held: a non-finite write reads as
    /// unchanged, so it never reaches the design.
    good: Vec<LastGood>,
    /// How each control is read, from [`ControlSet::control`].
    controls: [Control; MAX_CONTROLS],
    /// The fast controls, by index, in the order of the lanes they fill.
    fast: [usize; MAX_LANES],
    /// How many of `fast` are filled.
    fast_len: usize,
    /// Each [`ControlRead::Glide`] cell's glide on the stream clock.
    glides: [CellGlide; MAX_CONTROLS],
    /// A glide's length in frames: [`GAIN_GLIDE`] at the prepared rate, or none.
    glide: usize,
    /// Whether the glides have a value to start from: not after a prepare or a reset,
    /// when a cell lands whole.
    primed: bool,
}

impl<S: ControlSet> Params<S> {
    /// Cells of `set` holding `initial`. Control side; allocates.
    ///
    /// # Panics
    ///
    /// If `set` has more [`ControlRead::Fast`] controls than an [`Inputs`](tutti_dsp::Inputs) has
    /// lanes ([`MAX_LANES`]): only a hand-written [`ControlSet`] can, since
    /// `control_set!` refuses it at compile time.
    #[must_use]
    pub fn new(set: S, initial: &S::Controls) -> Self {
        let n = set.addrs().len();
        let (mut value, mut fallback) = ([0.0; MAX_CONTROLS], [0.0; MAX_CONTROLS]);
        set.flatten(initial, &mut value[..n]);
        set.flatten(&set.defaults(), &mut fallback[..n]);
        let (mut cells, mut good) = (Vec::with_capacity(n), Vec::with_capacity(n));
        let mut controls = [Control::GLIDE; MAX_CONTROLS];
        let (mut fast, mut fast_len) = ([0; MAX_LANES], 0);
        for k in 0..n {
            controls[k] = set.control(k);
            if controls[k].read == ControlRead::Fast {
                assert!(
                    fast_len < MAX_LANES,
                    "more fast controls than an Inputs has lanes"
                );
                fast[fast_len] = k;
                fast_len += 1;
            }
            let ok = controls[k].accepts(value[k]);
            let mut g = LastGood::new_if(value[k], fallback[k], ok);
            cells.push(Arc::new(AtomicF32::new(g.read_if(value[k], ok))));
            good.push(g);
        }
        Self {
            set,
            cells,
            good,
            controls,
            fast,
            fast_len,
            glides: [CellGlide::landed(0.0, 0); MAX_CONTROLS],
            glide: 1,
            primed: false,
        }
    }

    /// Takes the rate a cell's glide is measured at, and whether a moved cell glides
    /// at all: a family whose realization glides its own coefficients between block
    /// designs (a block cadence) lands it whole. Every cell lands whole on the next
    /// block, as after [`land`](Self::land).
    pub(crate) fn retune(&mut self, rate: SampleRate, glides: bool) {
        self.glide = if glides {
            GAIN_GLIDE.to_samples(rate).get().max(1)
        } else {
            1
        };
        self.land();
    }

    /// Back to no glide: every cell lands whole on the next block (a prepare, a reset).
    pub(crate) fn land(&mut self) {
        self.primed = false;
    }

    /// The block beginning at stream frame `position`: each [`ControlRead::Glide`]
    /// cell's value, read once for the block, and a glide started from where the control
    /// stands to it if it moved. A glide is on the stream clock, so it is the same
    /// whatever the blocks: it starts on the first frame of the block the write is first
    /// seen in, and arrives [`GAIN_GLIDE`] later.
    pub(crate) fn begin(&mut self, position: u64) {
        for k in 0..self.cells.len() {
            let control = self.controls[k];
            if control.read != ControlRead::Glide {
                continue;
            }
            let x = self.cells[k].load(Relaxed);
            let to = self.good[k].read_if(x, control.accepts(x));
            let glide = &mut self.glides[k];
            if !self.primed {
                *glide = CellGlide::landed(to, position);
            } else if to.to_bits() != glide.glide.target().to_bits() {
                // Where it stood on the frame before this block, the last one rendered.
                let from = glide.at(position.saturating_sub(1));
                let mut aimed = Glide::landed(from);
                aimed.aim_in(to, u32::try_from(self.glide).unwrap_or(u32::MAX));
                *glide = CellGlide {
                    glide: aimed,
                    start: position,
                };
            }
        }
        self.primed = true;
    }

    /// Each control's lowest and highest value in the block (the cell's where no
    /// lane drives it), or `None` when no lane does.
    fn extremes(&self, io: &Io<'_>) -> Option<[[f32; MAX_CONTROLS]; 2]> {
        let mut out = [[0.0; MAX_CONTROLS]; 2];
        let mut driven = false;
        for (k, cell) in self.cells.iter().enumerate() {
            let lane = io.param(k).frames();
            driven |= lane.is_some();
            let base = cell.load(Acquire);
            let fold = |pick: fn(f32, f32) -> f32| {
                lane.into_iter()
                    .flatten()
                    .copied()
                    .filter(|v| v.is_finite())
                    .fold(base, pick)
            };
            out[0][k] = fold(f32::min);
            out[1][k] = fold(f32::max);
        }
        driven.then_some(out)
    }

    /// The addresses of the cells, in port order.
    pub(crate) fn addrs(&self) -> &'static [UnitAddr] {
        self.set.addrs()
    }

    /// Adds each cell to `set` under its address.
    pub(crate) fn bind(&self, set: ParamSetBuilder) -> ParamSetBuilder {
        let cells = self.cells.iter().cloned();
        self.addrs()
            .iter()
            .zip(cells)
            .fold(set, |set, (&addr, cell)| set.param(addr, cell))
    }

    /// The base of the `k`th address ([`Node::param_base`](tutti_graph::Node::param_base)): its cell's value.
    pub(crate) fn base(&self, k: usize) -> Option<f32> {
        self.cells.get(k).map(|c| c.load(Acquire))
    }

    /// The controls at `at.frame`: each cell, or the value its port carries when the
    /// graph modulates it, read through [`LastGood`] so a value the control refuses
    /// ([`Control::accepts`]: non-finite by default) reads as unchanged. A
    /// [`ControlRead::Glide`] cell is its glide at that frame ([`begin`](Self::begin)),
    /// a [`ControlRead::Fast`] control is its cell whatever its port carries, and a
    /// [`ControlRead::Whole`] one is its cell.
    pub(crate) fn read(&mut self, io: &Io<'_>, at: At) -> S::Controls {
        let n = self.cells.len();
        let mut value = [0.0; MAX_CONTROLS];
        for (k, v) in value[..n].iter_mut().enumerate() {
            let control = self.controls[k];
            let lane = match control.read {
                ControlRead::Fast => None,
                ControlRead::Glide | ControlRead::Whole => io.param(k).frames(),
            };
            *v = match (lane, control.read) {
                (Some(lane), _) => {
                    let x = lane[at.frame];
                    self.good[k].read_if(x, control.accepts(x))
                }
                (None, ControlRead::Glide) => {
                    debug_assert!(self.primed, "a block reads its glides after `begin`");
                    self.glides[k].at(at.position + at.frame as u64)
                }
                (None, _) => {
                    let x = self.cells[k].load(Relaxed);
                    self.good[k].read_if(x, control.accepts(x))
                }
            };
        }
        self.set.assemble(&value[..n])
    }

    /// Whether a port drives a control, so the controls can differ between the frames
    /// of the block.
    pub(crate) fn driven(&self, io: &Io<'_>) -> bool {
        (0..self.cells.len())
            .any(|k| self.controls[k].read != ControlRead::Fast && io.param(k).frames().is_some())
    }

    /// The first corner at or after block offset `from` among the ports that drive a
    /// slow control ([`ParamInput::corners`](tutti_graph::ParamInput::corners): where
    /// the trajectory a port delivers changes piece), if any. A fast control is read
    /// every frame and needs none.
    pub(crate) fn corner_from(&self, io: &Io<'_>, from: usize) -> Option<usize> {
        (0..self.cells.len())
            .filter(|&k| self.controls[k].read != ControlRead::Fast)
            .filter_map(|k| {
                let corners = io.param(k).corners();
                let i = corners.partition_point(|&c| (c as usize) < from);
                corners.get(i).map(|&c| c as usize)
            })
            .min()
    }

    /// The per-frame lanes of the [`ControlRead::Fast`] controls, for the
    /// [`Inputs`](tutti_dsp::Inputs): the port's frames in the port's own unit, `None` where
    /// nothing drives it. They are the block's, so a caller slices them by the frames it
    /// renders.
    pub(crate) fn lanes<'a>(&self, io: &Io<'a>) -> [Option<&'a [f32]>; MAX_LANES] {
        let mut lanes = [None; MAX_LANES];
        for (lane, &k) in lanes.iter_mut().zip(&self.fast[..self.fast_len]) {
            *lane = io.param(k).frames();
        }
        lanes
    }

    /// The controls that ring longest among the values a modulated port takes anywhere
    /// in the block, or `None` when nothing is modulated.
    pub(crate) fn longest(&self, io: &Io<'_>) -> Option<S::Controls> {
        let [lowest, highest] = self.extremes(io)?;
        let n = self.cells.len();
        Some(self.set.longest(
            &self.set.assemble(&lowest[..n]),
            &self.set.assemble(&highest[..n]),
        ))
    }

    /// Stops sharing the cells, keeping their values: a fork's own.
    pub(crate) fn detach(&mut self) {
        for cell in &mut self.cells {
            *cell = Arc::new(AtomicF32::new(cell.load(Acquire)));
        }
    }

    /// Whether `target` is far enough from `applied` to design again ([`ControlSet::moved`]).
    pub(crate) fn moved(&self, applied: &S::Controls, target: &S::Controls) -> bool {
        self.set.moved(applied, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A glide of `n` frames from `from` to `to` begun at stream frame `start`, as
    /// [`Params::begin`] builds it.
    fn aimed(from: f32, to: f32, n: u32, start: u64) -> CellGlide {
        let mut glide = Glide::landed(from);
        glide.aim_in(to, n);
        CellGlide { glide, start }
    }

    /// Frame `i` of an `n`-frame glide is `from + (to - from) * (i + 1) / n`, and the
    /// last frame is `to` to the bit, whatever the arithmetic rounds to.
    #[test]
    fn a_cell_glide_is_linear_and_lands_on_its_target_bits() {
        for (from, to, n) in [(0.7f32, 0.2f32, 64u32), (3.3, 1.1, 17), (0.0, 1.0, 240)] {
            let g = aimed(from, to, n, 1_000);
            for i in 0..n - 1 {
                let want = f64::from(from)
                    + (f64::from(to) - f64::from(from)) * f64::from(i + 1) / f64::from(n);
                let got = f64::from(g.at(1_000 + u64::from(i)));
                assert!(
                    (got - want).abs() < 1e-6,
                    "{from} -> {to}: frame {i}: {got} vs {want}"
                );
            }
            assert_eq!(
                g.at(1_000 + u64::from(n) - 1).to_bits(),
                to.to_bits(),
                "{from} -> {to} over {n}: the last frame"
            );
            assert_eq!(
                g.at(1_000 + u64::from(n)).to_bits(),
                to.to_bits(),
                "held after"
            );
        }
    }

    /// A one-frame glide is the new value on its first frame.
    #[test]
    fn a_one_frame_cell_glide_is_the_new_value() {
        assert_eq!(aimed(3.0, 9.0, 1, 5).at(5), 9.0);
    }

    /// A retarget mid-glide starts from the value of the frame before it: the first
    /// frame of the new glide is one step on from there.
    #[test]
    fn a_cell_glide_retargeted_mid_flight_continues_from_its_value() {
        let first = aimed(0.0, 1.0, 100, 0);
        let before = first.at(39);
        let second = aimed(before, 0.25, 50, 40);
        let step = (0.25 - f64::from(before)) / 50.0;
        let got = f64::from(second.at(40));
        assert!((got - (f64::from(before) + step)).abs() < 1e-6, "{got}");
        assert_eq!(second.at(40 + 49).to_bits(), 0.25f32.to_bits());
    }

    /// A set of one glided control.
    #[derive(Clone, Debug)]
    struct One;

    impl ControlSet for One {
        type Controls = f32;
        fn addrs(&self) -> &'static [UnitAddr] {
            const ADDRS: [UnitAddr; 1] = [UnitAddr::first(tutti_types::UnitParam::Cutoff)];
            &ADDRS
        }
        fn defaults(&self) -> f32 {
            0.0
        }
        fn flatten(&self, c: &f32, out: &mut [f32]) {
            out[0] = *c;
        }
        fn assemble(&self, v: &[f32]) -> f32 {
            v[0]
        }
    }

    /// The glide length of the stream test, in frames.
    const N: u64 = 1_000;
    /// A write lands before these stream frames: a multiple of every block length the
    /// split test uses, the second inside the first's glide.
    const FIRST: u64 = 448;
    const SECOND: u64 = 896;
    const TOTAL: u64 = 3_136;

    /// A cell at 0 written to 1 before frame [`FIRST`] and to 0.25 before [`SECOND`],
    /// read per frame through [`Params::begin`] in blocks of `block` frames.
    fn stream(block: u64) -> Vec<f32> {
        let mut params = Params::new(One, &0.0);
        params.retune(SampleRate::SR_48K, true);
        params.glide = N as usize;
        let mut out = Vec::new();
        let mut position = 0;
        while position < TOTAL {
            match position {
                FIRST => params.cells[0].store(1.0, Relaxed),
                SECOND => params.cells[0].store(0.25, Relaxed),
                _ => {}
            }
            params.begin(position);
            for i in 0..block {
                out.push(params.glides[0].at(position + i));
            }
            position += block;
        }
        out
    }

    /// The first write glides `0 -> 1` over [`N`] frames from the first frame of its
    /// block; the second, mid-glide, starts from the first glide's value on the frame
    /// before its block and takes one step of the new distance on its first frame, and
    /// lands on the target's bits on its last. Expected values are the line
    /// `from + (to - from) * k / n` in `f64`.
    #[test]
    fn a_cell_written_mid_glide_continues_from_its_value_through_begin() {
        let got = stream(64);
        let line = |from: f64, to: f64, k: u64| from + (to - from) * k as f64 / N as f64;
        let near = |frame: u64, want: f64| {
            let got = f64::from(got[frame as usize]);
            assert!((got - want).abs() < 1e-6, "frame {frame}: {got} vs {want}");
        };
        near(FIRST - 1, 0.0);
        near(FIRST, line(0.0, 1.0, 1));
        near(SECOND - 1, line(0.0, 1.0, SECOND - FIRST));
        let mid = line(0.0, 1.0, SECOND - FIRST);
        near(SECOND, line(mid, 0.25, 1));
        near(SECOND + 499, line(mid, 0.25, 500));
        assert_eq!(got[(SECOND + N - 1) as usize].to_bits(), 0.25f32.to_bits());
        assert_eq!(got[TOTAL as usize - 1].to_bits(), 0.25f32.to_bits());
    }

    /// The same writes read in blocks of 1, 7 and 64 frames give the same value on every
    /// frame, bit for bit: a glide is on the stream clock, not the block's.
    #[test]
    fn a_cell_glide_is_the_same_whatever_the_blocks() {
        let whole = stream(64);
        for block in [1, 7] {
            let got = stream(block);
            for (f, (g, w)) in got.iter().zip(&whole).enumerate() {
                assert_eq!(g.to_bits(), w.to_bits(), "block {block}, frame {f}");
            }
        }
    }
}
