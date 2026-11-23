//! A node's params, declared once ([`declare_params!`](crate::declare_params)),
//! and the typed vocabulary they are declared in: [`ParamValue`], [`HasParam`].
//!
//! A node with params says three things about them: which the graph may
//! modulate (its [`Shape`](crate::Shape)'s ports), which a host reaches by
//! address (its [`ParamSet`]), and what unit each is in (what a typed key
//! hands a caller). The declaration states them once, next to the node's
//! `Param<U>` cells, and generates all three, so they cannot drift and the
//! unit is checked against the cell's own type.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use atomic_float::AtomicF64;

use tutti_rt::{Param, ParamCell};
use tutti_units::{
    Amplitude, Azimuth, Balance, Bipolar, BitDepth, Cents, CentsSpan, CompressionRatio, Db, Drive,
    Elevation, Feedback, Gain, HoldFactor, Hz, Metres, Mix, ModulationIndex, Normalized, Pan, Q,
    Resonance, SampleSpan, Seconds, Semitones, Spread, StereoWidth, Unit, UnitAddr,
};

use crate::controls::{DeclaredUnit, ParamSet, Params};
use crate::io::Io;
use crate::param::{MAX_PARAM_PORTS, ParamInput, ParamPorts};
use tutti_buffers::Float;

mod sealed {
    pub trait Sealed {}
}

/// What a param's value is: a unit tutti defines ([`Scalar`]), or a `bool` for
/// a switch (`Bypass`, `Solo`, `Freeze`: a cell at or above 0.5 is on). Sealed:
/// the engine's units are listed, and a new one joins them.
pub trait ParamValue: Copy + PartialEq + Send + Sync + 'static + sealed::Sealed {
    /// The value as the `f64` the param's cell carries: the unit's raw.
    fn to_cell(self) -> f64;

    /// The value a cell's `f64` stands for: the inverse of
    /// [`to_cell`](Self::to_cell).
    fn from_cell(cell: f64) -> Self;
}

/// A [`ParamValue`] that is a number in a unit (not a switch): what a range
/// (`Mods::range`) is a range of. Sealed.
pub trait Scalar: ParamValue + Unit<Raw = f64> {}

