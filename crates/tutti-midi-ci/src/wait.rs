//! The Timeout Wait ACK (M2-101 §5.10, Table 14): how long it asks, and how a
//! transaction's deadline takes it.

use std::time::Duration;

use super::{Ack, AckStatus};
use crate::deadline::Deadline;

/// How long an Initiator waits on a peer's answer where the specification gives
/// no limit: 3 seconds. A Profile Inquiry, Set Profile On or Off, or Details
/// Inquiry waits this long for its answer; a Process Inquiry Capabilities for
/// its reply; and a MIDI Message Report for its Reply and then for its End.
///
/// The figure is this crate's choice. M2-101 gives no wait for Profile Configuration
/// or Process Inquiry; the waits it does state are kept apart:
/// [`DISCOVERY_WINDOW`](crate::DISCOVERY_WINDOW) (§5.5.5, at least 3 seconds)
/// and [`PE_TIMEOUT`](crate::PE_TIMEOUT) (M2-103 §12.2, under 3 seconds).
pub const PEER_REPLY_TIMEOUT: Duration = Duration::from_secs(3);

tutti_units::ranged! {
    /// How long a Timeout Wait asks to keep a transaction open: 0 to 127 tenths of
    /// a second, 0 to 12.7 s (M2-101 Table 14, M2-103 §12.2.1).
    #[derive(Eq, Hash)]
    TimeoutWait(u8), NotATimeoutWait, "a timeout wait (0 to 127 tenths of a second)",
    |raw| raw <= 127
}

impl TimeoutWait {
    /// The wait in tenths of a second.
    #[must_use]
    pub const fn tenths(self) -> u8 {
        self.get()
    }

    /// The wait.
    #[must_use]
    pub const fn duration(self) -> Duration {
        Duration::from_millis(self.get() as u64 * 100)
    }

    /// The wait a Timeout Wait ACK asks (Status Code `0x10`, Status Data in
    /// tenths of a second), or `None` for any other ACK.
    #[must_use]
    pub fn from_ack(ack: &Ack) -> Option<Self> {
        match ack.status {
            AckStatus::TimeoutWait(wait) => Some(wait),
            _ => None,
        }
    }

    /// Adds this wait to a transaction's deadline (M2-103 §12.2.1: the wait is
    /// added to the current timeout period).
    pub(crate) fn extend(self, wait: &mut Deadline) {
        wait.extend(self.duration());
    }
}

#[cfg(test)]
mod tests;
