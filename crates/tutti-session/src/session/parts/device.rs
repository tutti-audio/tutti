//! [`DevicePart`] with the `device` feature: the one-shot listing of the host's output devices.

use std::time::Instant;

use crate::SessionEvent;
use crate::watch::OutputListing;

/// The output-device listing the session started.
#[derive(Debug, Default)]
pub(crate) struct DevicePart {
    /// The one-shot listing of the output devices, until it answers.
    listing: Option<OutputListing>,
    /// A listing was started by [`Session::list_outputs_once`](crate::Session::list_outputs_once).
    listed: bool,
}

impl DevicePart {
    /// Starts `listing`, replacing one still waiting.
    pub(crate) fn start(&mut self, listing: tutti_cpal::DeviceListing) {
        self.listing = Some(OutputListing::new(listing));
    }

    /// Whether [`mark_listed`](Self::start_once) already started one.
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
