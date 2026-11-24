//! When a device's callbacks are not the size the graph was prepared for.
//!
//! A device is opened asking for a callback size and a backend may deliver
//! another. A hosted plugin's chunk follows the size the graph was prepared
//! for, so callbacks of another size leave it mostly silent.
//! [`CallbackSizeRule`] is the decision as a value: it is fed one reading per
//! session update (the span of every callback since the previous one) and the
//! size the graph is prepared for, and answers when the delivered size has
//! settled at one other value, or never settles. It has no clock and does no
//! I/O.

use tutti_engine::{CallbackSpan, OutputQuantum};

/// How many readings in a row, each of callbacks all at one size other than
/// the prepared one, make that size the delivered one.
///
/// One odd callback (the short buffer a backend hands at a stream's start, a
/// catch-up) makes its reading varying and moves nothing; three steady
/// readings in a row, each at least one session update apart, is a backend
/// that delivers that size. A larger count only delays a restart that is
/// needed.
pub(crate) const SETTLE_READINGS: u32 = 3;

/// How many readings that are not part of a settled size may pass, with no
/// size reaching [`SETTLE_READINGS`], before the callbacks are called varying.
pub(crate) const VARY_READINGS: u32 = 32;

/// How many sizes are asked for between two readings that agree with the
/// prepared size: the second covers a backend that rounds the first measured
/// size once more.
pub(crate) const MAX_ASKS: u32 = 2;

/// What [`CallbackSizeRule::observe`] concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallbackSize {
    /// The callbacks settled at this size, which is not the prepared one.
    /// Said once for each size the stream settles at.
    Settled(OutputQuantum),
    /// The callbacks keep changing size, or settle at more sizes than are
    /// asked for. Said once; nothing is asked after it until a reading
    /// agrees with the prepared size.
    Varies,
}

/// The run of steady readings at one mismatching size.
#[derive(Clone, Copy, Debug)]
struct Run {
    size: OutputQuantum,
    readings: u32,
}

/// The rule: state is what the readings so far showed, and
/// [`observe`](Self::observe) is the only step. What it asked for survives a
/// restart (a reading of no callbacks); only a reading that agrees with the
/// prepared size forgets it.
#[derive(Debug, Default)]
pub(crate) struct CallbackSizeRule {
    run: Option<Run>,
    /// Readings that were not part of a settled size, since one that agreed
    /// or one that settled.
    unsettled: u32,
    asks: u32,
    varied: bool,
}

impl CallbackSizeRule {
    /// Takes one reading (`None`: no callback ran since the previous one)
    /// against the size the graph is `prepared` for, and says what it
    /// concludes, if anything.
    ///
    /// A reading of no callbacks is no evidence: it changes nothing. A
    /// reading of callbacks all at the prepared size forgets everything. A
    /// reading whose callbacks differ in size, or a steady one at another
    /// size than the run before it, ends the run.
    pub(crate) fn observe(
        &mut self,
        reading: Option<CallbackSpan>,
        prepared: OutputQuantum,
    ) -> Option<CallbackSize> {
        let span = reading?;
        let Some(size) = span.steady_size() else {
            self.run = None;
            return self.unsettled_reading();
        };
        if size == prepared {
            *self = Self::default();
            return None;
        }
        let readings = match self.run {
            Some(run) if run.size == size => run.readings.saturating_add(1),
            _ => 1,
        };
        self.run = Some(Run { size, readings });
        if readings < SETTLE_READINGS {
            return self.unsettled_reading();
        }
        // The reading that completes the run asks; later ones of the same
        // run have been answered.
        if readings > SETTLE_READINGS || self.varied {
            return None;
        }
        self.unsettled = 0;
        self.asks += 1;
        if self.asks > MAX_ASKS {
            self.varied = true;
            return Some(CallbackSize::Varies);
        }
        Some(CallbackSize::Settled(size))
    }

