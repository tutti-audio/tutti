//! How many plans an `update`'s events say it sent.

use tutti_session::SessionEvent;

/// The [`SessionEvent::Committed`] in `events`: one for each plan the update sent.
pub(crate) fn committed(events: &[SessionEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Committed))
        .count()
}
