//! The channel grouping a recursive filter renders in: [`CHANNEL_GROUP`], the most channels
//! one pass takes, and [`group_size`], how a width splits into passes.

/// Channels a recursive filter renders together, sample-inner.
///
/// A recursive filter is latency-bound on one channel: each sample waits on the
/// one before. Running up to `CHANNEL_GROUP` channels' independent chains side by side
/// fills that latency, while each channel still reads and writes its own planar
/// slice.
pub const CHANNEL_GROUP: usize = 8;

/// How many of the `remaining` channels the next group takes: the groups are
/// spread evenly rather than filled greedily, so twelve channels run as 6 + 6
/// rather than 8 + 4 (a small trailing group leaves latency unfilled).
#[inline]
pub(crate) fn group_size(remaining: usize) -> usize {
    let groups = remaining.div_ceil(CHANNEL_GROUP);
    remaining.div_ceil(groups)
}
