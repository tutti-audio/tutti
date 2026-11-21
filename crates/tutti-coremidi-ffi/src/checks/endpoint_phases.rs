//! The phases of making a UMP Endpoint that declares Function Blocks: register the
//! blocks, enable the endpoint, read the blocks back.
//!
//! CoreMIDI has been seen to refuse the registration and the enable with
//! `kMIDINotPermitted` while other processes create endpoints, and to accept the
//! same call a few milliseconds later. [`Phases`] is the state of one creation:
//! each poll runs the attempt of the current phase when the phase's [`Backoff`]
//! says one is due, and a phase that succeeds starts the next with a fresh bound.

use super::backoff::{Backoff, Failure, poll_attempt};
use super::os_status::OsStatus;
use std::task::Poll;
use tutti_platform::Instant;

/// A phase of the creation, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    /// `registerFunctionBlocks:markAsStatic:error:`, retried.
    Register,
    /// `setEnabled:error:`, retried.
    Enable,
    /// Reading the registered blocks back, once.
    ReadBack,
}

/// How `registerFunctionBlocks:markAsStatic:error:` failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegisterFailure<E> {
    /// CoreMIDI returned an `NSError`.
    Refused(E),
    /// It returned `NO` with no `NSError`.
    Unreported,
}

/// The outcome of a registration that reported a failure.
///
/// A refusal is its error. A `NO` without an error is accepted when the endpoint
/// holds `count` blocks, which `held` reads, and is `unreported` otherwise: a
/// count that differs means the blocks were not registered.
pub(crate) fn settle_register<E>(
    result: Result<(), RegisterFailure<E>>,
    count: usize,
    held: impl FnOnce() -> usize,
    unreported: E,
) -> Result<(), E> {
    match result {
        Ok(()) => Ok(()),
        Err(RegisterFailure::Refused(error)) => Err(error),
        Err(RegisterFailure::Unreported) if held() == count => Ok(()),
        Err(RegisterFailure::Unreported) => Err(unreported),
    }
}

/// One endpoint creation in progress.
#[derive(Debug)]
pub(crate) struct Phases {
    stage: Stage,
    backoff: Backoff,
}

impl Phases {
    /// A creation whose registration is first due at `now`.
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            stage: Stage::Register,
            backoff: Backoff::endpoint(now),
        }
    }

    /// The phase the next poll runs.
    pub(crate) fn stage(&self) -> Stage {
        self.stage
    }

    /// When the next poll can make progress.
    pub(crate) fn deadline(&self) -> Instant {
        self.backoff.deadline()
    }

    /// One poll at `now`: runs `attempt` for the current phase when it is due, and
    /// for each phase that succeeds the next, at the same `now`. `Ready(Ok)` once
    /// the read-back has succeeded; an error is the phase it came from and why.
    pub(crate) fn poll<E>(
        &mut self,
        now: Instant,
        mut attempt: impl FnMut(Stage) -> Result<(), E>,
        status: impl Fn(&E) -> Option<OsStatus>,
    ) -> Poll<Result<(), (Stage, Failure<E>)>> {
        loop {
            let stage = self.stage;
            if stage == Stage::ReadBack {
                return Poll::Ready(attempt(stage).map_err(|e| (stage, Failure::Failed(e))));
            }
            match poll_attempt(&mut self.backoff, now, || attempt(stage), &status) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(failure)) => return Poll::Ready(Err((stage, failure))),
                Poll::Ready(Ok(())) => {
                    self.backoff = Backoff::endpoint(now);
                    self.stage = match stage {
                        Stage::Register => Stage::Enable,
                        _ => Stage::ReadBack,
                    };
                }
            }
        }
    }
}

/// An object that is taken down when dropped unless it was released first.
///
/// An endpoint that is enabled must be disabled before it is released, so an
/// unfinished creation that is dropped, or fails, takes it down.
pub(crate) struct TakeDown<T> {
    held: Option<T>,
    dispose: fn(T),
}

impl<T> TakeDown<T> {
    /// Holds `held`; `dispose` takes it down.
    pub(crate) fn new(held: T, dispose: fn(T)) -> Self {
        Self {
            held: Some(held),
            dispose,
        }
    }

    /// The held object, until it is taken down or released.
    pub(crate) fn get(&self) -> Option<&T> {
        self.held.as_ref()
    }

    /// Takes the object down now.
    pub(crate) fn dispose(&mut self) {
        if let Some(held) = self.held.take() {
            (self.dispose)(held);
        }
    }

    /// The object, no longer taken down on drop: the creation finished.
    pub(crate) fn release(&mut self) -> Option<T> {
        self.held.take()
    }
}

impl<T> Drop for TakeDown<T> {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    const REFUSED: OsStatus = OsStatus::NOT_PERMITTED;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn status(status: &OsStatus) -> Option<OsStatus> {
        Some(*status)
    }

