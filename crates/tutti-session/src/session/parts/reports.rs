//! [`Reports`]: the fault count the last poll saw, and the events kept for the next poll.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use tutti_engine::{OutputStream, StreamFaultKind};

use crate::SessionEvent;

/// The stream-fault count the last poll saw, which the next reports the rise
/// of, and the events kept for the next poll.
#[derive(Debug, Default)]
pub(crate) struct Reports {
    /// The count of the stream's faults the last poll saw.
    faults_seen: u64,
    /// Events the session kept for its next poll: what a clock switch reported
    /// on the clock it left.
    carried: Vec<SessionEvent>,
}

impl Reports {
    /// Reports the newest fault `stream` raised since the last poll, if the
    /// clock has a stream. Whether the stream was lost: its device left.
    ///
    /// The message is *taken* from the record: a fault is reported once, by
    /// its own kind, and a later fault that only bumps the count (an overrun
    /// the record keeps no message for beside a latched panic) is not read as
    /// the old message again.
    pub(crate) fn faults(
        &mut self,
        stream: Option<&OutputStream>,
        out: &mut Vec<SessionEvent>,
    ) -> bool {
        let Some(stream) = stream else {
            return false;
        };
        let faults = stream.faults();
        let count = faults.count();
        if count == self.faults_seen {
            return false;
        }
        // A take that lags (a record mid-write) retries on the next poll; an
        // empty one (a counted fault whose text was lost) is seen.
        let Ok(taken) = faults.take_last() else {
            return false;
        };
        self.faults_seen = count;
        let Some(fault) = taken else {
            return false;
        };
        let lost = fault.kind == StreamFaultKind::Disconnected;
        out.push(SessionEvent::Fault(fault));
        lost
    }

    /// The fault record restarted (a new stream cleared it): its count is `count`.
    pub(crate) fn faults_restarted(&mut self, count: u64) {
        self.faults_seen = count;
    }

    /// Keeps `events` for the next poll.
    pub(crate) fn carry(&mut self, events: &mut Vec<SessionEvent>) {
        self.carried.append(events);
    }

    /// Moves the kept events onto `out`.
    pub(crate) fn drain_carried(&mut self, out: &mut Vec<SessionEvent>) {
        out.append(&mut self.carried);
    }
}
