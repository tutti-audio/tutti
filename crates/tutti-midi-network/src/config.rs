//! [`SessionConfig`]: the timing and data-integrity choices of a Session.

use std::num::{NonZeroU8, NonZeroUsize};
use std::time::Duration;

/// The shortest a timer is set: no interval or timeout is zero, so no endpoint
/// asks its driver for the clock in a loop.
const TIMER_MIN: Duration = Duration::from_millis(1);
/// The longest a timer is set. An `Instant` plus a duration that overflows
/// panics; an hour is far past any session timer and far inside every
/// platform's range.
pub(crate) const TIMER_MAX: Duration = Duration::from_secs(60 * 60);
/// The longest the first Zero Length UMP Data Command may follow the last
/// UMP Data Command that carried data (M2-124 section 7.2.1).
const IDLE_INTERVAL_MAX: Duration = Duration::from_millis(300);

tutti_units::ranged! {
    /// A session timer: a wait, an interval or a timeout, from 1 ms to 1 h.
    ///
    /// The bounds are this crate's, not the specification's (M2-124 section 6.13 and the
    /// other timers give no bound): no timer is zero, so no endpoint asks its driver
    /// for the clock in a loop, and none is past an hour, so an `Instant` plus
    /// a timer never overflows.
    Timer(Duration), TimerOutOfRange, "within 1 ms..=1 h",
    |raw| raw.as_nanos() >= TIMER_MIN.as_nanos() && raw.as_nanos() <= TIMER_MAX.as_nanos()
}

tutti_units::ranged! {
    /// The interval of the Zero Length UMP Data Commands that declare an idle
    /// period, from 1 ms to the 300 ms M2-124 section 7.2.1 requires of the
    /// first.
    IdleInterval(Duration), IdleIntervalOutOfRange, "within 1 ms..=300 ms",
    |raw| raw.as_nanos() >= TIMER_MIN.as_nanos() && raw.as_nanos() <= IDLE_INTERVAL_MAX.as_nanos()
}

const BUFFER_DEFAULT: NonZeroUsize = NonZeroUsize::new(256).unwrap();
const REQUESTS_DEFAULT: NonZeroU8 = NonZeroU8::new(5).unwrap();

/// Forward error correction (M2-124 section 7.2.2), an optional mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fec {
    /// No earlier command is sent again.
    Off,
    /// Each new command follows this many earlier ones.
    Repeats(NonZeroU8),
}

impl Fec {
    pub(crate) const fn repeats(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Repeats(n) => n.get(),
        }
    }
}

/// Serving Retransmit Requests (M2-124 section 7.2.3), an optional mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Retransmit {
    /// Retransmit is not implemented: a request is answered with a NAK.
    Unsupported,
    /// The last this many sent UMP Data Commands are kept to be sent again.
    Buffer(NonZeroUsize),
}

impl Retransmit {
    pub(crate) const fn capacity(self) -> usize {
        match self {
            Self::Unsupported => 0,
            Self::Buffer(n) => n.get(),
        }
    }
}

/// Asking for missing commands with Retransmit Requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Requests {
    /// A gap is given up as lost without asking.
    Never,
    /// A gap is asked for up to this many times.
    UpTo(NonZeroU8),
}

impl Requests {
    pub(crate) const fn limit(self) -> u8 {
        match self {
            Self::Never => 0,
            Self::UpTo(n) => n.get(),
        }
    }
}

/// How a Session sends, repeats, probes and recovers.
///
/// [`Default`] is a conservative choice for a local network: two forward
/// error correction repeats, a 256-command retransmit buffer, a Ping after
/// two silent seconds and a Session lost after six.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionConfig {
    pub(crate) fec: Fec,
    pub(crate) retransmit: Retransmit,
    pub(crate) idle_interval: IdleInterval,
    pub(crate) reorder_wait: Timer,
    pub(crate) requests: Requests,
    pub(crate) ping_interval: Timer,
    pub(crate) liveness_timeout: Timer,
    pub(crate) repeat_interval: Timer,
    pub(crate) command_timeout: Timer,
    pub(crate) pending_timeout: Timer,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            fec: Fec::Repeats(NonZeroU8::MIN.saturating_add(1)),
            retransmit: Retransmit::Buffer(BUFFER_DEFAULT),
            idle_interval: IdleInterval::of(Duration::from_millis(10)),
            reorder_wait: Timer::of(Duration::from_millis(10)),
            requests: Requests::UpTo(REQUESTS_DEFAULT),
            ping_interval: Timer::of(Duration::from_secs(2)),
            liveness_timeout: Timer::of(Duration::from_secs(6)),
            repeat_interval: Timer::of(Duration::from_millis(500)),
            command_timeout: Timer::of(Duration::from_secs(5)),
            pending_timeout: Timer::of(Duration::from_secs(60)),
        }
    }
}

impl SessionConfig {
    /// Whether, and how far, each new UMP Data Command carries earlier ones
    /// again (forward error correction, M2-124 section 7.2.2). Two repeats
    /// is the recommended figure.
    #[must_use]
    pub const fn fec(mut self, fec: Fec) -> Self {
        self.fec = fec;
        self
    }

