//! An arrival stamp as an `Instant`.

use std::time::{Duration, Instant};

/// How long after its stamp an event may be seen before the stamp is not
/// trusted: a handler that late has been starved, or the stamp's clock has
/// drifted from the OS's by that much.
const MAX_LATENESS: Duration = Duration::from_secs(1);

/// Places an OS stamp, the time since the port started, on the `Instant`
/// clock: an event is as long after the start as its stamp says. The start is
/// read just before the port starts, so no event is stamped before it.
///
/// An event seen before its stamp (the start was read late) or more than
/// [`MAX_LATENESS`] after it (the clocks have drifted apart) re-anchors the
/// start so that it arrives when it was seen, and the events after it are
/// placed against that.
#[derive(Clone, Copy, Debug)]
pub(in crate::backend) struct Anchor {
    start: Instant,
}

impl Anchor {
    /// An anchor whose port starts at `start`.
    pub(in crate::backend) fn new(start: Instant) -> Self {
        Self { start }
    }

    /// The instant an event stamped `stamped` after the start arrived, seen at
    /// `now`.
    pub(in crate::backend) fn arrival(&mut self, stamped: Duration, now: Instant) -> Instant {
        if let Some(at) = self.start.checked_add(stamped)
            && at <= now
            && now - at <= MAX_LATENESS
        {
            return at;
        }
        self.start = now.checked_sub(stamped).unwrap_or(now);
        now
    }
}
