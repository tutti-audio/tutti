//! The off-main deadline as a pure rule.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// What an off-main call makes of its wait at an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Wait {
    /// The main thread has until the deadline: block for at most this long.
    For(Duration),
    /// The deadline has come: answer "unavailable".
    Expired,
}

/// How long an off-main call waits for the main thread to run its work: from
/// the instant the call began to `limit` after it.
///
/// It reads no clock. The waiting call passes the instant it sampled, so the
/// rule is a function of the instants it is given and a test drives it with
/// constructed ones.
#[derive(Clone, Copy, Debug)]
pub(super) struct OffMainWait {
    due: Instant,
}

impl OffMainWait {
    /// A wait that began at `began` and lasts `limit`.
    pub(super) fn new(limit: Duration, began: Instant) -> Self {
        Self { due: began + limit }
    }

    /// The verdict at `now`: how much of the wait is left, or [`Wait::Expired`]
    /// once `now` is at or past the deadline.
    pub(super) fn at(&self, now: Instant) -> Wait {
        match self.due.checked_duration_since(now) {
            Some(left) if !left.is_zero() => Wait::For(left),
            _ => Wait::Expired,
        }
    }
}

/// The answer on `answer`, or `None` once `wait` has expired or the sender is
/// gone. `now` is the clock: each pass asks it, so the rule runs on whatever
/// instants it is given.
pub(super) fn await_answer<R>(
    answer: &Receiver<R>,
    wait: OffMainWait,
    mut now: impl FnMut() -> Instant,
) -> Option<R> {
    loop {
        let Wait::For(left) = wait.at(now()) else {
            return None;
        };
        match answer.recv_timeout(left) {
            Ok(answer) => return Some(answer),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// **The wait has what is left of the limit until the limit, and not a
    /// moment after.**
    #[test]
    fn the_wait_is_what_is_left_of_the_limit_until_it_is_spent() {
        let began = Instant::now();
        let wait = OffMainWait::new(ms(2000), began);
        assert_eq!(wait.at(began), Wait::For(ms(2000)));
        assert_eq!(wait.at(began + ms(500)), Wait::For(ms(1500)));
        assert_eq!(wait.at(began + ms(1999)), Wait::For(ms(1)));
        assert_eq!(wait.at(began + ms(2000)), Wait::Expired);
        assert_eq!(wait.at(began + ms(9000)), Wait::Expired);
    }

    /// **The deadline is fixed by the start**: an instant before it has more than
    /// the limit left, and a zero limit expires at the start.
    #[test]
    fn the_deadline_is_the_start_plus_the_limit() {
        let began = Instant::now() + ms(1000);
        let wait = OffMainWait::new(ms(100), began);
        assert_eq!(wait.at(began - ms(1000)), Wait::For(ms(1100)));
        assert_eq!(
            OffMainWait::new(Duration::ZERO, began).at(began),
            Wait::Expired
        );
    }

    /// **A wait whose deadline has come answers "unavailable" with the work
    /// still unanswered**, as a stalled main thread's caller does: the clock
    /// reads the deadline and nothing is waited for.
    #[test]
    fn a_wait_past_its_deadline_gives_up_on_an_unanswered_call() {
        let (_sender, answer) = std::sync::mpsc::sync_channel::<u8>(1);
        let began = Instant::now();
        let wait = OffMainWait::new(ms(2000), began);
        assert_eq!(await_answer(&answer, wait, || began + ms(2000)), None);
    }

    /// **An answer that is there before the deadline is the call's answer.**
    #[test]
    fn an_answer_before_the_deadline_is_returned() {
        let (sender, answer) = std::sync::mpsc::sync_channel(1);
        sender.send(7_u8).expect("room for one answer");
        let began = Instant::now();
        let wait = OffMainWait::new(ms(2000), began);
        assert_eq!(await_answer(&answer, wait, || began + ms(1)), Some(7));
    }

    /// **A work item dropped unrun ends the wait at once**, before the
    /// deadline: the sender's end is gone, so no answer will come.
    #[test]
    fn a_dropped_sender_ends_the_wait() {
        let (sender, answer) = std::sync::mpsc::sync_channel::<u8>(1);
        drop(sender);
        let began = Instant::now();
        let wait = OffMainWait::new(ms(2000), began);
        assert_eq!(await_answer(&answer, wait, || began), None);
    }
}
