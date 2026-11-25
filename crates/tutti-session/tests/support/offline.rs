//! The offline sessions several area binaries build the same way.

#![allow(dead_code, reason = "each area binary uses the helpers it needs")]

use tutti_session::Session;
use tutti_units::SampleRate;

/// An offline session at the defaults.
pub(crate) fn offline() -> Session {
    Session::builder().build().expect("builds")
}

/// An offline session at 48 kHz.
pub(crate) fn offline_48k() -> Session {
    Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .build()
        .expect("builds")
}
