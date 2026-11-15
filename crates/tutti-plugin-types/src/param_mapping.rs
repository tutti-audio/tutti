//! One parameter's map between its normalized position and its plain value.

use crate::Normalized;

/// One parameter's map between its normalized position and its plain value, as
/// its format defines it.
///
/// Each format implements it on its own type, at its own edge: CLAP's
/// `ParamBounds` (a [`LinearRange`] with stepped values on their integer),
/// AU's `ParamBounds` (a [`LinearRange`] in `f32`), VST2's integer range (a
/// [`LinearRange`] between its ends) and VST3's controller (the plugin's own
/// `normalizedParamToPlain`, whatever its shape). The line is the one
/// [`LinearRange`]; [`Control::parameter_plain`] and
/// [`Control::parameter_normalized`] build the parameter's mapping and ask it.
///
/// A mapping answers every input; the laws below hold for all of them, and
/// `contract::mapping_contract!` (feature `contract`) checks them over a sweep
/// of positions and edges:
///
/// 1. **Finite.** Every position on `0..=1` has a finite plain value.
/// 2. **Bounded.** Every plain value maps back onto `0..=1`, clamped where it
///    lies outside the map's range, so an infinity reads as the end of the
///    range it lies past. NaN is the one value that lies past no end: it is
///    `0.0`, the convention for "no value to read", never an escaping NaN.
/// 3. **Stable from the plain side.** `plain(normalized(p)) == p` for every `p`
///    the map produces, bit for bit for a stepped map (CLAP's integer steps)
///    and within rounding of the map's own width for a continuous one.
/// 4. **Monotonic.** Over increasing positions the plain value never changes
///    direction: a map increases, or decreases (VST3's `tone` in the shared
///    test plugin), or is constant. `normalized` is monotonic the same way.
///
/// A mapping that cannot be built (an id the plugin does not declare, a VST2
/// parameter with no integer range, no VST3 controller) is `None` at the
/// constructor of the format, not a failure of these two calls.
///
/// [`Control::parameter_plain`]: crate::Control::parameter_plain
/// [`Control::parameter_normalized`]: crate::Control::parameter_normalized
pub trait ParamMapping {
    /// The plain value of the position `value`: finite, in the map's range.
    #[must_use]
    fn plain(&self, value: Normalized) -> f64;

    /// The position of the plain value `plain`, clamped onto `0..=1`: a value
    /// past an end of the map, an infinity included, reads as that end. NaN is
    /// `0.0`.
    #[must_use]
    fn normalized(&self, plain: f64) -> Normalized;
}

/// The line between a parameter's two declared ends: position `0` is `min`,
/// position `1` is `max`, and the plain value is linear between them.
///
/// A range declared backwards (`min` above `max`) runs backwards: it is the
/// same line, read from the other end. A range with no span, `min` equal to
/// `max` or either end not finite, is a malformed declaration that gets no
/// invented span: its plain value is `min` when that is finite and `0.0`
/// otherwise, and its position is `0.0`. [`normalized`](ParamMapping::normalized)
/// clamps into the range, so a plain value past an end, an infinity included,
/// reads as that end, and NaN is `0.0` (the [`ParamMapping`] laws).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearRange {
    min: f64,
    max: f64,
}

impl LinearRange {
    /// The line from `min` (position `0`) to `max` (position `1`).
    #[must_use]
    pub const fn new(min: f64, max: f64) -> Self {
        Self { min, max }
    }

    /// The end at position `0`.
    #[must_use]
    pub const fn min(self) -> f64 {
        self.min
    }

    /// The end at position `1`.
    #[must_use]
    pub const fn max(self) -> f64 {
        self.max
    }

    /// The ends when the line has a span: both finite and apart.
    fn span(self) -> Option<(f64, f64)> {
        (self.min.is_finite() && self.max.is_finite() && self.min != self.max)
            .then_some((self.min, self.max))
    }
}

impl ParamMapping for LinearRange {
    fn plain(&self, value: Normalized) -> f64 {
        match self.span() {
            Some((min, max)) => min + value.get() * (max - min),
            None if self.min.is_finite() => self.min,
            None => 0.0,
        }
    }

    fn normalized(&self, plain: f64) -> Normalized {
        let Some((min, max)) = self.span() else {
            return Normalized::of(0.0);
        };
        if plain.is_nan() {
            return Normalized::of(0.0);
        }
        let plain = plain.clamp(min.min(max), min.max(max));
        Normalized::clamped((plain - min) / (max - min))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ends are the positions `0` and `1`, and the middle is the middle.
    #[test]
    fn the_line_runs_from_min_at_zero_to_max_at_one() {
        let range = LinearRange::new(20.0, 120.0);
        assert_eq!(range.plain(Normalized::of(0.0)), 20.0);
        assert_eq!(range.plain(Normalized::of(0.5)), 70.0);
        assert_eq!(range.plain(Normalized::of(1.0)), 120.0);
        assert_eq!(range.normalized(70.0).get(), 0.5);
    }

    /// A plain value past an end reads as that end, an infinity too; NaN is
    /// the one value with no end: `0.0`.
    #[test]
    fn a_value_past_an_end_reads_as_that_end_and_nan_as_zero() {
        let range = LinearRange::new(-10.0, 30.0);
        assert_eq!(range.normalized(1e300).get(), 1.0);
        assert_eq!(range.normalized(f64::INFINITY).get(), 1.0);
        assert_eq!(range.normalized(-31.0).get(), 0.0);
        assert_eq!(range.normalized(f64::NEG_INFINITY).get(), 0.0);
        assert_eq!(range.normalized(f64::NAN).get(), 0.0);
    }

    /// A backwards range is the same line from the other end: position `0` is
    /// the declared `min`, so a plain value past the *lower* number reads as
    /// position `1`.
    #[test]
    fn a_backwards_range_runs_backwards() {
        let range = LinearRange::new(10.0, 0.0);
        assert_eq!(range.plain(Normalized::of(0.25)), 7.5);
        assert_eq!(range.normalized(7.5).get(), 0.25);
        assert_eq!(range.normalized(f64::INFINITY).get(), 0.0);
        assert_eq!(range.normalized(f64::NEG_INFINITY).get(), 1.0);
    }

    /// A range with no span is `min` when that is finite, else `0.0`, with
    /// position `0.0`.
    #[test]
    fn a_range_with_no_span_is_its_min_at_position_zero() {
        for (min, max, plain) in [
            (3.0, 3.0, 3.0),
            (3.0, f64::NAN, 3.0),
            (3.0, f64::INFINITY, 3.0),
            (f64::NAN, 1.0, 0.0),
            (f64::NEG_INFINITY, f64::INFINITY, 0.0),
        ] {
            let range = LinearRange::new(min, max);
            assert_eq!(range.plain(Normalized::of(0.7)), plain, "{min}..{max}");
            assert_eq!(range.normalized(5.0).get(), 0.0, "{min}..{max}");
        }
    }
}
