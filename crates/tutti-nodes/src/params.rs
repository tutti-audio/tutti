//! One reader for the cells of every [`KernelNode`](crate::KernelNode).
//!
//! A kernel writes a [`ControlSet`]: its addresses, and the conversion between its
//! `Controls` and the `f64` cells. [`Params`] is the one reader over it,
//! with one rule for every control: the value its port carries at the solve frame when
//! the port delivers frames (a control signal, or the graph's ramp of a moved cell),
//! else the cell, each read through [`LastGood`]. Sanitising
//! (floors, clamps, whole-set validation) is the design's, never the reader's.
//!
//! A control the kernel reads **per frame** is [`ControlRead::Fast`]: the
//! kernel takes its port's frames from the [`ControlSignals`](crate::ControlSignals)
//! (`Params::signals`), and the reader sees only its cell, the base the member is
//! designed at.

use std::sync::Arc;
use std::sync::atomic::Ordering::{Acquire, Relaxed};

use atomic_float::AtomicF64;
use tutti_buffers::Float;
use tutti_graph::{DeclaredUnit, ErasedSetting, Io, MAX_PARAM_PORTS, ParamSetBuilder};
use tutti_units::UnitAddr;

use crate::kernel::MAX_SIGNALS;
use crate::last_good::LastGood;

/// The most controls a kernel has: as many as a node has param ports, which the 31-band
/// graphic equalizer's gains fill but for one.
const MAX_CONTROLS: usize = MAX_PARAM_PORTS;

/// A kernel's controls as a list of `f64` cells.
pub trait ControlSet: Clone + core::fmt::Debug + Send + 'static {
    /// The design's own `Controls`.
    type Controls: Copy + PartialEq + Send;

    /// The addresses of the cells, in port order after the blend's; its length is
    /// the number of controls [`flatten`](Self::flatten) writes.
    fn addrs(&self) -> &'static [UnitAddr];

    /// The unit of each cell, in the order of [`addrs`](Self::addrs).
    fn units(&self) -> &'static [DeclaredUnit];

    /// The controls a non-finite cell falls back to.
    fn defaults(&self) -> Self::Controls;

    /// `c` as one `f64` per address (`out.len() == addrs().len()`).
    fn flatten(&self, c: &Self::Controls, out: &mut [f64]);

    /// The controls of one `f64` per address: the inverse of
    /// [`flatten`](Self::flatten).
    fn assemble(&self, v: &[f64]) -> Self::Controls;

    /// How the reader takes control `k` (`k < addrs().len()`): whether the graph ramps a
    /// moved cell, lands it whole or the kernel reads it per frame, and which values it
    /// accepts. [`Control::RAMP`] by
    /// default; `control_set!` writes it from each row's flags, so a control is
    /// declared once.
    fn control(&self, _k: usize) -> Control {
        Control::RAMP
    }

    /// Whether `target` is far enough from `applied`, the controls the model was last
    /// designed from, to design again: the dead-band. Exact inequality by default.
    fn moved(&self, applied: &Self::Controls, target: &Self::Controls) -> bool {
        applied != target
    }

    /// The cells of the settings the set holds, each under its kind: what a session's key
    /// sets and reads. Allocates; called on the control side when the node's parts are built.
    /// None by default; a set that holds one also implements
    /// [`HasSetting`](tutti_graph::HasSetting) for its kind.
    fn settings(&self) -> Vec<ErasedSetting> {
        Vec::new()
    }

    /// Stops sharing what the set shares besides the cells (a setting's word), keeping its
    /// value: a fork's own. A set that shares nothing has nothing to do, the default.
    fn detach(&mut self) {}
}

/// The [`ControlSet`] of a kernel with no controls: no cells, and the unit `()` for its
/// `Controls`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoControls;

impl ControlSet for NoControls {
    type Controls = ();

