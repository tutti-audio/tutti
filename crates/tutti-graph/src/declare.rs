//! A node's params, declared once ([`declare_params!`](crate::declare_params)),
//! and the typed vocabulary they are declared in: [`ParamUnit`], [`HasParam`].
//!
//! A node with params says three things about them: which the graph may
//! modulate (its [`Shape`](crate::Shape)'s ports), which a host reaches by
//! address (its [`ParamSet`]), and what unit each is in (what a typed key
//! hands a caller). The declaration states them once, next to the node's
//! `Param<U>` cells, and generates all three, so they cannot drift and the
//! unit is checked against the cell's own type.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tutti_types::{
    Amplitude, AtomicF32, Azimuth, Balance, BitDepth, CentsSpan, CompressionRatio, Db, Depth,
    Drive, Elevation, Feedback, Gain, HoldFactor, Hz, Mix, Pan, Param, ParamCell, ParamRaw, Q,
    Resonance, SampleSpan, Seconds, Semitones, Spread, StereoWidth, Unit, UnitAddr,
};

use crate::controls::{ParamSet, Params};
use crate::io::Io;
use crate::param::{MAX_PARAM_PORTS, ParamInput, ParamPorts};

mod sealed {
    pub trait Sealed {}
}

/// What a param's value is: a unit tutti defines ([`Scalar`]), or a `bool` for
/// a switch (`Bypass`, `Solo`, `Freeze`: a cell at or above 0.5 is on). Sealed:
/// the engine's units are listed, and a new one joins them.
pub trait ParamUnit: Copy + PartialEq + Send + Sync + 'static + sealed::Sealed {
    /// The value as the `f32` the param's cell carries (a
    /// [`Seconds`] is narrowed: the `ParamSet` and a lane's value are `f32`).
    fn to_cell(self) -> f32;

    /// The value a cell's `f32` stands for: the inverse of
    /// [`to_cell`](Self::to_cell) (a `Seconds` is widened exactly).
    fn from_cell(cell: f32) -> Self;

    /// The value at the width its unit's cell holds, as an `f64`: exact for
    /// every unit (a [`Seconds`] is not narrowed).
    fn to_wide(self) -> f64;

    /// The value a cell's `f64` stands for: the inverse of
    /// [`to_wide`](Self::to_wide).
    fn from_wide(wide: f64) -> Self;
}

/// A [`ParamUnit`] that is a number in a unit (not a switch): what a range
/// (`Mods::range`) is a range of. Sealed.
pub trait Scalar: ParamUnit + Unit<Raw: ParamRaw> {}

macro_rules! scalar_units {
    ($($unit:ty),+ $(,)?) => {$(
        impl sealed::Sealed for $unit {}
        impl ParamUnit for $unit {
            fn to_cell(self) -> f32 {
                <<$unit as Unit>::Raw as ParamRaw>::to_f32(<$unit as Unit>::to_raw(self))
            }
            fn from_cell(cell: f32) -> Self {
                <$unit as Unit>::from_raw(<<$unit as Unit>::Raw as ParamRaw>::from_f32(cell))
            }
            fn to_wide(self) -> f64 {
                <<$unit as Unit>::Raw as ParamRaw>::to_f64(<$unit as Unit>::to_raw(self))
            }
            fn from_wide(wide: f64) -> Self {
                <$unit as Unit>::from_raw(<<$unit as Unit>::Raw as ParamRaw>::from_f64(wide))
            }
        }
        impl Scalar for $unit {}
    )+};
}

scalar_units!(
    Amplitude,
    Azimuth,
    Balance,
    BitDepth,
    CentsSpan,
    CompressionRatio,
    Db,
    Depth,
    Drive,
    Elevation,
    Feedback,
    Gain,
    HoldFactor,
    Hz,
    Mix,
    Pan,
    Q,
    Resonance,
    SampleSpan,
    Seconds,
    Semitones,
    Spread,
    StereoWidth,
);

impl sealed::Sealed for bool {}
impl ParamUnit for bool {
    fn to_cell(self) -> f32 {
        f32::from(u8::from(self))
    }

    fn from_cell(cell: f32) -> Self {
        cell >= 0.5
    }

    fn to_wide(self) -> f64 {
        f64::from(u8::from(self))
    }

    fn from_wide(wide: f64) -> Self {
        wide >= 0.5
    }
}

