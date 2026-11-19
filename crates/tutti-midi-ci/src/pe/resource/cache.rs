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
    /// How many times held data was dropped for a change.
    drops: u64,
}

impl GetCache {
    /// The number of drops so far: a Get that starts at one count and ends at
    /// another was overtaken by a change and its reply is not kept.
    pub(super) fn drops(&self) -> u64 {
        self.drops
    }

    /// Drops what has expired at `now`.
    pub(super) fn prune(&mut self, now: Instant) {
        self.held.retain(|h| !h.life.expired(now));
    }

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
    /// one held for the same request. A `time` of 0 holds nothing, and so does a
    /// request that began at a count of [`drops`](Self::drops) other than now's.
    pub(super) fn keep(
        &mut self,
        target: &ResourceTarget,
        page: Option<PePage>,
        data: PeData,
        time: PeCacheTime,
        began_at: u64,
        now: Instant,
    ) {
        self.held
            .retain(|h| !h.is(target, page) && !h.life.expired(now));
        if time.as_secs() > 0 && began_at == self.drops {
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
        self.drops += 1;
    }

    /// Drops everything held.
    pub(super) fn clear(&mut self) {
        self.held.clear();
        self.drops += 1;
    }
}

#[cfg(test)]
mod tests;