    fn addrs(&self) -> &'static [UnitAddr] {
        &[]
    }

    fn units(&self) -> &'static [DeclaredUnit] {
        &[]
    }

    fn defaults(&self) {}

    fn flatten(&self, _: &(), _: &mut [f64]) {}

    fn assemble(&self, _: &[f64]) {}
}

/// How a control reaches the kernel from its cell and its port.
#[derive(Clone, Copy, Debug, PartialEq, Hash, Eq)]
pub enum ControlRead {
    /// The graph ramps a moved cell on its port
    /// ([`ParamInput::Smoothing`](tutti_graph::ParamInput::Smoothing)), and the reader
    /// takes the port's value at the solve frame whenever the port delivers frames.
    Ramp,
    /// As [`Ramp`](Self::Ramp), but the graph lands a moved cell whole
    /// ([`ParamPorts::whole`](tutti_graph::ParamPorts::whole)): a control whose ramp is the
    /// kernel's own (a detector's, a makeup's) is not ramped twice.
    Whole,
    /// Read **per frame** by the kernel, from its port, into a
    /// [`ControlSignals`](crate::ControlSignals) control signal (the fast controls fill the control signals in row order).
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Control {
    /// How it is read.
    pub read: ControlRead,
    /// Which values it accepts.
    pub valid: ControlValid,
    /// What the kernel's [`tail_of`](crate::Kernel::tail_of) does with it.
    pub ring: Ring,
}

/// What a control does to the kernel's ring-out ([`Control::ring`]): whether
/// [`tail_of`](crate::Kernel::tail_of) reads it and, if so, where it is longest. A control
/// counts as driven for every block its port delivers [driven frames](tutti_graph::ParamInput::is_driven): through a connect or disconnect
/// declick, and from its source's first event on; a source that has not spoken leaves the port
/// on its base. A driven port may take any value later, so the node reports the most the
/// kernel rings over the driven controls' whole range, the others as their cells set them
/// ([`KernelNode`](crate::KernelNode)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ring {
    /// The tail never reads it: driving it leaves the ring-out as it is.
    Never,
    /// The tail is non-decreasing toward this raw value (the `f64` the cell and the port
    /// carry, in the control's unit), whatever the other controls are: the value is the end of
    /// the range, or any value at or past the point the kernel clamps it at.
    LongestAt(f64),
    /// The tail reads it and the longest value is not stated: driving it reports the kernel's
    /// ceiling ([`Kernel::tail`](crate::Kernel::tail)).
    Unstated,
}

impl Control {
    /// A control the graph ramps, accepting finite values: a row with no flags.
    pub const RAMP: Self = Self {
        read: ControlRead::Ramp,
        valid: ControlValid::Finite,
        ring: Ring::Unstated,
    };

    /// The row flag `whole`: a moved cell lands whole.
    #[must_use]
    pub(crate) const fn whole(self) -> Self {
        Self {
            read: ControlRead::Whole,
            ..self
        }
    }

    /// The row flag `fast`: read per frame from a [`ControlSignals`](crate::ControlSignals) control signal.
    #[must_use]
    pub(crate) const fn fast(self) -> Self {
        Self {
            read: ControlRead::Fast,
            ..self
        }
    }

    /// The row flag `no_ring`: the kernel's tail never reads this control, so driving it
    /// leaves the ring-out as it is. A control without a ring flag is read by the tail and
    /// [`Ring::Unstated`]: forgetting the flag errs long.
    #[must_use]
    pub(crate) const fn no_ring(self) -> Self {
        Self {
            ring: Ring::Never,
            ..self
        }
    }