/// That the node `Self` has a native param of the kind `P` (a packed
/// [`UnitAddr`] at index 0: `UnitParam::Cutoff as u16`), whose value is in
/// `Unit`. What a typed key's param method for that kind needs, and what
/// [`declare_params!`](crate::declare_params) writes from the node's own
/// cells.
pub trait HasParam<const P: u16> {
    /// The unit the param's value is in.
    type Unit: ParamUnit;
}

/// That the node `Self` has several params of the kind `P`, numbered from 0:
/// what a typed key's indexed method (`threshold_at`) needs. Whether index `i`
/// exists is the graph's to say when it is used.
pub trait HasParams<const P: u16>: HasParam<P> {}

/// A [`Params`] node has the params its node does.
impl<N: HasParam<P>, const P: u16> HasParam<P> for Params<N> {
    type Unit = N::Unit;
}
impl<N: HasParams<P>, const P: u16> HasParams<P> for Params<N> {}

/// A cell a declaration names, whose value is in `U`: what makes the unit a
/// declaration states agree with the cell's own type. Implemented for a
/// `&Param<U>` (any unit but its own does not compile), for a switch's
/// `&Arc<AtomicF32>` (`bool` only), and for an `Option` of either, where the
/// node has the param only sometimes.
#[doc(hidden)]
pub trait Cells<U: ParamUnit> {
    /// The cell's value as an `f32`, or `None` where the node has no such
    /// param.
    fn base(&self) -> Option<f32>;

    /// The cell, shared. Clones the `Arc`: control thread.
    fn cell(&self) -> Option<ParamCell>;
}

impl<U> Cells<U> for &Param<U>
where
    U: Scalar,
    Arc<<U::Raw as ParamRaw>::Atomic>: Into<ParamCell>,
{
    fn base(&self) -> Option<f32> {
        Some(self.load().to_cell())
    }

    fn cell(&self) -> Option<ParamCell> {
        Some(Param::cell(self))
    }
}

impl Cells<bool> for &Arc<AtomicF32> {
    fn base(&self) -> Option<f32> {
        Some(self.load(Ordering::Acquire))
    }

    fn cell(&self) -> Option<ParamCell> {
        Some(ParamCell::F32(Arc::clone(self)))
    }
}

impl<U: ParamUnit, C: Cells<U>> Cells<U> for Option<C> {
    fn base(&self) -> Option<f32> {
        self.as_ref().and_then(Cells::base)
    }

    fn cell(&self) -> Option<ParamCell> {
        self.as_ref().and_then(Cells::cell)
    }
}

/// One declared param, as a declaration visits it.
#[doc(hidden)]
pub struct Row<'a> {
    addr: UnitAddr,
    port: bool,
    base: f32,
    cell: &'a dyn Fn() -> Option<ParamCell>,
}

impl std::fmt::Debug for Row<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("addr", &self.addr)
            .field("port", &self.port)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// A declaration's visitor: called with each param the node has, in the order
