//! [`DevicePart`] with the `device` feature: the one-shot listing of the host's output devices.

use tutti_cpal::DeviceListing;
use tutti_platform::{Executor, Instant, WeakExecutor};

use crate::SessionEvent;
use crate::watch::{OutputListing, PolledSource};

/// The output-device listing the session started.
#[derive(Debug)]
pub(crate) struct DevicePart {
    /// The session's executor, reached by the listing and the poller loops this
    /// session starts.
    executor: WeakExecutor,
    /// The one-shot listing of the output devices, until it answers.
    listing: Option<OutputListing>,
    /// A listing was started by [`Session::list_outputs_once`](crate::Session::list_outputs_once).
    listed: bool,
}

impl DevicePart {
    /// The part for a session on `executor`.
    pub(crate) fn new(executor: &Executor) -> Self {
        Self {
            executor: executor.downgrade(),
            listing: None,
            listed: false,
        }
    }

    /// The session's executor, or `None` once it has shut down.
    pub(crate) fn executor(&self) -> Option<Executor> {
        self.executor.upgrade()
    }

    /// Starts `listing`, replacing one still waiting, whose blocking step is
    /// left to end on its own.
    pub(crate) fn start(&mut self, listing: DeviceListing) {
        let Some(executor) = self.executor.upgrade() else {
            self.listing = None;
            return;
        };
        self.listing = Some(OutputListing::new(&executor, listing));
    }

    /// The loop of `poller` on the session's executor, or `None` once the
    /// executor has shut down.
    pub(crate) fn poll_source(&self, poller: tutti_cpal::DevicePoller) -> Option<PolledSource> {
        let executor = self.executor.upgrade()?;
        Some(PolledSource::start(&executor, poller))
    }

    /// Whether a listing was started once ([`mark_listed`](Self::mark_listed)).
    pub(crate) fn listed(&self) -> bool {
        self.listed
    }

    /// Notes that a listing was started once.
    pub(crate) fn mark_listed(&mut self) {
        self.listed = true;
    }

    /// The listing's share of [`Phase::Poll`](crate::Phase::Poll): the answer of
    /// the output-device listing, if one is running.
    pub(crate) fn poll(&mut self, now: Instant, out: &mut Vec<SessionEvent>) {
        if let Some(listing) = &mut self.listing {
            let (event, finished) = listing.poll(now);
            out.extend(event);
            if finished {
                self.listing = None;
            }
        }
    }
}
