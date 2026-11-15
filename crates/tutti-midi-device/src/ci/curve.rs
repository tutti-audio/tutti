//! The amplitude curves of the drum map and drawbar organ Profiles, as
//! functions from a 32-bit controller value to a gain.

use tutti_types::{Amplitude, Db};
use tutti_ump::ControlValue;

/// The value as a fraction of full scale, `0.0..=1.0`.
fn fraction(value: ControlValue) -> f64 {
    f64::from(value.get()) / f64::from(u32::MAX)
}

/// The gain a volume controller value sets: `40 * log10(value / 0xFFFFFFFF)`
/// dB, so the amplitude goes as the square of the value, `0 dB` at
/// `0xFFFFFFFF` and `-inf` at `0` (M2-125 §5.1 for CC#7 and §6.1.1 for
/// RPNC#7, Table 6 and Table 7; M2-121 §5.1.3 for CC#7 and CC#11, Table 8 and
/// Table 9).
///
/// The tables of those sections print `-4.9`, `-11.9`, `-23.9` and `-36.0` dB
/// at `0xC1041041`, `0x80000000`, `0x40000000` and `0x20000000`; the formula
/// they state gives `-4.86`, `-12.04`, `-24.08` and `-36.12`. This follows the
/// formula.
#[must_use]
pub fn volume_gain(value: ControlValue) -> Db {
    // `log10(0)` is `-inf`, which `Db` carries as silence.
    Db::new((40.0 * fraction(value).log10()) as f32)
}

/// The send level a reverb send value sets: linear in amplitude, `1.0`
/// (100%) at `0xFFFFFFFF` (M2-125 §5.2 for CC#91 and §6.1.8 for RPNC#91).
#[must_use]
pub fn send_level(value: ControlValue) -> Amplitude {
    Amplitude::new(fraction(value) as f32)
}

#[cfg(test)]
mod tests;
