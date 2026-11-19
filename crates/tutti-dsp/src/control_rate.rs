//! Control-rate helpers: the solve interval and the
//! channel grouping the filters' held path runs in.

/// Samples between coefficient solves on a parameter that moves inside a block.
///
/// A swept filter solves its coefficients (a `tan` each) on a grid of
/// `COEFF_INTERVAL` frames of the stream, not from a block's start, and
/// interpolates them linearly between solves; a control is therefore in the
/// render up to `COEFF_INTERVAL` frames after it was sampled, wherever the blocks
/// fall. 16 samples is 0.33 ms at 48 kHz.
pub const COEFF_INTERVAL: usize = 16;

/// Channels a recursive filter renders together, sample-inner.
///
/// A recursive filter is latency-bound on one channel: each sample waits on the
/// one before. Running up to `LANES` channels' independent chains side by side
/// fills that latency, while each channel still reads and writes its own planar
/// slice.
pub const LANES: usize = 8;

/// How many of the `remaining` channels the next group takes: the groups are
/// spread evenly rather than filled greedily, so twelve channels run as 6 + 6
/// rather than 8 + 4 (a small trailing group leaves latency unfilled).
#[inline]
pub(crate) fn lane_group(remaining: usize) -> usize {
    let groups = remaining.div_ceil(LANES);
    remaining.div_ceil(groups)
}