macro_rules! scalar_units {
    ($($unit:ty),+ $(,)?) => {$(
        impl sealed::Sealed for $unit {}
        impl ParamValue for $unit {
            fn to_cell(self) -> f64 {
                <$unit as Unit>::to_raw(self)
            }
            fn from_cell(cell: f64) -> Self {
                <$unit as Unit>::from_raw(cell)
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
    Cents,
    CentsSpan,
    CompressionRatio,
    Db,
    Bipolar,
    Drive,
    Elevation,
    Feedback,
    Gain,
    HoldFactor,
    Hz,
    Metres,
    Mix,
    ModulationIndex,
    Normalized,
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
impl ParamValue for bool {
    fn to_cell(self) -> f64 {
        f64::from(u8::from(self))
    }

    fn from_cell(cell: f64) -> Self {
        cell >= 0.5
    }
}

/// That the node `Self` has a native param of the kind `P` (a packed
/// [`UnitAddr`] at index 0: `UnitParam::Cutoff as u16`), whose value is in
/// `Unit`. What a typed key's param method for that kind needs, and what
/// [`declare_params!`](crate::declare_params) writes from the node's own
/// cells.
pub trait HasParam<const P: u16> {
    /// The unit the param's value is in.
    type Unit: ParamValue;
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

/// A cell the macro's `Cells` struct holds: one `Param<U>`, or an array of
/// them for a kind with several indices. What lets the generated `from_config`
/// and `detached` treat both alike.
///
/// Hidden: `declare_params!` writes the calls; an array is built from an
/// array of values, so a count the declaration states is the count the
/// option takes.
#[doc(hidden)]
pub trait OwnedCell: Sized {
    /// The initial value: a unit, or an array of them.
    type Value;

    /// A cell (or each of an array's) holding `value`.
    fn from_value(value: Self::Value) -> Self;

    /// A copy that shares nothing with `self` and holds the values it holds
    /// now ([`Param::detach`]). Control thread: allocates.
    #[must_use]
    fn detached(&self) -> Self;
}

impl<U: Scalar> OwnedCell for Param<U> {
    type Value = U;

    fn from_value(value: U) -> Self {
        Param::new(value)
    }

    fn detached(&self) -> Self {
        let mut cell = self.handle();
        cell.detach();
        cell
    }
}

impl<C: OwnedCell, const N: usize> OwnedCell for [C; N] {
    type Value = [C::Value; N];

    fn from_value(value: [C::Value; N]) -> Self {
        value.map(C::from_value)
    }

    fn detached(&self) -> Self {
        std::array::from_fn(|i| self[i].detached())
    }
}

impl<C: OwnedCell> OwnedCell for Option<C> {
    type Value = Option<C::Value>;

    fn from_value(value: Option<C::Value>) -> Self {
        value.map(C::from_value)
    }

    fn detached(&self) -> Self {
        self.as_ref().map(C::detached)
    }
}

/// What one `params` entry of the cells form is the value of: a unit `U`
/// (the cell is a `Param<U>`) or an `Option<U>` (the node has the param only
/// when the config gives a value). It names the unit the entry's row is in
/// and the cell the node holds.
///
/// Hidden: `declare_params!` names it to write the cell's type and the row's
/// unit from the one type an entry states.
#[doc(hidden)]
pub trait ParamEntry: Copy {
    /// The unit the param's value is in.
    type Unit: ParamValue;

    /// The cell the node holds; its initial value is `Self`.
    type Cell: OwnedCell<Value = Self>;
}

impl<U: Scalar> ParamEntry for U {
    type Unit = U;
    type Cell = Param<U>;
}

impl<U: Scalar> ParamEntry for Option<U> {
    type Unit = U;
    type Cell = Option<Param<U>>;
}

/// A cell a declaration names, whose value is in `U`: what makes the unit a
/// declaration states agree with the cell's own type. Implemented for a
/// `&Param<U>` (any unit but its own does not compile), for a switch's
/// `&Arc<AtomicF64>` (`bool` only: 0.0 off, 1.0 on), and for an `Option` of either, where the
/// node has the param only sometimes.
#[doc(hidden)]
pub trait Cells<U: ParamValue> {
    /// The cell's value as an `f64`, or `None` where the node has no such
    /// param.
    fn base(&self) -> Option<f64>;

    /// The cell, shared. Clones the `Arc`: control thread.
    fn cell(&self) -> Option<ParamCell>;
}

impl<U: Scalar> Cells<U> for &Param<U> {
    fn base(&self) -> Option<f64> {
        Some(self.load().to_cell())
    }

    fn cell(&self) -> Option<ParamCell> {
        Some(Param::cell(self))
    }
}

impl<U: Scalar> Cells<U> for &Option<Param<U>> {
    fn base(&self) -> Option<f64> {
        self.as_ref().map(|cell| cell.load().to_cell())
    }

    fn cell(&self) -> Option<ParamCell> {
        self.as_ref().map(Param::cell)
    }
}

impl Cells<bool> for &Arc<AtomicF64> {
    fn base(&self) -> Option<f64> {
        Some(self.load(Ordering::Acquire))
    }

    fn cell(&self) -> Option<ParamCell> {
        Some(ParamCell::from(Arc::clone(self)))
    }
}

impl<U: ParamValue, C: Cells<U>> Cells<U> for Option<C> {
    fn base(&self) -> Option<f64> {
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
    unit: DeclaredUnit,
    port: bool,
    base: f64,
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
pub fn row<U: ParamValue, C: Cells<U>>(
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
        unit: DeclaredUnit::of::<U>(),
        port,
        base,
        cell: &|| cell.cell(),
    })
}

/// The ports a declaration gives the node's shape: its params that are
/// ports, in the order declared.
#[doc(hidden)]
pub fn ports(walk: impl FnOnce(Visit<'_>) -> bool) -> ParamPorts {
    let mut addrs = [UnitAddr::first(tutti_units::UnitParam::Cutoff); MAX_PARAM_PORTS];
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
            set = std::mem::replace(&mut set, ParamSet::builder()).param(row.unit, row.addr, cell);
        }
        true
    });
    set.build()
}

/// Port `k`'s cell value: what [`Node::param_base`](crate::Node::param_base)
/// answers.
#[doc(hidden)]
pub fn base(walk: impl FnOnce(Visit<'_>) -> bool, k: usize) -> Option<f64> {
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
pub fn input<'a, F: Float>(
    walk: impl FnOnce(Visit<'_>) -> bool,
    io: &Io<'a, F>,
    p: u16,
    index: u8,
) -> ParamInput<'a, F> {
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
/// use tutti_units::{Hz, Q};
/// use tutti_rt::Param;
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
/// let filter = Filter { cutoff: Param::new(Hz::new(1_000.0)), q: Param::new(Q::new(0.7)) };
/// // The shape's ports, the node's `ParamSet` and the base of port 1:
/// assert_eq!(filter.declared_ports().as_slice().len(), 2);
/// assert!(filter.declared_set().set(tutti_units::UnitParam::Cutoff, 2_000.0));
/// assert_eq!(filter.declared_base(0), Some(2_000.0));
/// ```
///
/// The unit a row states is checked against the cell's own type: a cutoff
/// declared in the wrong unit does not compile.
///
/// ```compile_fail
/// use tutti_graph::declare_params;
/// use tutti_units::{Db, Hz};
/// use tutti_rt::Param;
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
/// [`UnitParam`](tutti_units::UnitParam) kind, the unit its value is in, and
/// the cell: a `&Param<Unit>`, a `&Arc<AtomicF64>` for a `bool` switch, or an
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
/// [`param_addrs`](crate::Shape::param_addrs)), `declared_set` (the
/// node's [`ParamSet`], for [`ParamNode::param_set`](crate::ParamNode::param_set)) and
/// `declared_base` (port `k`'s cell value, for
/// [`Node::param_base`](crate::Node::param_base)), and the type's
/// [`HasParam`] impls.
///
/// # Settings
///
/// A value that is one word by its encoding (a mode: a fieldless enum written
/// with [`atomic_word!`](tutti_rt::atomic_word)) is not a param: it has no
/// address and no port, and cannot be modulated. It is a
/// [`AtomicCell<T>`](tutti_rt::AtomicCell) field of the type that owns the cells (the
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
///   builds each cell with `AtomicCell::new(config.mode)`;
/// - the control-side handle `owner.mode()`, a `&AtomicCell<Mode>`: `store` lands
///   at once, with no pending slot;
/// - the type check that the field `mode` is an `AtomicCell<Mode>` and that `Mode`
///   is an [`AtomicWord`](tutti_rt::AtomicWord).
///
/// The node reads `self.mode.load()` once per block.
///
/// ```
/// use tutti_graph::declare_params;
/// use tutti_rt::{AtomicCell, atomic_word};
///
/// #[derive(Clone, Copy, Debug, PartialEq)]
/// #[repr(u8)]
/// enum Mode { Off, On }
/// atomic_word!(Mode { Off, On });
///
/// #[derive(Debug)]
/// struct Controls {
///     mode: AtomicCell<Mode>,
/// }
///
/// declare_params!([] Controls, settings ControlsConfig { mode: Mode = Mode::Off });
///
/// impl Controls {
///     fn new(config: ControlsConfig) -> Self {
///         Self { mode: AtomicCell::new(config.mode) }
///     }
/// }
///
/// let controls = Controls::new(ControlsConfig::new().mode(Mode::On));
/// assert_eq!(controls.mode().load(), Mode::On);
/// controls.mode().store(Mode::Off);
/// assert_eq!(controls.mode.load(), Mode::Off);
/// ```
///
/// A cell that is not an `AtomicCell` of the declared type does not compile:
///
/// ```compile_fail
/// use tutti_graph::declare_params;
/// use tutti_rt::{AtomicCell, atomic_word};
///
/// #[derive(Clone, Copy, Debug)]
/// #[repr(u8)]
/// enum Mode { Off }
/// atomic_word!(Mode { Off });
///
/// struct Controls {
///     mode: u8,
/// }
///
/// declare_params!([] Controls, settings ControlsConfig { mode: Mode = Mode::Off });
/// ```
///
/// # The cells form
///
/// `declare_params!([generics] Type, Config, Cells { required { .. } params { .. } settings { .. } })`
/// writes the node's cells and its config from one list, so the node is added
/// bare (`graph.add(node)`, never `Params(node)`). Every block is optional, so
/// a node with no params states `settings`, `required` or nothing. Each `params`
/// entry is `Kind => field: Unit = default`, or `Kind => field[N]: Unit = default`
/// for a kind with `N` indices (addressed `0..N`, each at `default`). The node
/// holds a field named `cells` of type `Cells` (and no other `Param`: `fork_fresh`
/// detaches only `cells`) and derives `Clone`.
///
/// - **An `Option` entry**, `Kind => field: Option<Unit> = None`, is a param the
///   node has only when the config gives a value (`.field(Some(value))`): absent,
///   it is no port and not in the set.
/// - **A set-only entry** follows a `;` after the last entry that is a port
///   (`Cutoff => cutoff: Hz = .. ; Wet => wet: Mix = ..`): reached by address
///   and typed, never a port.
/// - **A setting**, `settings { mode: Mode = Mode::Off }`, is a value that is one
///   word by its encoding (an [`AtomicWord`](tutti_rt::AtomicWord)), not a param: no
///   address, no port. `Cells` holds it as a shared
///   [`AtomicCell`](tutti_rt::AtomicCell) (a clone shares it, a fork gets its
///   own at the value it holds), `cells.mode()` hands it out, and the config
///   has a chained option `.mode(Mode::On)`; the node reads `cells.mode().load()`
///   once per block.
///
/// The macro writes:
///
/// - `Cells`, with a sharing `Clone`, `detached()` (own cells at the values the
///   cells hold now) and `from_config(&Config)`;
/// - `Config::new(required..)`, taking the `required` entries in order (none
///   when the block is left out), and a chained option per param and setting
///   (`.field(value)`; `.field([a, b])` for a counted one) for its initial value;
/// - the params' ports, [`ParamSet`] and [`HasParam`] impls the rows form writes;
/// - [`ParamNode`](crate::ParamNode) (a fork detaches the cells and resets the node), and
///   [`IntoNode`](crate::IntoNode), whose controls are the node's [`ParamSet`].
///
/// The node's `Node` impl forwards to the declaration: `shape` takes
/// `self.declared_ports().as_slice()` and `param_base` answers
/// `self.declared_base(port)`. The fields of `Cells` and `Config`
/// are private to the declaring module, where the node's `From<Config>` builds
/// its cells with `Cells::from_config`.
///
/// ```
/// use tutti_buffers::{ChannelLayout, Float};
/// use tutti_graph::{declare_params, Cx, Io, Node, ParamNode, Prepare, Shape, Status};
/// use tutti_units::{Amplitude, Hz, UnitParam};
///
/// #[derive(Clone)]
/// struct Tone<F: Float> {
///     cells: ToneCells,
///     float: std::marker::PhantomData<fn() -> F>,
/// }
///
/// declare_params!([F: Float] Tone<F>, ToneConfig, ToneCells {
///     params {
///         Cutoff => freq: Hz = Hz::new(440.0),
///         Volume => level: Amplitude = Amplitude::UNITY,
///     }
/// });
///
/// impl<F: Float> From<ToneConfig> for Tone<F> {
///     fn from(config: ToneConfig) -> Self {
///         Self { cells: ToneCells::from_config(&config), float: std::marker::PhantomData }
///     }
/// }
///
/// impl<F: Float> Node<F> for Tone<F> {
///     fn shape(&self) -> Shape {
///         Shape::source_unassigned(ChannelLayout::MONO)
///             .param_addrs(self.declared_ports().as_slice())
///     }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
///         io.output(0).fill(F::from_unit(self.cells.level.load()));
///         Status::Modified
///     }
///     fn reset(&mut self) {}
///     fn param_base(&self, port: usize) -> Option<f64> {
///         self.declared_base(port)
///     }
/// }
///
/// let tone = Tone::<f32>::from(ToneConfig::new().freq(Hz::new(880.0)));
/// let set = tone.param_set();
/// assert_eq!(set.get(UnitParam::Cutoff), Some(880.0));
/// assert_eq!(set.get(UnitParam::Volume), Some(1.0));
/// ```
///
/// # Reading a driven param
///
/// `process` reads a param through the declaration, never by a port index:
/// `declared_param::<{ UnitParam::Cutoff as u16 }, _>(&io)` (`declared_param_at`
/// with the group's `index` for a repeated kind) is the [`ParamInput`] of the
/// port whose row names that kind, so reordering or inserting a row moves the
/// read with it. A kind the node does not declare does not compile. It walks
/// the rows (a handful of loads), so read it once per block, not per frame.
///
/// ```
/// use tutti_graph::{declare_params, Cx, Io, Node, Prepare, Shape, Status};
/// use tutti_units::{Amplitude, Hz, Real, UnitParam};
/// use tutti_rt::Param;
/// use tutti_buffers::ChannelLayout;
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
///             .param_addrs(self.declared_ports().as_slice())
///     }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
///         let level = self
///             .declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
///             .last_frame()
///             .unwrap_or_else(|| f32::from_unit(self.level.load()));
///         io.output(0).fill(level);
///         Status::Modified
///     }
///     fn reset(&mut self) {}
/// }
/// ```
#[macro_export]
macro_rules! declare_params {
    // The cells form: the node's cells, its config and its traits, written
    // from the blocks it states. The entries are gathered, set-only ones
    // marked, for the one rule that writes them.
    ([$($g:tt)*] $ty:ty, $cfg:ident, $cells:ident {
        $(required { $($req:ident : $rty:ty),* $(,)? })?
        $(params {
            $($kind:ident => $field:ident $([$count:expr])? : $unit:ty = $default:expr),* $(,)?
            $(; $($skind:ident => $sfield:ident $([$scount:expr])? : $sunit:ty = $sdefault:expr),+ $(,)?)?
        })?
        $(settings { $($sname:ident : $sty:ty = $sdefault_value:expr),+ $(,)? })?
    }) => {
        $crate::declare_params!(@cells [$($g)*] $ty, $cfg, $cells
            [$($($req : $rty,)*)?]
            [$($(port $kind => $field $([$count])? : $unit = $default,)*
               $($(set $skind => $sfield $([$scount])? : $sunit = $sdefault,)+)?)?]
            [$($($sname : $sty = $sdefault_value,)+)?]);
    };

    (@cells [$($g:tt)*] $ty:ty, $cfg:ident, $cells:ident
        [$($req:ident : $rty:ty,)*]
        [$($mode:ident $kind:ident => $field:ident $([$count:expr])? : $unit:ty = $default:expr,)*]
        [$($sname:ident : $sty:ty = $sdefault:expr,)*]
    ) => {
        #[doc = concat!("The cells of a [`", stringify!($ty), "`], one per declared param and setting. `Clone` shares them; [`detached`](Self::detached) copies them apart.")]
        #[derive(Clone, Debug)]
        pub struct $cells {
            $($field: $crate::declare_params!(@cell [$($count)?] $unit),)*
            $($sname: $crate::__private::Arc<$crate::__private::AtomicCell<$sty>>,)*
        }

        impl $cells {
            /// The cells at the values `config` holds.
            #[must_use]
            pub fn from_config(config: &$cfg) -> Self {
                // A node with no params and no settings reads nothing from it.
                let _ = config;
                Self {
                    $($field: $crate::__private::OwnedCell::from_value(config.$field),)*
                    $($sname: $crate::__private::Arc::new(
                        $crate::__private::AtomicCell::new(config.$sname)
                    ),)*
                }
            }

            /// Cells that share nothing with these, each holding the value its
            /// cell holds now.
            #[must_use]
            pub fn detached(&self) -> Self {
                Self {
                    $($field: $crate::__private::OwnedCell::detached(&self.$field),)*
                    $($sname: $crate::__private::Arc::new(
                        $crate::__private::AtomicCell::new(self.$sname.load())
                    ),)*
                }
            }

            $(
                #[doc = concat!("The setting `", stringify!($sname), "`: `store` on the control side, `load` once per block on the audio side.")]
                #[must_use]
                pub fn $sname(&self) -> &$crate::__private::AtomicCell<$sty> {
                    &self.$sname
                }
            )*
        }

        #[doc = concat!("What a [`", stringify!($ty), "`] is made from: its required values and the initial value of each param and setting, set by a chained option.")]
        #[derive(Clone, Debug)]
        pub struct $cfg {
            $($req: $rty,)*
            $($field: $crate::declare_params!(@value [$($count)?] $unit),)*
            $($sname: $sty,)*
        }

        impl $cfg {
            /// The configuration from its required values, every param and setting at its declared default.
            #[must_use]
            #[allow(clippy::new_without_default)]
            pub fn new($($req: $rty),*) -> Self {
                Self {
                    $($req,)*
                    $($field: $crate::declare_params!(@fill [$($count)?] $default),)*
                    $($sname: $sdefault,)*
                }
            }

            $(
                #[doc = concat!("The initial value of `", stringify!($field), "`.")]
                #[must_use]
                pub fn $field(mut self, value: $crate::declare_params!(@value [$($count)?] $unit)) -> Self {
                    self.$field = value;
                    self
                }
            )*

            $(
                #[doc = concat!("The initial value of the setting `", stringify!($sname), "`.")]
                #[must_use]
                pub fn $sname(mut self, value: $sty) -> Self {
                    self.$sname = value;
                    self
                }
            )*
        }

        $crate::declare_params!(@munch [$($g)*] $ty, n, [];
            $($mode $kind => $field $([$count])? : $unit,)*);

        impl<__LaneF: $crate::__private::Float, $($g)*> $crate::ParamNode<__LaneF> for $ty
        where
            $ty: $crate::Node<__LaneF> + Clone,
        {
            fn param_set(&self) -> $crate::ParamSet {
                self.declared_set()
            }

            fn fork_fresh(&self) -> Self {
                let mut fork = self.clone();
                fork.cells = self.cells.detached();
                $crate::Node::<__LaneF>::reset(&mut fork);
                fork
            }
        }

        impl<__LaneF: $crate::__private::Float, $($g)*> $crate::IntoNode<__LaneF> for $ty
        where
            $ty: $crate::ParamNode<__LaneF> + Clone,
        {
            type Controls = $crate::ParamSet;

            fn into_parts(self) -> $crate::NodeParts<$crate::ParamSet, __LaneF> {
                $crate::param_parts(self)
            }
        }
    };

    // The cells form's rows, written as the rows form: a kind with a count is
    // a group, and a set-only entry follows a `;`.
    (@munch [$($g:tt)*] $ty:ty, $n:ident, [$($acc:tt)*];) => {
        $crate::declare_params!([$($g)*] $ty, $n { $($acc)* });
    };
    (@munch [$($g:tt)*] $ty:ty, $n:ident, [$($acc:tt)*];
        port $kind:ident => $field:ident : $unit:ty, $($rest:tt)*) => {
        $crate::declare_params!(@munch [$($g)*] $ty, $n,
            [$($acc)* $kind => <$unit as $crate::__private::ParamEntry>::Unit = &$n.cells.$field,];
            $($rest)*);
    };
    (@munch [$($g:tt)*] $ty:ty, $n:ident, [$($acc:tt)*];
        port $kind:ident => $field:ident [$count:expr] : $unit:ty, $($rest:tt)*) => {
        $crate::declare_params!(@munch [$($g)*] $ty, $n,
            [$($acc)* [i in 0..$count] { $kind => $unit = &$n.cells.$field[i], },]; $($rest)*);
    };
    (@munch [$($g:tt)*] $ty:ty, $n:ident, [$($acc:tt)*];
        set $kind:ident => $field:ident : $unit:ty, $($rest:tt)*) => {
        $crate::declare_params!(@munch [$($g)*] $ty, $n,
            [$($acc)* ; $kind => <$unit as $crate::__private::ParamEntry>::Unit = &$n.cells.$field,];
            $($rest)*);
    };
    (@munch [$($g:tt)*] $ty:ty, $n:ident, [$($acc:tt)*];
        set $kind:ident => $field:ident [$count:expr] : $unit:ty, $($rest:tt)*) => {
        $crate::declare_params!(@munch [$($g)*] $ty, $n,
            [$($acc)* [i in 0..$count] { ; $kind => $unit = &$n.cells.$field[i], },]; $($rest)*);
    };

    // The type of a cell, of a config's value, and a default filled to a count.
    (@cell [] $unit:ty) => { <$unit as $crate::__private::ParamEntry>::Cell };
    (@cell [$count:expr] $unit:ty) => { [$crate::__private::Param<$unit>; $count] };
    (@value [] $unit:ty) => { $unit };
    (@value [$count:expr] $unit:ty) => { [$unit; $count] };
    (@fill [] $default:expr) => { $default };
    (@fill [$count:expr] $default:expr) => { [$default; $count] };

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
            fn is_a_atomic_word<T: $crate::__private::AtomicWord>() {}
            #[allow(dead_code)]
            fn every_setting_is_a_word() {
                $(is_a_atomic_word::<$st>();)+
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
                #[doc = concat!("The setting `", stringify!($name), "`: `store` on the control side, `load` once per block on the audio side. The owner hands out the cell until the session's key target takes it.")]
                pub fn $name(&self) -> &$crate::__private::AtomicCell<$st> {
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
                // A node with no rows visits nothing.
                let _ = (&$n, &visit);
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
            fn declared_base(&self, k: usize) -> Option<f64> {
                $crate::__private::base(|visit| self.declared_visit(visit), k)
            }

            /// What this block reads for the param of kind `P` (a
            /// `UnitParam` as `u16`), through the port its row declares.
            /// Does not compile for a kind the node does not declare.
            #[allow(dead_code)]
            fn declared_param<'a, const P: u16, __LaneF: $crate::__private::Float>(
                &self,
                io: &$crate::Io<'a, __LaneF>,
            ) -> $crate::ParamInput<'a, __LaneF>
            where
                Self: $crate::HasParam<P>,
            {
                $crate::__private::input(|visit| self.declared_visit(visit), io, P, 0)
            }

            /// [`declared_param`](Self::declared_param) for the group row
            /// `index` of kind `P` (a band's threshold).
            #[allow(dead_code)]
            fn declared_param_at<'a, const P: u16, __LaneF: $crate::__private::Float>(
                &self,
                io: &$crate::Io<'a, __LaneF>,
                index: u8,
            ) -> $crate::ParamInput<'a, __LaneF>
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
