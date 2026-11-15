//! Test-only support for this crate's unit tests: the signal helpers.

mod signals;

pub(crate) use signals::{amplitude_at, lock_in};