    #[test]
    fn register_runs_first_then_enable_then_the_read_back_in_one_poll_when_none_is_refused() {
        let t0 = Instant::now();
        let mut phases = Phases::new(t0);
        let seen = RefCell::new(Vec::new());
        let polled = phases.poll(
            t0,
            |stage| {
                seen.borrow_mut().push(stage);
                Ok(())
            },
            status,
        );
        assert_eq!(polled, Poll::Ready(Ok(())));
        assert_eq!(
            *seen.borrow(),
            [Stage::Register, Stage::Enable, Stage::ReadBack]
        );
    }

    #[test]
    fn a_refused_registration_waits_and_the_enable_does_not_run_until_it_passes() {
        let t0 = Instant::now();
        let mut phases = Phases::new(t0);
        let seen = RefCell::new(Vec::new());
        let refuse_register = Cell::new(true);
        let mut attempt = |stage| {
            seen.borrow_mut().push(stage);
            if stage == Stage::Register && refuse_register.replace(false) {
                Err(REFUSED)
            } else {
                Ok(())
            }
        };
        assert_eq!(phases.poll(t0, &mut attempt, status), Poll::Pending);
        assert_eq!(phases.stage(), Stage::Register);
        assert_eq!(phases.deadline(), t0 + ms(10));
        assert_eq!(phases.poll(t0 + ms(9), &mut attempt, status), Poll::Pending);
        assert_eq!(*seen.borrow(), [Stage::Register]);
        assert_eq!(
            phases.poll(t0 + ms(10), &mut attempt, status),
            Poll::Ready(Ok(()))
        );
        assert_eq!(
            *seen.borrow(),
            [
                Stage::Register,
                Stage::Register,
                Stage::Enable,
                Stage::ReadBack
            ]
        );
    }

    #[test]
    fn each_phase_has_a_fresh_bound() {
        let t0 = Instant::now();
        let mut phases = Phases::new(t0);
        // The registration succeeds after 1.5 s of refusals; the enable is then
        // refused for another 1.5 s, which would pass the first phase's bound.
        let mut now = t0;
        let mut enabled_from = None;
        let outcome = loop {
            let polled = phases.poll(
                now,
                |stage| match stage {
                    Stage::Register if now < t0 + ms(1500) => Err(REFUSED),
                    Stage::Enable => {
                        let from = *enabled_from.get_or_insert(now);
                        if now < from + ms(1500) {
                            Err(REFUSED)
                        } else {
                            Ok(())
                        }
                    }
                    _ => Ok(()),
                },
                status,
            );
            if let Poll::Ready(done) = polled {
                break done;
            }
            now = phases.deadline();
        };
        assert_eq!(outcome, Ok(()));
        assert!(
            now > t0 + Duration::from_secs(2),
            "the second phase ran past the first bound"
        );
    }

    #[test]
    fn a_phase_that_keeps_being_refused_times_out_naming_the_phase() {
        let t0 = Instant::now();
        let mut phases = Phases::new(t0);
        let mut attempt = |stage| {
            if stage == Stage::Register {
                Ok(())
            } else {
                Err(REFUSED)
            }
        };
        assert_eq!(phases.poll(t0, &mut attempt, status), Poll::Pending);
        assert_eq!(phases.stage(), Stage::Enable);
        assert_eq!(
            phases.poll(t0 + Duration::from_secs(2), &mut attempt, status),
            Poll::Ready(Err((Stage::Enable, Failure::TimedOut { last: REFUSED })))
        );
    }

    #[test]
    fn a_status_that_is_not_retried_fails_the_phase_at_once() {
        let t0 = Instant::now();
        let mut phases = Phases::new(t0);
        assert_eq!(
            phases.poll(t0, |_| Err(OsStatus::new(-10843)), status),
            Poll::Ready(Err((
                Stage::Register,
                Failure::Failed(OsStatus::new(-10843))
            )))
        );
    }

    #[test]
    fn an_unreported_registration_is_accepted_only_when_the_held_count_matches() {
        let unreported = || Err(RegisterFailure::<&str>::Unreported);
        assert_eq!(settle_register(unreported(), 2, || 2, "none"), Ok(()));
        assert_eq!(settle_register(unreported(), 2, || 1, "none"), Err("none"));
        assert_eq!(settle_register(unreported(), 2, || 3, "none"), Err("none"));
        assert_eq!(
            settle_register(Err(RegisterFailure::Refused("no")), 2, || 2, "none"),
            Err("no"),
            "a refusal is never accepted"
        );
    }

    #[test]
    fn an_unfinished_object_is_taken_down_on_drop_and_a_released_one_is_not() {
        thread_local!(static DOWN: Cell<u32> = const { Cell::new(0) });
        fn down(_: u8) {
            DOWN.with(|d| d.set(d.get() + 1));
        }
        drop(TakeDown::new(1, down));
        assert_eq!(DOWN.with(Cell::get), 1);
        let mut finished = TakeDown::new(2, down);
        assert_eq!(finished.release(), Some(2));
        drop(finished);
        assert_eq!(DOWN.with(Cell::get), 1);
        let mut failed = TakeDown::new(3, down);
        failed.dispose();
        drop(failed);
        assert_eq!(
            DOWN.with(Cell::get),
            2,
            "a disposed object is taken down once"
        );
    }
}
