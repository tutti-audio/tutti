//! [`ActiveSensingSender`]: the transmitter's side of MIDI 1.0 Detailed
//! Specification 4.2.1 (p. 32), as a pure scheduler.
//!
//! A transmitter that sends Active Sensing (`0xFE`) sends one at least every
//! 300 ms in which it sent nothing else, and a receiver that has seen one
//! turns its voices off when 330 ms pass with no message
//! (`ActiveSensing`). The spec recommends the
//! transmitter send within 270 ms, a margin for a scheduler that wakes late,
//! and that is the [`ACTIVE_SENSING_INTERVAL`] here.
//!
//! The sender decides when and gives the message out; it reads no clock,
//! sleeps nowhere and does no I/O. The caller tells it each message it sent
//! ([`sent`](ActiveSensingSender::sent)), asks what is due at a time
//! ([`poll`](ActiveSensingSender::poll)) and when to ask next
//! ([`deadline`](ActiveSensingSender::deadline)). Once a sender exists it keeps
//! sending for as long as it is polled: a transmitter that starts must go on
//! (p. 32), so there is no stopping it but dropping it.

use std::time::{Duration, Instant};

use tutti_ump::{Message, MidiGroup, System, Ump};

/// How long the output may be silent before an Active Sensing is due: 270 ms,
/// the spec's recommended send time, inside its 300 ms limit with the margin
/// for a wake that comes late.
pub const ACTIVE_SENSING_INTERVAL: Duration = Duration::from_millis(270);

/// The Active Sensing schedule of one output on one Group.
///
/// ```
/// use std::time::{Duration, Instant};
/// use tutti_midi_runtime::{ACTIVE_SENSING_INTERVAL, ActiveSensingSender};
/// use tutti_ump::{Message, MidiGroup, System};
///
/// let start = Instant::now();
/// let mut sender = ActiveSensingSender::new(MidiGroup::FIRST, start);
/// assert!(sender.poll(start + Duration::from_millis(100)).is_none());
/// let due = sender.poll(start + ACTIVE_SENSING_INTERVAL).unwrap();
/// assert_eq!(due.decode(), Ok(Message::System(MidiGroup::FIRST, System::ActiveSensing)));
/// ```
#[derive(Clone, Copy, Debug)]
pub struct ActiveSensingSender {
    group: MidiGroup,
    interval: Duration,
    /// When the output last sent anything, this sender's own Active Sensing
    /// included.
    last: Instant,
}

impl ActiveSensingSender {
    /// A sender on `group` for an output that has been silent since `now`, at
    /// [`ACTIVE_SENSING_INTERVAL`].
    #[must_use]
    pub const fn new(group: MidiGroup, now: Instant) -> Self {
        Self::with_interval(group, ACTIVE_SENSING_INTERVAL, now)
    }

    /// A sender that sends after `interval` of silence. An interval over the
    /// spec's 300 ms is not a transmitter's (p. 32); the caller picks it.
    #[must_use]
    pub const fn with_interval(group: MidiGroup, interval: Duration, now: Instant) -> Self {
        Self {
            group,
            interval,
            last: now,
        }
    }

    /// The Group the Active Sensing goes out on.
    #[must_use]
    pub const fn group(&self) -> MidiGroup {
        self.group
    }

    /// The output sent a message at `now`: it is not silent, so the next
    /// Active Sensing is due an interval after it. A time before the last
    /// message is taken as the last.
    pub fn sent(&mut self, now: Instant) {
        self.last = self.last.max(now);
    }

    /// When [`poll`](Self::poll) next has a message: an interval after the
    /// output last sent anything.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.last + self.interval
    }

    /// The Active Sensing due at `now`, if the output has been silent for an
    /// interval; sending it is a message sent, so the next is an interval
    /// later. A caller that wakes late is sent one, never a burst for the
    /// intervals it missed.
    pub fn poll(&mut self, now: Instant) -> Option<Ump> {
        (now >= self.deadline()).then(|| {
            self.sent(now);
            Ump::from(Message::System(self.group, System::ActiveSensing))
        })
    }
}

#[cfg(test)]
mod tests;
