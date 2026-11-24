//! [`Param<U>`](Param), a lock-free parameter cell typed by its unit.
//!
//! Wraps an `Arc` of an atomic float with a `PhantomData<U>` marker so
//! different units are distinct types at compile time, at zero runtime cost
//! over a bare `Arc<AtomicF64>`: every continuous unit's raw is `f64`.
//! `SampleRate` is not modulated at audio rate: construct it directly,
//! don't wrap it in `Param`.
//!
//! RT-safety: `load`/`store` never clone the `Arc`. Only `handle()` does, and
//! it is intended for control-thread construction, not the audio path.
//!
//! [`ParamCell`] is a cell addressed without its unit type: the value a host
//! that addresses params by kind (a modulation lane's knob,
//! `tutti_graph::ParamSet`) reads and writes as `f64`.
//!
//! [`Param64<U>`](Param64) is a different thing: an inline, unshared `f64`
//! cell, for a hot loop's own state.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU64, Ordering};

use atomic_float::AtomicF64;
use std::sync::Arc;
use tutti_units::Unit;

/// A [`Param`]'s cell without its unit, read and written as `f64`.
///
/// What a host that addresses params by kind rather than by unit holds: the
/// modulation path (control lanes, `ParamSet`) works in `f64` values, the raw
/// of every unit.
#[derive(Clone, Debug)]
pub struct ParamCell(Arc<AtomicF64>);

impl ParamCell {
    /// Loads the value.
    #[inline]
    pub fn load(&self, order: Ordering) -> f64 {
        self.0.load(order)
    }

    /// Stores a value.
    #[inline]
    pub fn store(&self, value: f64, order: Ordering) {
        self.0.store(value, order);
    }

    /// A fresh cell holding the value now: control thread (allocates).
    #[must_use]
    pub fn detached(&self) -> Self {
        Self(Arc::new(AtomicF64::new(self.0.load(Ordering::Acquire))))
    }

    /// Whether both are the same cell.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl From<Arc<AtomicF64>> for ParamCell {
    fn from(c: Arc<AtomicF64>) -> Self {
        Self(c)
    }
}

impl From<ParamCell> for Arc<AtomicF64> {
    fn from(c: ParamCell) -> Self {
        c.0
    }
}

/// A shared, lock-free scalar parameter carrying its unit in the type.
///
/// Cloning shares the cell rather than copying the value, so a control thread
/// and the audio thread hold the same `Param` and neither allocates to read or
/// write it. [`load`](Self::load) and [`store`](Self::store) are safe on the
/// audio thread; [`new`](Self::new), [`handle`](Self::handle) and
/// [`detach`](Self::detach) are for the control thread.
///
/// The cell holds the unit's raw `f64`.
/// [`SampleRate`](tutti_units::SampleRate) is not modulated at audio rate, so it
/// is used directly rather than through a `Param`.
///
/// # Examples
///
/// ```
/// use tutti_units::Hz;
/// use tutti_rt::Param;
///
/// let cutoff = Param::new(Hz::new(1_000.0)); // control thread: allocates
/// let on_audio_thread = cutoff.handle(); // shares the cell
///
/// cutoff.store(Hz::new(2_000.0));
/// assert_eq!(on_audio_thread.load(), Hz::new(2_000.0));
/// ```
pub struct Param<U: Unit<Raw = f64>> {
    inner: Arc<AtomicF64>,
    _unit: PhantomData<U>,
}

impl<U: Unit<Raw = f64>> Param<U> {
    /// Allocates a new cell holding `v`.
    ///
    /// Control-thread only: this is the one constructor that allocates.
    #[inline]
    pub fn new(v: U) -> Self {
        Self {
            inner: Arc::new(AtomicF64::new(v.to_raw())),
            _unit: PhantomData,
        }
    }

    /// Loads the current value with `Acquire` ordering, pairing with `store`'s
    /// `Release`.
    ///
    /// Audio-thread safe: no clone, no allocation.
    #[inline]
    pub fn load(&self) -> U {
        U::from_raw(self.inner.load(Ordering::Acquire))
    }

    /// Stores a new value with `Release` ordering.
    ///
    /// Audio-thread safe; every holder of the cell sees it.
    #[inline]
    pub fn store(&self, v: U) {
        self.inner.store(v.to_raw(), Ordering::Release);
    }

