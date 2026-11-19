//! [`DevicePart`] without the `device` feature: no device is listed. `device` is this module's
//! twin when devices are built.

use tutti_platform::Instant;

use crate::SessionEvent;

/// No output device listing.
#[derive(Debug, Default)]
pub(crate) struct DevicePart;

impl DevicePart {
    /// No listing answers.
    pub(crate) fn poll(&mut self, _: Instant, _: &mut Vec<SessionEvent>) {}
}