    /// The row flag `longest = raw`: the tail is longest at `raw`, the port's own raw scale
    /// ([`Ring::LongestAt`]).
    #[must_use]
    pub(crate) const fn longest(self, raw: f64) -> Self {
        Self {
            ring: Ring::LongestAt(raw),
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
    pub(crate) fn accepts(self, v: f64) -> bool {
        match self.valid {
            ControlValid::Finite => v.is_finite(),
            ControlValid::NotNan => !v.is_nan(),
        }
    }

    /// How many of `controls` are [`ControlRead::Fast`]; `const`, for the macro's check
    /// against [`MAX_SIGNALS`].
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
/// `control_set!` writes from the kernel's own `Controls` fields, and what
/// the typed key of a [`KernelNode`](crate::KernelNode) reads
/// ([`HasParam`](tutti_graph::HasParam)).
pub trait HasControl<const P: u16> {
    /// The unit the control's value is in.
    type Unit: tutti_graph::ParamValue;
}

/// That a [`ControlSet`] has several controls of the kind `P`, numbered from 0.
pub trait HasControls<const P: u16>: HasControl<P> {}

/// Writes a kernel's [`ControlSet`] from its `Controls` fields, once: the
/// addresses, the conversion to the `f64` cells and back, and the
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
/// rows the set has (`all`, or an expression: a graphic EQ has a row for
/// each of its bands). A row is `Kind => Unit = field`, a single control, where `field` may be
/// a path of fields (`inner.field`, as the FDN's `network.decay[0]` and `kind.pitch`);
/// `Kind[0] => Unit = field[0]` the first of several of a kind (`HasControls`),
/// and `Kind[i]` with `i` above 0 the others. A row `Kind => Unit = field via Ranged` is a
/// range-checked field: it reads as `field.get()` and a cell becomes `Ranged::held(cell)`, the
/// hold of a live value that has no caller to refuse it to. A row may end in flags, `{ .. }`, that
/// say how the reader takes it ([`Control`]): `whole` (lands whole, no ramp), `fast`
/// (read per frame into a [`ControlSignals`](crate::ControlSignals) control signal, in row order; more fast rows
/// than [`MAX_SIGNALS`] do not compile), `not_nan` (accepts `+-inf`), `no_ring` (the
/// kernel's tail never reads it) and `longest = raw` (the tail is longest at that raw value of
/// the control, in the unit's raw scale; see [`Ring`]). A row with neither of the last two is
/// read by the tail with no longest value stated. A flag that is not one of these does not
/// compile.
macro_rules! control_set {
    (
        $set:ident($s:ident): $controls:ty = $base:expr;
        live: ($($live:tt)+);
        $($kind:ident $([$ix:tt])? => $unit:ty = $f:ident $(. $g:ident)* $([$pi:literal])? $(via $ranged:ty)?
            $({ $($flag:ident $(= $val:expr)?),* })?),+ ;
        $($items:tt)*
    ) => {
        impl $crate::params::ControlSet for $set {
            type Controls = $controls;

            #[allow(unused_variables)]
            fn addrs(&self) -> &'static [tutti_units::UnitAddr] {
                const ALL: &[tutti_units::UnitAddr] = &[$(
                    control_set!(@addr $kind $([$ix])?)
                ),+];
                let $s = self;
                &ALL[..control_set!(@live ALL, $($live)+)]
            }

            fn units(&self) -> &'static [tutti_graph::DeclaredUnit] {
                const ALL: &[tutti_graph::DeclaredUnit] =
                    &[$(tutti_graph::DeclaredUnit::of::<$unit>()),+];
                &ALL[..self.addrs().len()]
            }

            fn control(&self, k: usize) -> $crate::params::Control {
                const ALL: &[$crate::params::Control] = &[$(
                    $crate::params::Control::RAMP $($(.$flag($($val)?))*)?
                ),+];
                const _: () = assert!(
                    $crate::params::Control::fast_count(ALL) <= $crate::MAX_SIGNALS,
                    "more fast controls than a ControlSignals has signals",
                );
                ALL[k]
            }

            fn flatten(&self, c: &$controls, out: &mut [f64]) {
                let all = [$(
                    <$unit as tutti_graph::ParamValue>::to_cell(
                        control_set!(@read c . $f $(. $g)* $([$pi])? $(via $ranged)?)
                    )
                ),+];
                out.copy_from_slice(&all[..out.len()]);
            }

            #[allow(unused_variables)]
            fn assemble(&self, v: &[f64]) -> $controls {
                let $s = self;
                let mut c: $controls = $base;
                let mut cells = v.iter().copied();
                $(
                    if let Some(x) = cells.next() {
                        c.$f $(. $g)* $([$pi])? = control_set!(@write $unit, x $(, via $ranged)?);
                    }
                )+
                c
            }

            $($items)*
        }
        $( control_set!(@has $set; $kind $([$ix])? => $unit); )+
    };
    (@read $c:ident . $f:ident $(. $g:ident)* $([$pi:literal])? via $ranged:ty) => {
        $c.$f $(. $g)* $([$pi])?.get()
    };
    (@read $c:ident . $f:ident $(. $g:ident)* $([$pi:literal])?) => { $c.$f $(. $g)* $([$pi])? };
    (@write $unit:ty, $x:ident, via $ranged:ty) => {
        <$ranged>::held(<$unit as tutti_graph::ParamValue>::from_cell($x))
    };
    (@write $unit:ty, $x:ident) => { <$unit as tutti_graph::ParamValue>::from_cell($x) };
    (@addr $kind:ident) => { tutti_units::UnitAddr::first(tutti_units::UnitParam::$kind) };
    (@addr $kind:ident [$ix:tt]) => {
        tutti_units::UnitAddr::new(tutti_units::UnitParam::$kind, $ix)
    };
    (@live $all:ident, all) => { $all.len() };
    (@live $all:ident, $($live:tt)+) => { $($live)+ };
    (@has $set:ident; $kind:ident => $unit:ty) => {
        impl $crate::params::HasControl<{ tutti_units::UnitParam::$kind as u16 }> for $set {
            type Unit = $unit;
        }
    };
    (@has $set:ident; $kind:ident [0] => $unit:ty) => {
        impl $crate::params::HasControl<{ tutti_units::UnitParam::$kind as u16 }> for $set {
            type Unit = $unit;
        }
        impl $crate::params::HasControls<{ tutti_units::UnitParam::$kind as u16 }> for $set {}
    };
    (@has $set:ident; $kind:ident [$ix:tt] => $unit:ty) => {};
}

/// The cells of a [`ControlSet`] and how a block reads them.
#[derive(Clone, Debug)]
pub struct Params<S: ControlSet> {
    set: S,
    cells: Vec<Arc<AtomicF64>>,
    /// The last finite value each control held: a non-finite write reads as
    /// unchanged, so it never reaches the design.
    good: Vec<LastGood<f64>>,
    /// How each control is read, from [`ControlSet::control`].
    controls: [Control; MAX_CONTROLS],
    /// The fast controls, by index, in the order of the control signals they fill.
    fast: [usize; MAX_SIGNALS],
    /// How many of `fast` are filled.
    fast_len: usize,
}

impl<S: ControlSet> Params<S> {
    /// Cells of `set` holding `initial`. Control side; allocates.
    ///
    /// # Panics
    ///
    /// If `set` has more [`ControlRead::Fast`] controls than [`ControlSignals`](crate::ControlSignals) has
    /// control signals ([`MAX_SIGNALS`]): only a hand-written [`ControlSet`] can, since
    /// `control_set!` refuses it at compile time.
    #[must_use]
    pub fn new(set: S, initial: &S::Controls) -> Self {
        let n = set.addrs().len();
        let (mut value, mut fallback) = ([0.0f64; MAX_CONTROLS], [0.0f64; MAX_CONTROLS]);
        set.flatten(initial, &mut value[..n]);
        set.flatten(&set.defaults(), &mut fallback[..n]);
        let (mut cells, mut good) = (Vec::with_capacity(n), Vec::with_capacity(n));
        let mut controls = [Control::RAMP; MAX_CONTROLS];
        let (mut fast, mut fast_len) = ([0; MAX_SIGNALS], 0);
        for k in 0..n {
            controls[k] = set.control(k);
            if controls[k].read == ControlRead::Fast {
                assert!(
                    fast_len < MAX_SIGNALS,
                    "more fast controls than a ControlSignals has signals"
                );
                fast[fast_len] = k;
                fast_len += 1;
            }
            let ok = controls[k].accepts(value[k]);
            let mut g = LastGood::new_if(value[k], fallback[k], ok);
            cells.push(Arc::new(AtomicF64::new(g.read_if(value[k], ok))));
            good.push(g);
        }
        Self {
            set,
            cells,
            good,
            controls,
            fast,
            fast_len,
        }
    }

