//! The OS monotonic clock ([`tutti_platform::monotonic_ns`]) and the `Instant`s the
//! backends stamp with are one timebase; this relates them.

use std::time::Duration;

use tutti_platform::Instant;

/// The one pairing of the two clocks, read once per process: an `Instant` and
/// the [`tutti_platform::monotonic_ns`] reading taken with it.
fn pairing() -> (Instant, i64) {
    use std::sync::OnceLock;
    static PAIRING: OnceLock<(Instant, i64)> = OnceLock::new();
    *PAIRING.get_or_init(|| (Instant::now(), tutti_platform::monotonic_ns()))
}

/// The `Instant` of a reading of [`tutti_platform::monotonic_ns`]: the one pairing of
/// the two clocks, so the conversion is an offset. `None` where it is beyond
/// what an `Instant` holds.
pub(crate) fn os_instant(ns: i64) -> Option<Instant> {
    let (instant, base) = pairing();
    let delta = ns.saturating_sub(base);
    let span = Duration::from_nanos(delta.unsigned_abs());
    if delta >= 0 {
        instant.checked_add(span)
    } else {
        instant.checked_sub(span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A reading is the pairing's instant plus its distance from the paired
    /// reading**, after the pairing's origin, at it, and before it.
    #[test]
    fn a_reading_is_an_offset_from_the_pairing() {
        let (origin, base) = pairing();
        let second = 1_000_000_000;
        assert_eq!(os_instant(base), Some(origin));
        assert_eq!(os_instant(base + 1), Some(origin + Duration::from_nanos(1)));
        assert_eq!(
            os_instant(base + 5 * second),
            Some(origin + Duration::from_secs(5))
        );
        assert_eq!(
            os_instant(base - 3 * second),
            origin.checked_sub(Duration::from_secs(3))
        );
    }
}
