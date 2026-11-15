//! Control-rate helpers: the solve interval and the
//! channel grouping the filters' held path runs in.

/// Samples between coefficient solves on a parameter that moves inside a block.
///
/// A filter whose cutoff is swept (by a block ramp or an audio-rate param port)
/// needs a `tan` per coefficient solve. Solving every sample is the dominant
/// cost of a modulated SVF; solving every 16 samples and interpolating the
/// coefficients linearly between solves costs a quarter of a 64-frame block's
/// `tan`s. 16 samples is 0.33 ms at 48 kHz — far below the rate any musical
/// sweep moves at, so the interpolation error is a small fraction of the change
/// between two solves. Measured against the per-sample solve: 3e-5 worst on a
/// 200 Hz → 8 kHz cutoff sweep in 85 ms (4.6e-4 at 64 samples; exactly 0 at
/// 1), 3.5e-6 on a 2 Hz phaser. (Every bank solves on the stream's grid, not
/// from a block's start: a control is in the render up to 16 frames
/// after it was sampled, wherever the blocks fall.)
pub const COEFF_INTERVAL: usize = 16;

/// Channels a recursive node renders together, sample-inner.
///
/// A recursive filter is latency-bound on one channel: each sample waits on
/// the one before. Running a few channels' independent chains side by side
/// fills that latency (instruction-level parallelism) while each still reads
/// and writes its own planar slice. Measured on the f64 SVF at width 6, one
/// block: all six as one group 0.49 µs, as 3 + 3 0.60 µs, one channel at a
/// time 1.33 µs.
pub const LANES: usize = 8;

/// How many of the `remaining` channels the next group takes: the groups are
/// spread evenly rather than filled greedily, so twelve channels run as 6 + 6
/// rather than 8 + 4 (a small trailing group leaves latency unfilled).
#[inline]
pub(crate) fn lane_group(remaining: usize) -> usize {
    let groups = remaining.div_ceil(LANES);
    remaining.div_ceil(groups)
}
