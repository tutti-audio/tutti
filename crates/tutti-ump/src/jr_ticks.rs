//! [`JrTicks`]: a JR Timestamp or JR Clock value.

use tutti_units::{SampleRate, Seconds};

/// A JR Timestamp or JR Clock value (M2-104 §7.2.2): ticks of 1/31250 s in 16
/// bits, so the count wraps every 2.097 s.
///
/// A count is not ordered: of two values, which is later depends on the wrap, so
/// [`since`](Self::since) gives the signed step between them, which is right
/// for values under half a wrap (1.048 s) apart.
///
/// ```
/// use tutti_ump::JrTicks;
///
/// assert_eq!(JrTicks::new(31_250).seconds().get(), 1.0);
/// // 3 ticks after 0xFFFE, past the wrap.
/// assert_eq!(JrTicks::new(1).since(JrTicks::new(0xFFFE)), 3);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u16", into = "u16"))]
pub struct JrTicks(u16);

// Every `u16` is a value the wire carries, so the serde form reads back through
// a conversion that cannot fail; a value enters through `new` otherwise.
#[cfg(feature = "serde")]
#[allow(
    clippy::infallible_try_from,
    reason = "no `From<u16>`: a value enters through `new`; this is the serde form"
)]
impl TryFrom<u16> for JrTicks {
    type Error = core::convert::Infallible;

    fn try_from(ticks: u16) -> Result<Self, Self::Error> {
        Ok(Self(ticks))
    }
}

#[cfg(feature = "serde")]
impl From<JrTicks> for u16 {
    fn from(ticks: JrTicks) -> u16 {
        ticks.0
    }
}

impl JrTicks {
    /// The tick rate, ticks a second (§7.2.2.1).
    pub const RATE: SampleRate = SampleRate::of(31_250.0);

    /// The value of `ticks` ticks.
    #[must_use]
    pub const fn new(ticks: u16) -> Self {
        Self(ticks)
    }

    /// The 16 bits on the wire.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The value of a running count of `ticks`: its low 16 bits, as the wire
    /// wraps it.
    #[must_use]
    pub const fn masked(ticks: u64) -> Self {
        Self((ticks & 0xFFFF) as u16)
    }

    /// The signed step from `earlier` to `self`, across the wrap: positive when
    /// `self` is the later of two values less than half a wrap apart.
    #[must_use]
    pub const fn since(self, earlier: Self) -> i16 {
        self.0.wrapping_sub(earlier.0).cast_signed()
    }

    /// The time the ticks take.
    #[must_use]
    pub fn seconds(self) -> Seconds {
        Seconds::new(f64::from(self.0) / Self::RATE.get())
    }
}