    /// Whether a control signal drives a control the tail reads ([`Control::ring`] is not
    /// [`Ring::Never`]) in the block, a fast one included.
    pub(crate) fn ring_driven<G: Float>(&self, io: &Io<'_, G>) -> bool {
        (0..self.cells.len())
            .any(|k| self.controls[k].ring != Ring::Never && io.param(k).is_driven())
    }

    /// The controls the tail is longest at, given which ports drive a control in the block:
    /// each driven control with a [`Ring::LongestAt`] at that value, every other as its cell
    /// holds it. `None` when a driven control the tail reads has no value stated. It depends
    /// on which ports are driven, not on the values they carry. A cell written during the
    /// block is read as written.
    pub(crate) fn longest<G: Float>(&mut self, io: &Io<'_, G>) -> Option<S::Controls> {
        let n = self.cells.len();
        let mut value = [0.0f64; MAX_CONTROLS];
        for (k, v) in value[..n].iter_mut().enumerate() {
            let control = self.controls[k];
            *v = match control.ring {
                Ring::LongestAt(raw) if io.param(k).is_driven() => raw,
                Ring::Unstated if io.param(k).is_driven() => return None,
                _ => {
                    let x = self.cells[k].load(Relaxed);
                    self.good[k].read_if(x, control.accepts(x))
                }
            };
        }
        Some(self.set.assemble(&value[..n]))
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
            .zip(self.set.units().iter().copied())
            .zip(cells)
            .fold(set, |set, ((&addr, unit), cell)| {
                set.param(unit, addr, cell)
            })
    }

