//! The Property Data a Get got, kept for the `cacheTime` of its reply
//! (M2-103 §7.4.3). Every method takes the `now` it is asked at.

use tutti_platform::Instant;

use super::super::header::{PeCacheTime, PePage};
use super::data::PeData;
use super::request::ResourceTarget;
use crate::deadline::Pending;

/// One request's reply: the request is the Resource, its entry and its page.
#[derive(Debug)]
struct Held {
    target: ResourceTarget,
    page: Option<PePage>,
    data: PeData,
    life: Pending,
}

impl Held {
    fn is(&self, target: &ResourceTarget, page: Option<PePage>) -> bool {
        self.target == *target && self.page == page
    }
}

/// The replies held, one per request, each expiring on its own.
#[derive(Debug, Default)]
pub(super) struct GetCache {
    held: Vec<Held>,
}

impl GetCache {
    /// The data of the request, while its `cacheTime` has not elapsed at `now`.
    pub(super) fn fresh(
        &self,
        target: &ResourceTarget,
        page: Option<PePage>,
        now: Instant,
    ) -> Option<&PeData> {
        self.held
            .iter()
            .find(|h| h.is(target, page) && !h.life.expired(now))
            .map(|h| &h.data)
    }

    /// Holds the reply to the request for its `time` from `now`, in place of the
    /// one held for the same request. A `time` of 0 holds nothing.
    pub(super) fn keep(
        &mut self,
        target: &ResourceTarget,
        page: Option<PePage>,
        data: PeData,
        time: PeCacheTime,
        now: Instant,
    ) {
        self.held
            .retain(|h| !h.is(target, page) && !h.life.expired(now));
        if time.as_secs() > 0 {
            self.held.push(Held {
                target: target.clone(),
                page,
                data,
                life: Pending::after(now, time.into()),
            });
        }
    }

    /// Drops every page of the Resource entry: its data changed.
    pub(super) fn forget(&mut self, target: &ResourceTarget) {
        self.held.retain(|h| h.target != *target);
    }
}

#[cfg(test)]
mod tests;
