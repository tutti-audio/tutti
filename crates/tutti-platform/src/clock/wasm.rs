//! The web: nanoseconds since the first call, read from `Instant`
//! (`performance.now()`).

use std::sync::OnceLock;

use crate::Instant;

pub(super) fn monotonic_ns() -> i64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = *ORIGIN.get_or_init(Instant::now);
    // The first call reads 0; `Instant::now` never goes back, so a reading is
    // never negative.
    i64::try_from(origin.elapsed().as_nanos()).unwrap_or(i64::MAX)
}