    /// Whether this end answers Retransmit Requests, and how many sent UMP
    /// Data Commands it keeps to do so.
    #[must_use]
    pub const fn retransmit(mut self, retransmit: Retransmit) -> Self {
        self.retransmit = retransmit;
        self
    }

    /// How long after sending data the first Zero Length UMP Data Command
    /// follows, and the interval of the next few.
    #[must_use]
    pub const fn idle_interval(mut self, interval: IdleInterval) -> Self {
        self.idle_interval = interval;
        self
    }

    /// How long a missing sequence number is waited for before a Retransmit
    /// Request asks for it (the specification suggests 10 ms), which lets
    /// an out-of-order command arrive first.
    #[must_use]
    pub const fn reorder_wait(mut self, wait: Timer) -> Self {
        self.reorder_wait = wait;
        self
    }

    /// Whether this end asks for missing commands, and how many Retransmit
    /// Requests it sends for one gap before giving it up as lost. The wait
    /// between requests doubles.
    #[must_use]
    pub const fn requests(mut self, requests: Requests) -> Self {
        self.requests = requests;
        self
    }

    /// How long a peer is silent before a Ping is sent.
    #[must_use]
    pub const fn ping_interval(mut self, interval: Timer) -> Self {
        self.ping_interval = interval;
        self
    }

    /// How long a peer is silent before the Session is ended with a Bye for
    /// timeout.
    #[must_use]
    pub const fn liveness_timeout(mut self, timeout: Timer) -> Self {
        self.liveness_timeout = timeout;
        self
    }

    /// The delay between the repeats of an Invitation, Session Reset or Bye.
    /// M2-124 section 6.2 says "A delay between 300ms and 2 seconds is
    /// recommended"; the default is 500 ms.
    #[must_use]
    pub const fn repeat_interval(mut self, interval: Timer) -> Self {
        self.repeat_interval = interval;
        self
    }

    /// How long a repeated command is repeated before the endpoint gives up
    /// and ends the Session with a Bye.
    #[must_use]
    pub const fn command_timeout(mut self, timeout: Timer) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// How long a Client waits for an Invitation Reply: Accepted after the
    /// Host said it was Pending (a user may be deciding), and how long a Host
    /// holds a challenge or a confirmation open.
    #[must_use]
    pub const fn pending_timeout(mut self, timeout: Timer) -> Self {
        self.pending_timeout = timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_fixtures::{RangeContract, check_range_contract};

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn a_timer_takes_one_millisecond_to_one_hour_and_refuses_the_rest() {
        check_range_contract(&RangeContract {
            new: Timer::new,
            get: Timer::get,
            of: Some(Timer::of),
            valid: &[MS(1), MS(250), TIMER_MAX],
            invalid: &[
                Duration::ZERO,
                MS(1) - Duration::from_nanos(1),
                TIMER_MAX + Duration::from_nanos(1),
                Duration::MAX,
            ],
        });
    }

    /// Spec: m2-124/7.2.1#2
    #[test]
    fn an_idle_interval_takes_up_to_the_300_ms_the_first_zero_length_command_must_meet() {
        check_range_contract(&RangeContract {
            new: IdleInterval::new,
            get: IdleInterval::get,
            of: Some(IdleInterval::of),
            valid: &[MS(1), MS(10), MS(300)],
            invalid: &[Duration::ZERO, MS(301), Duration::from_secs(5)],
        });
    }

    #[test]
    fn a_repeat_interval_outside_the_recommended_300_ms_to_2_s_is_accepted_and_kept() {
        for value in [MS(100), Duration::from_secs(5)] {
            let config = SessionConfig::default().repeat_interval(Timer::of(value));
            assert_eq!(config.repeat_interval.get(), value);
        }
    }

    #[test]
    fn an_option_keeps_the_value_it_is_given() {
        let config = SessionConfig::default()
            .idle_interval(IdleInterval::of(MS(300)))
            .reorder_wait(Timer::of(MS(1)))
            .ping_interval(Timer::of(MS(250)))
            .liveness_timeout(Timer::of(TIMER_MAX))
            .repeat_interval(Timer::of(MS(300)))
            .command_timeout(Timer::of(MS(7)))
            .pending_timeout(Timer::of(MS(9)));
        assert_eq!(config.idle_interval.get(), MS(300));
        assert_eq!(config.reorder_wait.get(), MS(1));
        assert_eq!(config.ping_interval.get(), MS(250));
        assert_eq!(config.liveness_timeout.get(), TIMER_MAX);
        assert_eq!(config.repeat_interval.get(), MS(300));
        assert_eq!(config.command_timeout.get(), MS(7));
        assert_eq!(config.pending_timeout.get(), MS(9));
    }

    #[test]
    fn no_timer_is_past_an_hour_so_no_instant_overflows() {
        let now = tutti_platform::Instant::now();
        assert!(now.checked_add(TIMER_MAX).is_some());
        assert!(Timer::new(TIMER_MAX + Duration::from_nanos(1)).is_none());
    }
}