    /// Whether both are handles on the same cell.
    #[inline]
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Returns another handle on the same cell, for control-thread automation.
    ///
    /// Clones the `Arc`, so keep it off the audio path. Same as `clone`, but
    /// named so hot-path code visibly avoids it.
    #[inline]
    #[must_use]
    pub fn handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _unit: PhantomData,
        }
    }

    /// Stops sharing the cell, replacing it with a fresh one that holds the
    /// *current* value.
    ///
    /// Every other holder of the old cell (the live node, a UI handle, a mod
    /// target) keeps it; this copy no longer sees their writes, and they no
    /// longer see this copy's.
    ///
    /// A node's fork for an offline render calls this on each control the node
    /// reads, so the fork renders a snapshot of the controls taken at fork time rather
    /// than following live knob moves while it runs. Keeping the value is the
    /// point: a fork that snapped to a default would render something nobody
    /// heard. Control-thread only: it allocates, like [`Param::new`].
    #[inline]
    pub fn detach(&mut self) {
        *self = Self::new(self.load());
    }

    /// Returns the shared raw atomic, for APIs that address parameters
    /// without their unit types.
    ///
    /// Clones the `Arc`; do not call on the audio thread.
    #[inline]
    #[must_use]
    pub fn as_atomic(&self) -> Arc<AtomicF64> {
        Arc::clone(&self.inner)
    }

    /// The shared cell as a [`ParamCell`].
    ///
    /// Clones the `Arc`; do not call on the audio thread.
    #[inline]
    #[must_use]
    pub fn cell(&self) -> ParamCell {
        self.as_atomic().into()
    }
}

impl<U: Unit<Raw = f64>> Clone for Param<U> {
    #[inline]
    fn clone(&self) -> Self {
        self.handle()
    }
}

impl<U: Unit<Raw = f64> + Default> Default for Param<U> {
    #[inline]
    fn default() -> Self {
        Self::new(U::default())
    }
}

impl<U: Unit<Raw = f64>> core::fmt::Debug for Param<U> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Param")
            .field("value", &self.load())
            .finish()
    }
}

/// A lock-free scalar cell for a unit whose raw representation is `f64`
/// (`Unit<Raw = f64>`), held inline: [`Param`]'s neighbour for a hot loop's own
/// state, not a mode of it.
///
/// The width is the reason it exists. [`ReadRate`](tutti_units::ReadRate) and
/// [`SamplePosition`](tutti_units::SamplePosition) are `f64` because a disk voice adds
/// the rate into its read position once per sample for the length of the file.
/// A cell that stored the rate as `f32` and widened it on load would put that
/// rounding back into the sum on every load, and the error does not average
/// out: it integrates. (A `Param<ReadRate>` would hold the full width too, but
/// shared through an `Arc`; this cell is for state nothing else holds.)
///
/// Unlike [`Param`], nothing is shared by cloning: the cell is an inline
/// [`AtomicU64`] holding the value's bits, construction does not allocate, and
/// another thread reaches it through `&self` on whatever already shares its
/// owner (a voice's `RtState`, a memory source). It is not what a node's
/// control is: those are `Param`s, which a host addresses by `UnitAddr`. There
/// is no `Default`: a default [`ReadRate`](tutti_units::ReadRate) is `0.0`, a stopped
/// voice, so the initial value is always named.
///
/// [`load`](Self::load) and [`store`](Self::store) are audio-thread safe, with
/// `Param`'s `Acquire`/`Release` pairing. The bits round-trip exactly, a `NaN`
/// payload included.
///
/// # Examples
///
/// ```
/// use tutti_units::ReadRate;
/// use tutti_rt::Param64;
///
/// let stretch_rate = Param64::new(ReadRate::UNITY); // no allocation
/// stretch_rate.store(ReadRate::new(0.75));
/// assert_eq!(stretch_rate.load(), ReadRate::new(0.75));
/// ```
///
/// A count is not a `Param64`:
///
/// ```compile_fail
/// use tutti_units::Samples;
/// use tutti_rt::Param64;
/// struct Delay {
///     length: Param64<Samples>, // a count is not a continuous quantity
/// }
/// ```
pub struct Param64<U: Unit<Raw = f64>> {
    inner: AtomicU64,
    _unit: PhantomData<U>,
}

impl<U: Unit<Raw = f64>> Param64<U> {
    /// A cell holding `v`. Does not allocate.
    #[inline]
    pub fn new(v: U) -> Self {
        Self {
            inner: AtomicU64::new(v.to_raw().to_bits()),
            _unit: PhantomData,
        }
    }

    /// Loads the current value with `Acquire` ordering, pairing with `store`'s
    /// `Release`.
    ///
    /// Audio-thread safe.
    #[inline]
    pub fn load(&self) -> U {
        U::from_raw(f64::from_bits(self.inner.load(Ordering::Acquire)))
    }