/// declared; `false` stops the walk.
#[doc(hidden)]
pub type Visit<'a> = &'a mut dyn FnMut(Row<'_>) -> bool;

/// Visits one declared param of unit `U`.
#[doc(hidden)]
pub fn row<U: ParamUnit, C: Cells<U>>(
    visit: Visit<'_>,
    addr: UnitAddr,
    port: bool,
    cell: C,
) -> bool {
    let Some(base) = cell.base() else {
        return true;
    };
    visit(Row {
        addr,
        port,
        base,
        cell: &|| cell.cell(),
    })
}

/// The ports a declaration gives the node's shape: its params that are
/// ports, in the order declared.
#[doc(hidden)]
pub fn ports(walk: impl FnOnce(Visit<'_>) -> bool) -> ParamPorts {
    let mut addrs = [UnitAddr::first(tutti_types::UnitParam::Cutoff); MAX_PARAM_PORTS];
    let mut n = 0;
    walk(&mut |row| {
        if row.port {
            addrs[n] = row.addr;
            n += 1;
        }
        true
    });
    ParamPorts::of(&addrs[..n])
}

/// The [`ParamSet`] a declaration gives the node: every param it has.
#[doc(hidden)]
pub fn set(walk: impl FnOnce(Visit<'_>) -> bool) -> ParamSet {
    let mut set = ParamSet::builder();
    walk(&mut |row| {
        if let Some(cell) = (row.cell)() {
            set = std::mem::replace(&mut set, ParamSet::builder()).param(row.addr, cell);
        }
        true
    });
    set.build()
}

/// Port `k`'s cell value: what [`Node::param_base`](crate::Node::param_base)
/// answers.
#[doc(hidden)]
pub fn base(walk: impl FnOnce(Visit<'_>) -> bool, k: usize) -> Option<f32> {
    let (mut next, mut found) = (0, None);
    walk(&mut |row| {
        if !row.port {
            return true;
        }
        if next == k {
            found = Some(row.base);
            return false;
        }
        next += 1;
        true
    });
    found
}

/// The block's read of the param declared at `addr`: the port its row is,
/// counted among the declaration's ports as [`ports`] counts them, so a
/// reordered row moves the read with it. A param the node has no port for
/// (a row after a `;`, or an `Option` cell that is `None`) reads
/// [`ParamInput::Base`].
#[doc(hidden)]
pub fn input<'a>(
    walk: impl FnOnce(Visit<'_>) -> bool,
    io: &Io<'a>,
    p: u16,
    index: u8,
) -> ParamInput<'a> {
    let addr = UnitAddr::try_from(p | u16::from(index) << 8)
        .expect("`HasParam<P>` is declared only for a `UnitParam` kind");
    let (mut next, mut found) = (0, None);
    walk(&mut |row| {
        if !row.port {
            return true;
        }
        if row.addr == addr {
            found = Some(next);
            return false;
        }
        next += 1;
        true
    });
    found.map_or(ParamInput::Base, |k| io.param(k))
}

/// Declares a node's params once, next to its cells: what a host reaches by
/// address (its [`ParamSet`]), what the graph may modulate (its shape's
/// ports), and the unit each is in (a typed key's
/// [`HasParam`]).
///
/// ```
/// use tutti_graph::declare_params;
/// use tutti_types::{Hz, Param, Q};
///
/// struct Filter {
///     cutoff: Param<Hz>,
///     q: Param<Q>,
/// }
///
/// declare_params!([] Filter, n {
///     Cutoff => Hz = &n.cutoff,
///     Q => Q = &n.q,
/// });
///
/// let filter = Filter { cutoff: Param::new(Hz(1_000.0)), q: Param::new(Q(0.7)) };
/// // The shape's ports, the node's `ParamSet` and the base of port 1:
/// assert_eq!(filter.declared_ports().as_slice().len(), 2);
/// assert!(filter.declared_set().set(tutti_types::UnitParam::Cutoff, 2_000.0));
/// assert_eq!(filter.declared_base(0), Some(2_000.0));
/// ```
///
/// The unit a row states is checked against the cell's own type: a cutoff
/// declared in the wrong unit does not compile.
///
/// ```compile_fail
/// use tutti_graph::declare_params;
/// use tutti_types::{Db, Hz, Param};
///
/// struct Filter {
///     cutoff: Param<Db>,
/// }
///
/// // `cutoff` is a `Param<Db>`, not a `Param<Hz>`.
/// declare_params!([] Filter, n {
///     Cutoff => Hz = &n.cutoff,
/// });
/// ```
///
/// # Form
///
/// `declare_params!([generics] Type, n { rows })`, where `n` names the node in
/// the rows' cell expressions. Each row is `Kind => Unit = cell`, a
/// [`UnitParam`](tutti_types::UnitParam) kind, the unit its value is in, and
/// the cell: a `&Param<Unit>`, a `&Arc<AtomicF32>` for a `bool` switch, or an
/// `Option` of either for a param the node has only sometimes. The rows are
/// the node's ports in this order, and its set in this order.
///
/// - **Several of a kind** are a group, `[i in range] { rows }`: each row is
///   addressed at index `i`, and the group's rows repeat for each `i` before
///   the next (a band's params, then the next band's). The kind then has the
///   indexed `HasParams`.
/// - **A param that is not a port** (set by address, never modulated) follows
///   a `;` in its group, or in the node's rows.
///
/// It generates, on the type, `declared_ports` (a
/// [`ParamPorts`] for the shape's
/// [`with_param_addrs`](crate::Shape::with_param_addrs)), `declared_set` (the
/// node's [`ParamSet`], for [`ParamNode::param_set`](crate::ParamNode::param_set)) and
/// `declared_base` (port `k`'s cell value, for
/// [`Node::param_base`](crate::Node::param_base)), and the type's
/// [`HasParam`] impls.
///
/// # Settings
///
/// A value that is one word by its encoding (a mode: a fieldless enum written
/// with [`setting_word!`](tutti_rt::setting_word)) is not a param: it has no
/// address and no port, and cannot be modulated. It is a
/// [`Setting<T>`](tutti_rt::Setting) field of the type that owns the cells (the
/// node, or the `Arc`-shared controls a node reads), declared after the rows:
///
/// ```text
/// declare_params!([] Node, n { rows } settings NodeConfig { mode: Mode = Mode::Off });
/// declare_params!([] Controls, settings ControlsConfig { mode: Mode = Mode::Off });
/// ```
///
/// With no rows the `n { rows }` part is left out. Each entry writes:
///
/// - the config named after `settings`: its `new()` holds each entry's
///   default and a chained option per entry (`.mode(Mode::On)`) sets the
///   initial value. Its fields are
///   private to the module that declares it, where the type's constructor
///   builds each cell with `Setting::new(config.mode)`;
/// - the control-side handle `owner.mode()`, a `&Setting<Mode>`: `set` lands
///   at once, with no pending slot;
/// - the type check that the field `mode` is a `Setting<Mode>` and that `Mode`
///   is a [`SettingWord`](tutti_rt::SettingWord).
///
/// The node reads `self.mode.get()` once per block.
///
/// ```
/// use tutti_graph::declare_params;
/// use tutti_rt::{Setting, setting_word};
///
/// #[derive(Clone, Copy, Debug, PartialEq)]
/// #[repr(u8)]
/// enum Mode { Off, On }
/// setting_word!(Mode { Off, On });
///
/// #[derive(Debug)]
/// struct Controls {
///     mode: Setting<Mode>,
/// }
///
/// declare_params!([] Controls, settings ControlsConfig { mode: Mode = Mode::Off });
///
/// impl Controls {
///     fn new(config: ControlsConfig) -> Self {
///         Self { mode: Setting::new(config.mode) }
///     }
/// }
///
/// let controls = Controls::new(ControlsConfig::new().mode(Mode::On));
/// assert_eq!(controls.mode().get(), Mode::On);
/// controls.mode().set(Mode::Off);
/// assert_eq!(controls.mode.get(), Mode::Off);
/// ```
///
/// A cell that is not a `Setting` of the declared type does not compile:
///
/// ```compile_fail
/// use tutti_graph::declare_params;
/// use tutti_rt::{Setting, setting_word};
///
/// #[derive(Clone, Copy, Debug)]
/// #[repr(u8)]
/// enum Mode { Off }
/// setting_word!(Mode { Off });
///
/// struct Controls {
///     mode: u8,
/// }
///
/// declare_params!([] Controls, settings ControlsConfig { mode: Mode = Mode::Off });
/// ```
///
/// # Reading a driven param
///
/// `process` reads a param through the declaration, never by a port index:
/// `declared_param::<{ UnitParam::Cutoff as u16 }>(&io)` (`declared_param_at`
/// with the group's `index` for a repeated kind) is the [`ParamInput`] of the
/// port whose row names that kind, so reordering or inserting a row moves the
/// read with it. A kind the node does not declare does not compile. It walks
/// the rows (a handful of loads), so read it once per block, not per frame.
///
/// ```
/// use tutti_graph::{declare_params, Cx, Io, Node, Prepare, Shape, Status};
/// use tutti_types::{Amplitude, ChannelLayout, Hz, Param, UnitParam};
///
/// struct Tone {
///     level: Param<Amplitude>,
///     freq: Param<Hz>,
/// }
///
/// // Declared in this order; the reads below do not depend on it.
/// declare_params!([] Tone, n {
///     Cutoff => Hz = &n.freq,
///     Volume => Amplitude = &n.level,
/// });
///
/// impl Node for Tone {
///     fn shape(&self) -> Shape {
///         Shape::source_unassigned(ChannelLayout::MONO)
///             .with_param_addrs(self.declared_ports().as_slice())
///     }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
///         let level = self
///             .declared_param::<{ UnitParam::Volume as u16 }>(&io)
///             .last_frame()
///             .unwrap_or_else(|| self.level.load().get());
///         io.output(0).fill(level);
///         Status::Modified
///     }
///     fn reset(&mut self) {}
/// }
/// ```
#[macro_export]
macro_rules! declare_params {
    // A node's rows and its settings: the two forms below.
    ([$($g:tt)*] $ty:ty, $n:ident { $($rows:tt)* }
        settings $cfg:ident { $($set:tt)* }) => {
        $crate::declare_params!([$($g)*] $ty, $n { $($rows)* });
        $crate::declare_params!([$($g)*] $ty, settings $cfg { $($set)* });
    };

    // A type with settings and no params.
    ([$($g:tt)*] $ty:ty, settings $cfg:ident {
        $($name:ident : $st:ty = $default:expr),+ $(,)?
    }) => {
        const _: () = {
            fn is_a_setting_word<T: $crate::__private::SettingWord>() {}
            #[allow(dead_code)]
            fn every_setting_is_a_word() {
                $(is_a_setting_word::<$st>();)+
            }
        };

        #[doc = concat!("The initial settings of a `", stringify!($ty), "`: each entry's value, set by a chained option.")]
        #[derive(Clone, Copy, Debug)]
        pub struct $cfg {
            $($name: $st,)+
        }

        impl $cfg {
            /// The configuration with every setting at its declared default.
            #[must_use]
            pub fn new() -> Self {
                Self { $($name: $default,)+ }
            }

            $(
                #[doc = concat!("The initial value of `", stringify!($name), "`.")]
                #[must_use]
                pub fn $name(mut self, value: $st) -> Self {
                    self.$name = value;
                    self
                }
            )+
        }

        impl Default for $cfg {
            fn default() -> Self {
                Self::new()
            }
        }

        impl<$($g)*> $ty {
            $(
                #[doc = concat!("The setting `", stringify!($name), "`: `set` on the control side, `get` once per block on the audio side. The owner hands out the cell until the session's key target takes it.")]
                pub fn $name(&self) -> &$crate::__private::Setting<$st> {
                    &self.$name
                }
            )+
        }
    };

    ([$($g:tt)*] $ty:ty, $n:ident { $($rows:tt)* }) => {
        impl<$($g)*> $ty {
            #[allow(dead_code)]
            fn declared_visit(&self, visit: $crate::__private::Visit<'_>) -> bool {
                let $n = self;
                $crate::declare_params!(@walk $n visit [] true; $($rows)*);
                true
            }

            /// The params the graph may modulate, in port order.
            #[allow(dead_code)]
            fn declared_ports(&self) -> $crate::ParamPorts {
                $crate::__private::ports(|visit| self.declared_visit(visit))
            }

            /// The params a host reaches by address.
            #[allow(dead_code)]
            fn declared_set(&self) -> $crate::ParamSet {
                $crate::__private::set(|visit| self.declared_visit(visit))
            }

            /// Port `k`'s cell value.
            #[allow(dead_code)]
            fn declared_base(&self, k: usize) -> Option<f32> {
                $crate::__private::base(|visit| self.declared_visit(visit), k)
            }

            /// What this block reads for the param of kind `P` (a
            /// `UnitParam` as `u16`), through the port its row declares.
            /// Does not compile for a kind the node does not declare.
            #[allow(dead_code)]
            fn declared_param<'a, const P: u16>(
                &self,
                io: &$crate::Io<'a>,
            ) -> $crate::ParamInput<'a>
            where
                Self: $crate::HasParam<P>,
            {
                $crate::__private::input(|visit| self.declared_visit(visit), io, P, 0)
            }

            /// [`declared_param`](Self::declared_param) for the group row
            /// `index` of kind `P` (a band's threshold).
            #[allow(dead_code)]
            fn declared_param_at<'a, const P: u16>(
                &self,
                io: &$crate::Io<'a>,
                index: u8,
            ) -> $crate::ParamInput<'a>
            where
                Self: $crate::HasParams<P>,
            {
                $crate::__private::input(|visit| self.declared_visit(visit), io, P, index)
            }
        }
        $crate::declare_params!(@has [$($g)*] $ty; [] $($rows)*);
    };

    // The walk: a row, a group, and the `;` that turns the rest to set-only.
    (@walk $n:ident $v:ident [$($i:ident)?] $port:tt;) => {};
    (@walk $n:ident $v:ident [$($i:ident)?] $port:tt; ; $($rest:tt)*) => {
        $crate::declare_params!(@walk $n $v [$($i)?] false; $($rest)*);
    };
    (@walk $n:ident $v:ident [] $port:tt;
        [$i:ident in $range:expr] { $($body:tt)* } $(, $($rest:tt)*)?) => {
        for $i in $range {
            $crate::declare_params!(@walk $n $v [$i] true; $($body)*);
        }
        $crate::declare_params!(@walk $n $v [] $port; $($($rest)*)?);
    };
    (@walk $n:ident $v:ident [$($i:ident)?] $port:tt;
        $kind:ident => $unit:ty = $cell:expr $(, $($rest:tt)*)?) => {
        $crate::declare_params!(@row $v [$($i)?] $port; $kind, $unit, $cell);
        $crate::declare_params!(@walk $n $v [$($i)?] $port; $($($rest)*)?);
    };
    (@walk $n:ident $v:ident [$($i:ident)?] $port:tt;
        $kind:ident => $unit:ty = $cell:expr ; $($rest:tt)*) => {
        $crate::declare_params!(@row $v [$($i)?] $port; $kind, $unit, $cell);
        $crate::declare_params!(@walk $n $v [$($i)?] false; $($rest)*);
    };

    (@row $v:ident [] $port:tt; $kind:ident, $unit:ty, $cell:expr) => {
        if !$crate::__private::row::<$unit, _>(
            &mut *$v,
            $crate::__private::UnitAddr::first($crate::__private::UnitParam::$kind),
            $port,
            $cell,
        ) {
            return false;
        }
    };
    (@row $v:ident [$i:ident] $port:tt; $kind:ident, $unit:ty, $cell:expr) => {
        if !$crate::__private::row::<$unit, _>(
            &mut *$v,
            $crate::__private::UnitAddr::new($crate::__private::UnitParam::$kind, $i as u8),
            $port,
            $cell,
        ) {
            return false;
        }
    };

    // The typed side: one `HasParam` per kind, `HasParams` for a group's.
    (@has [$($g:tt)*] $ty:ty; [$($grp:ident)?]) => {};
    (@has [$($g:tt)*] $ty:ty; [$($grp:ident)?] ; $($rest:tt)*) => {
        $crate::declare_params!(@has [$($g)*] $ty; [$($grp)?] $($rest)*);
    };
    (@has [$($g:tt)*] $ty:ty; []
        [$i:ident in $range:expr] { $($body:tt)* } $(, $($rest:tt)*)?) => {
        $crate::declare_params!(@has [$($g)*] $ty; [$i] $($body)*);
        $crate::declare_params!(@has [$($g)*] $ty; [] $($($rest)*)?);
    };
    (@has [$($g:tt)*] $ty:ty; [$($grp:ident)?]
        $kind:ident => $unit:ty = $cell:expr $(, $($rest:tt)*)?) => {
        $crate::declare_params!(@one [$($g)*] $ty; [$($grp)?] $kind, $unit);
        $crate::declare_params!(@has [$($g)*] $ty; [$($grp)?] $($($rest)*)?);
    };
    (@has [$($g:tt)*] $ty:ty; [$($grp:ident)?]
        $kind:ident => $unit:ty = $cell:expr ; $($rest:tt)*) => {
        $crate::declare_params!(@one [$($g)*] $ty; [$($grp)?] $kind, $unit);
        $crate::declare_params!(@has [$($g)*] $ty; [$($grp)?] $($rest)*);
    };

    (@one [$($g:tt)*] $ty:ty; [] $kind:ident, $unit:ty) => {
        impl<$($g)*> $crate::HasParam<{ $crate::__private::UnitParam::$kind as u16 }> for $ty {
            type Unit = $unit;
        }
    };
    (@one [$($g:tt)*] $ty:ty; [$i:ident] $kind:ident, $unit:ty) => {
        impl<$($g)*> $crate::HasParam<{ $crate::__private::UnitParam::$kind as u16 }> for $ty {
            type Unit = $unit;
        }
        impl<$($g)*> $crate::HasParams<{ $crate::__private::UnitParam::$kind as u16 }> for $ty {}
    };
}