    /// One more reading that is not a settled size.
    fn unsettled_reading(&mut self) -> Option<CallbackSize> {
        self.unsettled = self.unsettled.saturating_add(1);
        if self.unsettled >= VARY_READINGS && !self.varied {
            self.varied = true;
            return Some(CallbackSize::Varies);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREPARED: usize = 512;

    fn q(frames: usize) -> OutputQuantum {
        OutputQuantum::of(frames)
    }

    fn steady(frames: usize) -> Option<CallbackSpan> {
        Some(CallbackSpan::steady(q(frames)))
    }

    fn span(smallest: usize, largest: usize) -> Option<CallbackSpan> {
        CallbackSpan::new(q(smallest), q(largest))
    }

    fn feed(
        rule: &mut CallbackSizeRule,
        prepared: usize,
        readings: &[Option<CallbackSpan>],
    ) -> Vec<CallbackSize> {
        readings
            .iter()
            .filter_map(|r| rule.observe(*r, q(prepared)))
            .collect()
    }

    fn settled(frames: usize) -> CallbackSize {
        CallbackSize::Settled(q(frames))
    }

    /// **A steady size that holds for `SETTLE_READINGS` readings is answered
    /// once**, at the reading that completes the run.
    #[test]
    fn a_steady_size_is_answered_once_at_the_reading_that_completes_the_run() {
        let mut rule = CallbackSizeRule::default();
        let n = SETTLE_READINGS as usize;
        assert!(feed(&mut rule, PREPARED, &vec![steady(512); 10]).is_empty());
        assert!(feed(&mut rule, PREPARED, &vec![steady(480); n - 1]).is_empty());
        assert_eq!(feed(&mut rule, PREPARED, &[steady(480)]), [settled(480)]);
        assert!(feed(&mut rule, PREPARED, &vec![steady(480); 10]).is_empty());
    }

    /// **A device that alternates between two sizes never settles**, however
    /// its readings fall: a reading that spans both is varying, and one that
    /// catches a single size does not repeat.
    #[test]
    fn an_alternating_device_never_yields_settled() {
        let mut rule = CallbackSizeRule::default();
        let spanning = vec![span(471, 472); 200];
        let seen = feed(&mut rule, PREPARED, &spanning);
        assert_eq!(seen, [CallbackSize::Varies], "varying, once");
        let mut rule = CallbackSizeRule::default();
        let single: Vec<_> = (0..200)
            .map(|i| steady(if i % 2 == 0 { 471 } else { 472 }))
            .collect();
        assert!(
            feed(&mut rule, PREPARED, &single)
                .iter()
                .all(|a| *a == CallbackSize::Varies)
        );
    }

    /// **A reading of no callbacks neither advances nor resets a run.**
    #[test]
    fn a_reading_with_no_callbacks_is_no_evidence() {
        let mut rule = CallbackSizeRule::default();
        let readings = [steady(480), None, steady(480), None, None, steady(480)];
        assert_eq!(feed(&mut rule, PREPARED, &readings), [settled(480)]);
    }

    /// **What was asked survives a restart, and at most two sizes are asked
    /// between agreeing readings**: ask, a restart (no readings), a different
    /// steady size is a second ask, a third is varying and nothing is asked
    /// after it.
    #[test]
    fn the_asks_survive_a_restart_and_a_third_size_is_varying() {
        let mut rule = CallbackSizeRule::default();
        let n = SETTLE_READINGS as usize;
        assert_eq!(
            feed(&mut rule, PREPARED, &vec![steady(480); n]),
            [settled(480)]
        );
        // The host restarts onto 480: no readings, then 441.
        assert!(feed(&mut rule, 480, &[None, None]).is_empty());
        assert_eq!(feed(&mut rule, 480, &vec![steady(441); n]), [settled(441)]);
        assert!(feed(&mut rule, 441, &[None]).is_empty());
        assert_eq!(
            feed(&mut rule, 441, &vec![steady(400); n]),
            [CallbackSize::Varies]
        );
        assert!(
            feed(&mut rule, 441, &vec![steady(256); n * 3]).is_empty(),
            "nothing more is asked until a reading agrees"
        );
    }

    /// **A reading that agrees with the prepared size forgets everything**,
    /// whatever came before it.
    #[test]
    fn an_agreeing_reading_resets_after_an_ask_and_after_varying() {
        let n = SETTLE_READINGS as usize;
        let mut rule = CallbackSizeRule::default();
        feed(&mut rule, PREPARED, &vec![steady(480); n]);
        feed(&mut rule, 480, &vec![steady(441); n]);
        feed(&mut rule, 441, &vec![steady(400); n]);
        assert!(feed(&mut rule, 400, &[steady(400)]).is_empty());
        assert_eq!(
            feed(&mut rule, 400, &vec![steady(300); n]),
            [settled(300)],
            "asked again after agreeing"
        );
        let mut rule = CallbackSizeRule::default();
        let varying = vec![span(400, 480); VARY_READINGS as usize];
        assert_eq!(feed(&mut rule, PREPARED, &varying), [CallbackSize::Varies]);
        feed(&mut rule, PREPARED, &[steady(512)]);
        assert_eq!(
            feed(&mut rule, PREPARED, &vec![steady(256); n]),
            [settled(256)]
        );
    }

    /// **A size that changes mid-stream on a running device is asked once**:
    /// 512 agreed for a while, then steady 256.
    #[test]
    fn a_size_that_changes_on_a_running_device_is_asked_once() {
        let mut rule = CallbackSizeRule::default();
        assert!(feed(&mut rule, PREPARED, &vec![steady(512); 50]).is_empty());
        assert_eq!(
            feed(&mut rule, PREPARED, &vec![steady(256); 50]),
            [settled(256)]
        );
    }

    /// **Steady readings at a size the host has been asked for are not a
    /// varying device**: a host that ignores the request does not turn it into
    /// one.
    #[test]
    fn an_ignored_ask_is_not_reported_varying() {
        let mut rule = CallbackSizeRule::default();
        let seen = feed(&mut rule, PREPARED, &vec![steady(480); 500]);
        assert_eq!(seen, [settled(480)]);
    }
}
