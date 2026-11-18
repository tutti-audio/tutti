//! Test-only support for this crate's unit tests: the signal helpers.

mod signals;

pub(crate) use signals::{
    amplitude_at, butterworth_all_pass, direct_form, lock_in, noise, sine_fit, steady_deviation,
};
