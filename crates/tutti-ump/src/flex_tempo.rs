//! [`FlexTempo`]: the Set Tempo field of a Flex Data message.

use tutti_units::{Bpm, Seconds};

/// 10-nanosecond units in one second.
const TEN_NS_UNITS_PER_SECOND: f64 = 100_000_000.0;

/// 10-nanosecond units in one microsecond.
const TEN_NS_UNITS_PER_MICRO: u32 = 100;

/// The Set Tempo field (M2-104 §7.5.3): the number of 10 ns units in one
/// quarter note, so a beat lasts `get() / 1e8` seconds.
///
/// Every `u32` is a field the wire carries, and `0` declares no tempo, so
/// [`bpm`](Self::bpm) is `None` for it. The slowest tempo the field holds is
/// 1.39698 bpm, and the fastest, one unit, is 6e9 bpm.
///
/// ```
/// use tutti_ump::FlexTempo;
/// use tutti_units::Bpm;
///
/// // 0.5 s per quarter note is 120 bpm.
/// assert_eq!(FlexTempo::new(50_000_000).bpm(), Bpm::new(120.0));
/// assert_eq!(FlexTempo::new(0).bpm(), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u32", into = "u32"))]
pub struct FlexTempo(u32);

// Every `u32` is a field the wire carries, so the serde form reads back through
// a conversion that cannot fail; a value enters through `new` otherwise.
#[cfg(feature = "serde")]
#[allow(
    clippy::infallible_try_from,
    reason = "no `From<u32>`: a value enters through `new`; this is the serde form"
)]
impl TryFrom<u32> for FlexTempo {
    type Error = core::convert::Infallible;

    fn try_from(units: u32) -> Result<Self, Self::Error> {
        Ok(Self(units))
    }
}

#[cfg(feature = "serde")]
impl From<FlexTempo> for u32 {
    fn from(tempo: FlexTempo) -> u32 {
        tempo.0
    }
}

impl FlexTempo {
    /// The field of `units` 10 ns units per quarter note.
    #[must_use]
    pub const fn new(units: u32) -> Self {
        Self(units)
    }

    /// The number of 10 ns units per quarter note.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The field of a quarter note that lasts `micros` microseconds, as a
    /// Standard MIDI File's Set Tempo meta event (`FF 51`) holds it. A period
    /// past the 32-bit field (over about 42.9 s) is the slowest the field holds.
    #[must_use]
    pub const fn from_micros_per_quarter(micros: u32) -> Self {
        Self(micros.saturating_mul(TEN_NS_UNITS_PER_MICRO))
    }

    /// The field that declares `bpm`: its beat period in 10 ns units, rounded to
    /// the nearest unit, the inverse of [`bpm`](Self::bpm). A tempo too slow for
    /// the field (under 1.39698 bpm) is the slowest the field holds, and one too
    /// fast for a unit (over 6e9 bpm) is one unit.
    ///
    /// ```
    /// use tutti_ump::FlexTempo;
    /// use tutti_units::Bpm;
    ///
    /// let bpm = Bpm::new(120.0).unwrap();
    /// assert_eq!(FlexTempo::from_bpm(bpm).get(), 50_000_000);
    /// assert_eq!(FlexTempo::from_bpm(bpm).bpm(), Some(bpm));
    /// ```
    #[must_use]
    pub fn from_bpm(bpm: Bpm) -> Self {
        // `as` saturates the float into the field; `max(1)` keeps a field that
        // `bpm` reads as a tempo.
        Self((libm::round(bpm.beat_period().get() * TEN_NS_UNITS_PER_SECOND) as u32).max(1))
    }

    /// The tempo the field declares, `None` for `0`: no valid tempo.
    #[must_use]
    pub fn bpm(self) -> Option<Bpm> {
        (self.0 != 0)
            .then(|| {
                Bpm::from_beat_period(Seconds::new(f64::from(self.0) / TEN_NS_UNITS_PER_SECOND))
            })
            .flatten()
    }
}