    /// Stores a new value with `Release` ordering.
    ///
    /// Audio-thread safe.
    #[inline]
    pub fn store(&self, v: U) {
        self.inner.store(v.to_raw().to_bits(), Ordering::Release);
    }
}

impl<U: Unit<Raw = f64>> core::fmt::Debug for Param64<U> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Param64")
            .field("value", &self.load())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{Param, Param64};
    use core::sync::atomic::Ordering;
    use tutti_units::{Gain, Hz, ReadRate, SamplePosition, SampleSpan};

    /// Both `f64` units round-trip through the cell bit for bit: a value `f32`
    /// cannot hold, an hour's position at 48 kHz plus a fraction, and a `NaN`
    /// whose payload bits survive `to_bits`/`from_bits`.
    #[test]
    fn param64_round_trips_its_bits() {
        let nan = f64::from_bits(0x7ff8_0000_dead_beef);
        assert!(nan.is_nan());

        let rates = [1.0 + f64::EPSILON, 0.1, 17.4, nan];
        let cell = Param64::new(ReadRate::UNITY);
        for want in rates {
            cell.store(ReadRate::new(want));
            assert_eq!(cell.load().get().to_bits(), want.to_bits(), "{want}");
        }
        assert_eq!(
            Param64::new(ReadRate::new(nan)).load().get().to_bits(),
            nan.to_bits()
        );

        let positions = [172_800_000.0 + 0.25, 1.0 / 3.0, -0.0, nan];
        let cell = Param64::new(SamplePosition::new(0.0));
        for want in positions {
            cell.store(SamplePosition::new(want));
            assert_eq!(cell.load().get().to_bits(), want.to_bits(), "{want}");
        }
    }

    /// An `f64` unit's `Param` holds the full width, shares one cell across
    /// handles, and a `ParamCell` reads and writes it at full width.
    #[test]
    fn an_f64_param_keeps_its_width_and_its_cell_is_shared() {
        use tutti_units::Seconds;
        let long = Seconds::new(3600.0 + 1.0e-9); // not an f32
        let a = Param::new(Seconds::new(0.0));
        let b = a.handle();
        a.store(long);
        assert_eq!(b.load().get().to_bits(), long.get().to_bits());

        let cell = a.cell();
        assert_eq!(cell.load(Ordering::Acquire).to_bits(), long.get().to_bits());
        cell.store(0.25, Ordering::Release);
        assert_eq!(b.load(), Seconds::new(0.25));
        assert!(cell.ptr_eq(&b.cell()));
    }

    /// `detach` keeps the value and cuts the cell both ways.
    #[test]
    fn detach_keeps_the_value_and_severs_the_cell() {
        let live = Param::new(Hz::new(440.0));
        let mut copy = live.handle();
        copy.detach();
        assert_eq!(
            copy.load(),
            Hz::new(440.0),
            "detach keeps the current value"
        );

        live.store(Hz::new(880.0));
        assert_eq!(
            copy.load(),
            Hz::new(440.0),
            "a live write must not reach it"
        );
        copy.store(Hz::new(110.0));
        assert_eq!(
            live.load(),
            Hz::new(880.0),
            "its write must not reach the live cell"
        );
    }

    /// A tap, a routing weight: a driven, signed cell that reads back as written.
    #[test]
    fn a_gain_is_a_param() {
        let tap = Param::new(Gain::new(-0.5));
        let on_audio_thread = tap.handle();
        assert_eq!(on_audio_thread.load(), Gain::new(-0.5));
        tap.store(Gain::INVERTING);
        assert_eq!(on_audio_thread.load(), Gain::INVERTING);

        // The cell a modulation lane writes keeps the sign.
        tap.cell()
            .store(0.75, core::sync::atomic::Ordering::Relaxed);
        assert_eq!(tap.load(), Gain::new(0.75));
    }

    /// A driven delay time: the cell holds a span at full `f64` width, a write
    /// reads back exact, and a cell written out of range reads back saturated.
    #[test]
    fn a_span_is_a_param() {
        let time = Param::new(SampleSpan::of(504.25));
        let on_audio_thread = time.handle();
        assert_eq!(on_audio_thread.load(), SampleSpan::of(504.25));
        time.store(SampleSpan::of(48_000.5));
        assert_eq!(on_audio_thread.load().get(), 48_000.5);

        // The cell a modulation lane writes carries an `f64`.
        let cell = time.cell();
        cell.store(-3.0, core::sync::atomic::Ordering::Relaxed);
        assert_eq!(time.load(), SampleSpan::ZERO);
        cell.store(100.0, core::sync::atomic::Ordering::Relaxed);
        assert_eq!(time.load(), SampleSpan::of(100.0));
    }
}
