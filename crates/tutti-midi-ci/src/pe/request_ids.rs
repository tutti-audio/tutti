//! The Request IDs one device has in use toward one peer (M2-103 §5.3).

use super::capabilities::SimultaneousRequests;
use super::chunk::PeRequestId;

/// The Request IDs of one peer: a set of the 128 that are in use, and where the
/// next search starts.
#[derive(Clone, Debug, Default)]
pub(super) struct RequestIds {
    used: u128,
    next: u8,
}

impl RequestIds {
    const COUNT: u8 = 128;

    /// An ID not in use, searching on from the one after the last given, unless
    /// `limit` IDs are in use already.
    pub(super) fn allocate(&mut self, limit: SimultaneousRequests) -> Option<PeRequestId> {
        if self.used.count_ones() >= u32::from(limit.get()) {
            return None;
        }
        (0..Self::COUNT)
            .map(|step| (self.next + step) % Self::COUNT)
            .find(|n| self.used & (1u128 << n) == 0)
            .and_then(|n| {
                self.used |= 1u128 << n;
                self.next = (n + 1) % Self::COUNT;
                PeRequestId::new(n)
            })
    }

    pub(super) fn release(&mut self, id: PeRequestId) {
        self.used &= !(1u128 << id.get());
    }
}
