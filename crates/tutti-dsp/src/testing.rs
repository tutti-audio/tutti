//! Test-only support for this crate's unit tests: the signal helpers and the f64 narrowing check.

mod signals;

pub(crate) use signals::{amplitude_at, assert_close_f64, lock_in, noise, wide_signal};
