//! [`DevicePart`] without the `device` feature: no device is listed. `device` is this module's
//! twin when devices are built.

use tutti_platform::{Executor, Instant};

use crate::SessionEvent;

/// No output device listing.
#[derive(Debug)]
pub(crate) struct DevicePart;

impl DevicePart {
    /// The part for a session on `executor`, which it does not use.
    pub(crate) fn new(_: &Executor) -> Self {
        Self
    }

    /// No listing answers.
    pub(crate) fn poll(&mut self, _: Instant, _: &mut Vec<SessionEvent>) {}
}
