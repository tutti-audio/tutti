//! The Property Data a Get got, kept for the `cacheTime` of its reply
//! (M2-103 §7.4.3). Every method takes the `now` it is asked at.

use tutti_platform::Instant;

use super::super::header::{PeCacheTime, PePage};
use super::super::tree::PeJson;
use super::data::PeData;
use super::request::ResourceTarget;
use crate::deadline::Pending;

/// What a reply gave: its Property Data and the `totalCount` of its header.
#[derive(Debug)]
pub(super) struct Cached {
    pub(super) data: PeData,
    /// The reply's JSON as it came, which a walk joins: a re-encoding of `data`
    /// would not be what the peer sent.
    pub(super) json: Option<PeJson>,
    pub(super) total_count: Option<u32>,
}

/// One request's reply: the request is the Resource, its entry and its page.
#[derive(Debug)]
struct Held {
    target: ResourceTarget,
    page: Option<PePage>,
    cached: Cached,
    life: Pending,
}

/// Whether the `cacheTime`s of a Resource's requests are kept apart or as one
/// (M2-103 §7.4.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PeCacheScope {
    /// Each request, each page of a paginated Get, expires by its own
    /// `cacheTime`: the spec's recommendation.
    #[default]
    PerRequest,
    /// One `cacheTime` per Resource entry, `resId` and all: the data held for an
    /// entry expires together. The oldest `cacheTime` is retained (#5), read
    /// strictly: a reply that arrives while data is held cannot lengthen the
    /// time, and one with a shorter time shortens it, so no page is used past its
    /// own reply's `cacheTime` or the oldest. A reply that arrives after the time
    /// held ended finds all the data held dropped, and only it is valid (#6).
    PerResource,
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
    scope: PeCacheScope,
    /// How many times held data was dropped for a change.
    drops: u64,
}

impl GetCache {
    /// The number of drops so far: a Get that starts at one count and ends at
    /// another was overtaken by a change and its reply is not kept.
    pub(super) fn drops(&self) -> u64 {
        self.drops
    }

    /// Keeps the replies by `scope` from now on, and drops what is held.
    pub(super) fn set_scope(&mut self, scope: PeCacheScope) {
        self.scope = scope;
        self.clear();
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
    ) -> Option<&Cached> {
        self.held
            .iter()
            .find(|h| h.is(target, page) && !h.life.expired(now))
            .map(|h| &h.cached)
    }

    /// Holds the reply to the request for its `time` from `now`, in place of the
    /// one held for the same request. A `time` of 0 holds nothing, and so does a
    /// request that began at a count of [`drops`](Self::drops) other than now's.
    pub(super) fn keep(
        &mut self,
        target: &ResourceTarget,
        page: Option<PePage>,
        cached: Cached,
        time: PeCacheTime,
        began_at: u64,
        now: Instant,
    ) {
        let keeping = time.as_secs() > 0 && began_at == self.drops;
        let life = keeping.then(|| {
            let own = Pending::after(now, time.into());
            if self.scope == PeCacheScope::PerResource {
                // The entry's time held, this request's own reply included, as the
                // retain below has not yet dropped it: the reply may not outlive it.
                self.held
                    .iter()
                    .filter(|h| h.target == *target && !h.life.expired(now))
                    .fold(own, |life, h| life.min(h.life))
            } else {
                own
            }
        });
        self.held.retain_mut(|h| {
            if h.life.expired(now) || h.is(target, page) {
                return false;
            }
            if let Some(life) = life
                && self.scope == PeCacheScope::PerResource
                && h.target == *target
            {
                h.life = life;
            }
            true
        });
        if let Some(life) = life {
            self.held.push(Held {
                target: target.clone(),
                page,
                cached,
                life,
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