    /// The ports the graph does not smooth, as a [`ParamPorts::whole`](tutti_graph::ParamPorts::whole) mask:
    /// bit `k` for a control that lands [whole](ControlRead::Whole) or is
    /// [fast](ControlRead::Fast).
    pub(crate) fn whole_mask(&self) -> u32 {
        self.controls[..self.cells.len()]
            .iter()
            .enumerate()
            .filter(|(_, c)| c.read != ControlRead::Ramp)
            .fold(0, |mask, (k, _)| mask | 1 << k)
    }

    /// The base of the `k`th address ([`Node::param_base`](tutti_graph::Node::param_base)): its cell's value.
    pub(crate) fn base(&self, k: usize) -> Option<f64> {
        self.cells.get(k).map(|c| c.load(Acquire))
    }

    /// The controls at block frame `frame`: the value a control's port carries there when
    /// the port delivers frames (a control signal, or the graph's ramp of a moved
    /// [`ControlRead::Ramp`] cell), else its cell, read through [`LastGood`] so a value
    /// the control refuses ([`Control::accepts`]: non-finite by default) reads as
    /// unchanged. A [`ControlRead::Fast`] control is its cell whatever its port carries.
    pub(crate) fn read<G: Float>(&mut self, io: &Io<'_, G>, frame: usize) -> S::Controls {
        let n = self.cells.len();
        let mut value = [0.0f64; MAX_CONTROLS];
        for (k, v) in value[..n].iter_mut().enumerate() {
            let control = self.controls[k];
            let port = match control.read {
                ControlRead::Fast => None,
                ControlRead::Ramp | ControlRead::Whole => io.param(k).frames(),
            };
            let x = port.map_or_else(|| self.cells[k].load(Relaxed), |frames| frames[frame]);
            *v = self.good[k].read_if(x, control.accepts(x));
        }
        self.set.assemble(&value[..n])
    }

