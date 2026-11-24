//! The wait for the host's parameter writes to reach the plugin, as a polled
//! rule.
//!
//! A read or a save made after a write is answered once the audio side has
//! handed the write to the plugin ([`Verdict::Covered`]). The rule reads no
//! clock and blocks nothing: the caller passes the instant it sampled and what
//! the audio side shows, and acts on the verdict, polling again while it is [`Waiting`](Verdict::Waiting).

use std::time::Duration;
use tutti_platform::Instant;
use tutti_rt::Generation;

/// The least a wait gives the audio side, without a sign of it, before it calls
/// the node idle. A node whose blocks are longer than this gives a window of
/// several of its own block periods ([`Settle::window_for`]).
pub(super) const WINDOW: Duration = Duration::from_millis(100);

/// How many block periods of the node a window spans at least.
const PERIODS: u32 = 4;

/// The most a wait gives the audio side to hand the writes over, running or
/// not: the out-of-process side's allowance for a parameter flush before a
/// read proceeds (`Timeouts::param_wait`, `param` 5 s plus a 1 s reply margin;
/// this crate does not depend on the transport that names it). Both hostings
/// proceed with what the plugin holds after their limit.
pub(super) const LIMIT: Duration = Duration::from_secs(6);

/// What the wait makes of its state at an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The audio side has taken the writes: handed them to the plugin, or, for a
    /// parameter the format forbids applying outside a block (CLAP
    /// `REQUIRES_PROCESS` of an active plugin), held them for the next block. A
    /// held write is not the plugin's value yet: a read or a save made while no
    /// block runs does not show it until one does.
    Covered,
    /// The audio side is running, or the window is open: poll again.
    Waiting,
    /// A whole window passed with no sign of the audio side: nothing will
    /// drain the writes, and the control side applies them itself.
    Idle,
    /// The audio side ran for the whole [`LIMIT`] and did not hand the writes to
    /// the plugin (every flush refused, every block failed, values left owed): the
    /// call answers what the plugin holds. It does not apply out of band, since
    /// a running node may be inside a block.
    Unhanded,
}

/// What the audio side shows now.
#[derive(Clone, Copy, Debug)]
pub(super) struct Shown {
    /// Whether it has handed the writes up to the target to the plugin.
    pub(super) covered: bool,
    /// Its count of drain visits (a block or a parked poll).
    pub(super) pulse: u64,
}

/// One wait, for the writes made before it began.
#[derive(Clone, Copy, Debug)]
pub(super) struct Settle {
    target: Generation,
    due: Instant,
    limit: Instant,
    window: Duration,
    pulse: u64,
}

impl Settle {
    /// A wait for the writes up to `target`, begun at `began` with the audio
    /// side's `pulse` as it showed.
    pub(super) fn new(target: Generation, pulse: u64, began: Instant, window: Duration) -> Self {
        Self {
            target,
            due: began + window,
            limit: began + LIMIT,
            window,
            pulse,
        }
    }

    /// The window for a node whose blocks last `period` (zero when unknown):
    /// the longer of [`WINDOW`] and a few block periods, so a block that runs
    /// longer than the minimum window is not taken for an idle node.
    pub(super) fn window_for(period: Duration) -> Duration {
        WINDOW.max(period * PERIODS)
    }

    /// The writes the wait is for: those up to this generation.
    pub(super) fn target(&self) -> Generation {
        self.target
    }

