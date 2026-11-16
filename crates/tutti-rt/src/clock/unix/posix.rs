//! Linux and the other Unixes: `CLOCK_MONOTONIC`.

pub(super) const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