    /// Whether a port drives a control, so the controls can differ between the frames
    /// of the block.
    pub(crate) fn driven<G: Float>(&self, io: &Io<'_, G>) -> bool {
        (0..self.cells.len())
            .any(|k| self.controls[k].read != ControlRead::Fast && io.param(k).is_driven())
    }

    /// The first corner at or after block offset `from` among the ports that drive a
    /// slow control ([`ParamInput::corners`](tutti_graph::ParamInput::corners): where
    /// the trajectory a port delivers changes piece), if any. A fast control is read
    /// every frame and needs none.
    pub(crate) fn corner_from<G: Float>(&self, io: &Io<'_, G>, from: usize) -> Option<usize> {
        (0..self.cells.len())
            .filter(|&k| self.controls[k].read != ControlRead::Fast)
            .filter_map(|k| {
                let corners = io.param(k).corners();
                let i = corners.partition_point(|&c| (c as usize) < from);
                corners.get(i).map(|&c| c as usize)
            })
            .min()
    }

    /// The per-frame control signals of the [`ControlRead::Fast`] controls, for the
    /// [`ControlSignals`](crate::ControlSignals): the port's frames in the port's own unit, `None` where
    /// nothing drives it. They are the block's, so a caller slices them by the frames it
    /// renders.
    pub(crate) fn signals<'a, G: Float>(&self, io: &Io<'a, G>) -> [Option<&'a [G]>; MAX_SIGNALS] {
        let mut signals = [None; MAX_SIGNALS];
        for (signal, &k) in signals.iter_mut().zip(&self.fast[..self.fast_len]) {
            *signal = io.param(k).narrowed();
        }
        signals
    }

    /// Stops sharing the cells, keeping their values: a fork's own.
    pub(crate) fn detach(&mut self) {
        for cell in &mut self.cells {
            *cell = Arc::new(AtomicF64::new(cell.load(Acquire)));
        }
        self.set.detach();
    }

    /// The set the cells are addressed by.
    pub(crate) fn set(&self) -> &S {
        &self.set
    }

    /// Whether `target` is far enough from `applied` to design again ([`ControlSet::moved`]).
    pub(crate) fn moved(&self, applied: &S::Controls, target: &S::Controls) -> bool {
        self.set.moved(applied, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A set of one control.
    #[derive(Clone, Debug)]
    struct One;

    impl ControlSet for One {
        type Controls = f64;
        fn addrs(&self) -> &'static [UnitAddr] {
            const ADDRS: [UnitAddr; 1] = [UnitAddr::first(tutti_units::UnitParam::Cutoff)];
            &ADDRS
        }
        fn units(&self) -> &'static [DeclaredUnit] {
            const UNITS: [DeclaredUnit; 1] = [DeclaredUnit::of::<tutti_units::Hz>()];
            &UNITS
        }
        fn defaults(&self) -> f64 {
            0.0
        }
        fn flatten(&self, c: &f64, out: &mut [f64]) {
            out[0] = *c;
        }
        fn assemble(&self, v: &[f64]) -> f64 {
            v[0]
        }
    }

    /// A control cell holds an `f64` figure `f32` cannot: what a host writes is what
    /// the controls hold and what `param_base` says, to the bit, with no narrowing
    /// between.
    #[test]
    fn an_f64_control_round_trips_a_figure_f32_cannot_hold() {
        const FINE: f64 = 1_000.000_000_1;
        assert_ne!(f64::from(FINE as f32), FINE, "setup: f32 loses FINE");
        let params = Params::new(One, &0.0);
        params.cells[0].store(FINE, Relaxed);
        assert_eq!(params.base(0).map(f64::to_bits), Some(FINE.to_bits()));
    }
}