    /// The verdict at `now`. A window that ends while the pulse has moved
    /// since it opened opens again: the audio side is running and will drain,
    /// until the [`LIMIT`] ends the wait.
    pub(super) fn poll(&mut self, shown: Shown, now: Instant) -> Verdict {
        if shown.covered {
            return Verdict::Covered;
        }
        if now < self.due && now < self.limit {
            return Verdict::Waiting;
        }
        if shown.pulse == self.pulse {
            return Verdict::Idle;
        }
        if now >= self.limit {
            return Verdict::Unhanded;
        }
        self.pulse = shown.pulse;
        self.due = now + self.window;
        Verdict::Waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn shown(covered: bool, pulse: u64) -> Shown {
        Shown { covered, pulse }
    }

    fn settle(began: Instant) -> Settle {
        Settle::new(Generation::new(3), 7, began, WINDOW)
    }

    /// **A wait whose writes are covered ends at once, window or not.**
    #[test]
    fn covered_writes_end_the_wait_at_any_instant() {
        let began = Instant::now();
        let mut wait = settle(began);
        assert_eq!(wait.poll(shown(true, 7), began), Verdict::Covered);
        assert_eq!(
            wait.poll(shown(true, 7), began + ms(5000)),
            Verdict::Covered
        );
    }

    /// **The window is the deadline, to the instant**: until it the wait
    /// waits, whatever the pulse shows; at it, with the pulse where it
    /// began, the node is idle.
    #[test]
    fn an_unmoved_pulse_is_idle_exactly_at_the_deadline() {
        let began = Instant::now();
        let mut wait = settle(began);
        assert_eq!(wait.poll(shown(false, 7), began), Verdict::Waiting);
        assert_eq!(
            wait.poll(shown(false, 7), began + WINDOW - ms(1)),
            Verdict::Waiting
        );
        assert_eq!(wait.poll(shown(false, 7), began + WINDOW), Verdict::Idle);
        assert_eq!(wait.poll(shown(false, 7), began + ms(9000)), Verdict::Idle);
    }

    /// **A moving pulse is never idle**: each window that ends with the pulse
    /// moved opens another from that instant, and the node is idle only after
    /// a whole window with the pulse where the last one left it.
    #[test]
    fn a_moving_pulse_reopens_the_window() {
        let began = Instant::now();
        let mut wait = settle(began);
        let first_end = began + WINDOW;
        assert_eq!(wait.poll(shown(false, 8), first_end), Verdict::Waiting);
        assert_eq!(
            wait.poll(shown(false, 9), first_end + WINDOW),
            Verdict::Waiting
        );
        assert_eq!(
            wait.poll(shown(false, 9), first_end + WINDOW + WINDOW - ms(1)),
            Verdict::Waiting
        );
        assert_eq!(
            wait.poll(shown(false, 9), first_end + WINDOW + WINDOW),
            Verdict::Idle
        );
    }

    /// **A pulse that moves for the whole limit ends the wait**, to the
    /// instant: just before the limit the window reopens, at it the audio side
    /// is called to have run without handing the writes over.
    #[test]
    fn a_moving_pulse_ends_at_the_limit() {
        let began = Instant::now();
        let mut wait = settle(began);
        let mut pulse = 7;
        let mut at = began;
        // One tick every window, the pulse moving each time, to the last window
        // before the limit.
        while at + WINDOW < began + LIMIT {
            at += WINDOW;
            pulse += 1;
            assert_eq!(wait.poll(shown(false, pulse), at), Verdict::Waiting);
        }
        pulse += 1;
        assert_eq!(
            wait.poll(shown(false, pulse), began + LIMIT - ms(1)),
            Verdict::Waiting,
            "just before the limit"
        );
        pulse += 1;
        assert_eq!(
            wait.poll(shown(false, pulse), began + LIMIT),
            Verdict::Unhanded,
            "at the limit"
        );
        assert_eq!(
            wait.poll(shown(false, pulse + 1), began + LIMIT + ms(9000)),
            Verdict::Unhanded,
            "and after it"
        );
    }

    /// **Covered still wins at the limit**, and a quiet pulse past it is idle,
    /// not unhanded: nothing ran, so the control side may apply.
    #[test]
    fn the_limit_does_not_outrank_covered_or_idle() {
        let began = Instant::now();
        let mut wait = settle(began);
        assert_eq!(wait.poll(shown(true, 9), began + LIMIT), Verdict::Covered);
        assert_eq!(wait.poll(shown(false, 7), began + LIMIT), Verdict::Idle);
    }

    /// **A long block is not an idle node**: 4,800 frames at 48 kHz is a 100 ms
    /// block, and the window made for it outlasts the minimum, so a wait that has
    /// seen no new pulse for 100 ms still waits.
    #[test]
    fn a_window_follows_the_block_period() {
        let period = Duration::from_secs_f64(4800.0 / 48_000.0);
        assert_eq!(Settle::window_for(Duration::ZERO), WINDOW, "unknown period");
        assert_eq!(Settle::window_for(ms(5)), WINDOW, "short blocks");
        assert_eq!(Settle::window_for(period), period * 4);
        let began = Instant::now();
        let mut wait = Settle::new(Generation::new(3), 7, began, Settle::window_for(period));
        assert_eq!(
            wait.poll(shown(false, 7), began + ms(100)),
            Verdict::Waiting,
            "a block in progress is not idle"
        );
        assert_eq!(
            wait.poll(shown(false, 7), began + ms(400)),
            Verdict::Idle,
            "after the longer window"
        );
    }
}
