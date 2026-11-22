//! The OS monotonic clock, in nanoseconds: the clock a hosted plugin compares
//! its transport's system time against (VST3 `systemTime`, VST2
//! `nanoSeconds`, Core Audio's host time).
//!
//! - Linux and the other Unixes: `CLOCK_MONOTONIC` (a vDSO read on Linux).
//! - macOS: `CLOCK_UPTIME_RAW`, which is `mach_absolute_time` in nanoseconds:
//!   Core Audio's host time (`AudioTimeStamp::mHostTime`, read through
//!   `AudioConvertHostTimeToNanos`) is that clock.
//! - Windows: `QueryPerformanceCounter`, scaled to nanoseconds by its
//!   frequency (fixed at boot).
//! - The web (`wasm32`): nanoseconds since the first call, from [`Instant`]
//!   (`performance.now()`). No plugin runs and no second process exists
//!   there, so an origin at the first call is enough.
//!
//! The OS's epoch, not one this process started: a plugin in another process
//! (a plugin server) reads the same timebase without being handed an origin,
//! which is why `std::time::Instant` (an opaque, process-local value) does not
//! do.

use std::time::Duration;

use crate::Instant;

/// The OS monotonic clock now, in nanoseconds since the OS's own epoch (see
/// the module docs). Audio-thread safe: no lock, no allocation and no syscall
/// beyond the vDSO's or QPC's. 2^63 ns is 292 years of uptime, so the
/// saturation is unreachable.
///
/// A bare `i64` of nanoseconds, not `Seconds`: `Seconds` is an `f64`, and
/// every reader hands this exact integer count on (VST3's `systemTime`, the
/// plugin wire), where a float would round it.
#[must_use]
pub fn monotonic_ns() -> i64 {
    os::monotonic_ns()
}

/// The one pairing of the two clocks, read once per process: an [`Instant`] and
/// the [`monotonic_ns`] reading taken with it.
fn pairing() -> (Instant, i64) {
    use std::sync::OnceLock;
    static PAIRING: OnceLock<(Instant, i64)> = OnceLock::new();
    *PAIRING.get_or_init(|| (Instant::now(), monotonic_ns()))
}

/// The [`Instant`] of a [`monotonic_ns`] reading: [`Instant`] and the OS
/// monotonic clock are one timebase on a native target, related by the one
/// pairing of the two, so the conversion is an offset, exact to the gap between
/// the pairing's two reads. `None` where the result
/// is beyond what an `Instant` holds.
#[must_use]
pub fn instant_of_monotonic_ns(ns: i64) -> Option<Instant> {
    let (instant, base) = pairing();
    let delta = ns.saturating_sub(base);
    let span = Duration::from_nanos(delta.unsigned_abs());
    if delta >= 0 {
        instant.checked_add(span)
    } else {
        instant.checked_sub(span)
    }
}

/// The [`monotonic_ns`] reading of an [`Instant`]: the inverse of
/// [`instant_of_monotonic_ns`], through the same pairing, so it is an offset,
/// exact to the gap between the pairing's two reads. `None` where the result is
/// beyond what an `i64` of nanoseconds holds.
#[must_use]
pub fn monotonic_ns_of(instant: Instant) -> Option<i64> {
    let (origin, base) = pairing();
    match instant.checked_duration_since(origin) {
        Some(after) => base.checked_add(i64::try_from(after.as_nanos()).ok()?),
        None => {
            let before = origin.duration_since(instant);
            base.checked_sub(i64::try_from(before.as_nanos()).ok()?)
        }
    }
}

#[cfg_attr(unix, path = "clock/unix.rs")]
#[cfg_attr(windows, path = "clock/windows.rs")]
#[cfg_attr(target_arch = "wasm32", path = "clock/wasm.rs")]
mod os;

#[cfg(test)]
mod tests {
    use super::*;

    /// **A reading is the pairing's instant plus its distance from the paired
    /// reading**, after the pairing's origin, at it, and before it, and an
    /// instant's reading is its inverse at each, and `None` past an `i64`.
    #[test]
    fn a_reading_is_an_offset_from_the_pairing() {
        let (origin, base) = pairing();
        let second = 1_000_000_000;
        assert_eq!(instant_of_monotonic_ns(base), Some(origin));
        assert_eq!(
            instant_of_monotonic_ns(base + 1),
            Some(origin + Duration::from_nanos(1))
        );
        assert_eq!(
            instant_of_monotonic_ns(base + 5 * second),
            Some(origin + Duration::from_secs(5))
        );
        assert_eq!(
            instant_of_monotonic_ns(base - 3 * second),
            origin.checked_sub(Duration::from_secs(3))
        );
        for ns in [base - 3 * second, base, base + 1, base + 5 * second] {
            let at = instant_of_monotonic_ns(ns).expect("near the origin");
            assert_eq!(monotonic_ns_of(at), Some(ns));
        }
        assert_eq!(
            monotonic_ns_of(origin + Duration::from_secs(u64::MAX / 1_000_000_000 + 1)),
            None,
            "centuries ahead is past an i64 of nanoseconds"
        );
    }

    /// Two readings never go back.
    #[test]
    fn it_never_goes_back() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(a > 0 && b >= a, "{a} then {b}");
    }
}
