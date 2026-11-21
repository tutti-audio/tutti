//! [`Normalized`], a neutral value on `0..=1`.

// `Normalized` is a `Unit` (below), which is already a `Quantity`, so it takes the type
// and its error without the macro's own `Quantity` impl.
crate::ranged! {
    @items []
    /// A position on the unit interval, `0..=1`, with no meaning beyond it.
    ///
    /// It is what a plugin parameter's normalized value and a MIDI velocity or
    /// controller value scaled to `0..=1` are, before anything reads them as a
    /// level, a depth or a mix. Which plain value a position stands for is the
    /// reader's answer, never a formula here.
    ///
    /// A `Normalized` is always a finite value on `0..=1`. [`new`](Self::new)
    /// refuses anything else (NaN included) and so does deserializing one.
    /// [`clamped`](Self::clamped) is the named saturation, for a value that
    /// arrives from outside and is better pinned than dropped (automation
    /// output, a plugin's own report).
    ///
    /// Deliberately no `Deref`, no `From<f64>` and no arithmetic: those are the
    /// routes by which a raw float becomes a `Normalized` without passing the
    /// check.
    ///
    /// ```
    /// use tutti_units::Normalized;
    ///
    /// assert_eq!(Normalized::new(0.25).map(Normalized::get), Some(0.25));
    /// assert_eq!(Normalized::new(40.0), None);
    /// assert_eq!(Normalized::clamped(40.0), Normalized::of(1.0));
    /// assert_eq!(Normalized::clamped(f64::NAN), Normalized::of(0.0));
    /// ```
    #[repr(transparent)]
    #[derive(Default, PartialOrd)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "f64", into = "f64"))]
    Normalized(f64), NormalizedOutOfRange, "on 0..=1",
    |raw| raw >= 0.0 && raw <= 1.0
}

/// `from_raw` saturates ([`Normalized::clamped`]): a parameter cell holds any float, and a
/// unit's `from_raw` cannot refuse.
impl crate::Unit for Normalized {
    type Raw = f64;
    #[inline]
    fn from_raw(v: f64) -> Self {
        Self::clamped(v)
    }
    #[inline]
    fn to_raw(self) -> f64 {
        self.get()
    }
}

impl Normalized {
    /// `v` pinned onto `0..=1`: out-of-range input saturates at the nearest
    /// bound, and NaN becomes `0.0`.
    ///
    /// NaN maps to `0.0` rather than clamping, because `f64::clamp` returns NaN
    /// for a NaN input. This value reaches a live plugin parameter, and a NaN in
    /// a filter coefficient does not stay confined to the parameter it entered.
    /// The saturation reports nothing, so use it only where a pinned value is
    /// better than a dropped one.
    #[must_use]
    pub const fn clamped(v: f64) -> Self {
        if v.is_nan() || v < 0.0 {
            Self(0.0)
        } else if v > 1.0 {
            Self(1.0)
        } else {
            Self(v)
        }
    }

    /// How far along `done` frames of `of` are: `done / of`, pinned onto
    /// `0..=1` as [`clamped`](Self::clamped) pins it (`of == 0` reads as
    /// arrived unless `done` is 0 too, which reads as not started). The one
    /// spelling of a frame count as progress.
    #[must_use]
    pub fn progress(done: crate::Samples, of: crate::Samples) -> Self {
        Self::clamped(done.get() as f64 / of.get() as f64)
    }
}
