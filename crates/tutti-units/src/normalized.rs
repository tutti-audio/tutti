//! [`Normalized`], a neutral value on `0..=1`.

crate::ranged! {
    /// A parameter value on the host's `0..=1` scale.
    ///
    /// The four hosted plugin formats disagree about which domain a parameter
    /// value is in: VST3 and VST2 speak normalized natively, AU and CLAP speak
    /// the plugin's own units. The shared seam picks normalized, leaving each
    /// format to convert at its own edge, so what a `Normalized` means in plain
    /// units is the format's answer, never a formula over a range. Carried only
    /// in prose over a bare `f64`, that rule is unenforceable: writing
    /// `set_parameter(id, 20_000.0)` against a filter cutoff compiles and sets
    /// the parameter to full scale rather than 20 kHz.
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
}
