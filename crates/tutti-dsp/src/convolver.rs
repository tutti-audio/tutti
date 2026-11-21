//! Mono FFT convolver primitive: a partitioned FFT convolution whose
//! transformed impulse response is **stored once**, in an [`IrSpectra`]
//! behind an `Arc`, and shared by every convolver built from it.
//!
//! Each level of the convolution is the uniformly partitioned overlap-add
//! convolution `fft-convolver` 0.3 runs (its `FFTConvolver`), with the same
//! arithmetic in the same order, over [`RealFft`](crate::RealFft), the engine's
//! CPU-independent real FFT (rustfft's scalar planner, where
//! `fft-convolver`'s `realfft` picks SIMD at run time). An output is
//! therefore bit-identical across CPUs on one OS and libm. It is written here
//! rather than depended on because `fft-convolver` keeps the IR's spectra
//! inside each convolver with no way to share them, so every channel of a
//! shared-IR node and every fork of a node would hold its own copy:
//! megabytes for a long reverb. Here a channel, a clone and a fork hold one
//! `Arc` to the spectra, which nothing writes after they are built; each
//! keeps only its own running state (the input spectra history, the overlap,
//! the FFT scratch).
//!
//! # Non-uniform partitions
//!
//! The latency is the head partition `B` (the `block_size` given to
//! [`Convolver::new`]): the output at frame `n` is the convolution at frame
//! `n - B`. A long IR is cut into **levels**, each a uniform partitioned
//! convolver with its own partition `P` (a power-of-two multiple of `B`) and
//! FFT size `2P`: the head level has `P = B`, and the tail levels have larger
//! ones, up to four times `B` and 4096 frames, so the IR's late part is
//! covered by far fewer, longer partitions. A partition costs one complex
//! multiply-accumulate per bin per `P` frames, so with `L` taps a level costs
//! `L / P` per frame: a tail four times the partition is four times cheaper,
//! and the transforms it adds are a small fixed overhead per level.
//!
//! A level covers the IR taps `start..start + count * P`. It is a uniform
//! convolver of latency `P` followed by a delay of `start + B - P` frames,
//! which is never negative because a tail level starts at `start = P` (one of
//! its own partitions in): the total is `B` on every level, so the node's
//! declared latency is the same as the uniform convolver's. That delay is
//! **slack**: once a block of `P` input frames is complete the level's output
//! is not due for `start + B - P` frames (`B` for a tail level), and the
//! level's two transforms run in **steps** at fixed offsets inside it, at
//! offsets no other level's steps use, so no host block of up to one slot of
//! `B` carries more than one step of a tail level's transforms. A tail
//! transform whose inner FFT has more than [`MAX_SUB_FFT`] points
//! ([`StagedRealFft`]) is cut by decimation in time into sub-FFTs of at most
//! that many points and radix-2 combine passes, one step each, so the biggest
//! unit of work a host block is handed is a 1024-point complex FFT whatever
//! the partition; the cut costs a few percent of the transform's total and
//! changes its rounding but not its result. Every level's output is summed
//! into one ring at the frame it is due, in the order of the steps' frames,
//! which does not depend on how the host splits its calls.
//!
//! A cost model picks the levels (`plan_levels`): the layout with the lowest
//! modelled cost per frame wins, the single uniform level included, and only
//! if it costs at most 0.7 of the uniform one. A short IR (where an extra
//! level's transforms cost about what they save) is the uniform convolver it
//! always was, as is any IR when `B` is 4096 or more.
//!
//! **Each level's partition sum is spread across the blocks of its
//! partition.** Only the newest partition's multiply, the inverse transform
//! and the overlap-add need the input that completes a block; the products of
//! the older partitions with the older history are fixed as soon as the
//! previous block's transform is done. [`Convolver::process`] therefore
//! sums them a few partitions per call, in proportion to the frames consumed,
//! finishing a call's length before the next block's transform. Each bin adds
//! its partitions in ascending order from zero, so any split of the work into
//! calls gives the same bits.
//!
//! # Zero latency
//!
//! [`Convolver::zero_latency`] splits the IR at `B` (Gardner, "Efficient Convolution without
//! Input-Output Delay", 1995). The first `B` taps run as a direct-form FIR, one output per
//! frame with no wait: `B` multiply-adds per frame per convolver. The taps from `B` on are an
//! IR of their own whose convolution has latency `B` (above), and an IR starting `B` taps
//! late is exactly that delay, so the levels' output at frame `n` is the tail's contribution
//! to the full convolution at frame `n`, with no shifting. The two are summed: the latency is
//! `0`. The head's taps are stored with the [`IrSpectra`], so they are shared like the
//! spectra; a convolver keeps only its input history for them.
//!
//! No allocation happens after construction, and the render is bit-identical
//! for any split of the input into calls, including one frame at a time.

use std::sync::Arc;

use crate::FftFloat;
use tutti_buffers::{Float, Mono, MonoMut, Sample};
use tutti_units::Samples;

use crate::Complex;
use crate::fft::StagedRealFft;
use crate::sample_convert::convert_equal;
use head::{HeadState, HeadTaps};

/// Default FFT block size when the caller doesn't pick one. A power of two,
/// as every partition size here is.
pub(crate) const DEFAULT_BLOCK_SIZE: Samples = Samples::new(512);

/// A tail level's partition is at most this many times the head's, and at
/// most [`MAX_PARTITION`] frames: the larger it is, the more steps its
/// transforms take, and the fewer slots of the head's slack are left.
const MAX_TAIL_RATIO: usize = 4;
const MAX_PARTITION: usize = 4096;

/// A tail transform's inner complex FFT is cut into sub-FFTs of at most this
/// many points, run on different host blocks ([`StagedRealFft`]): a partition
/// `P` has a `P`-point inner FFT, so a level of partition 2048 runs its
/// transforms as two steps of 1024 points and a combine. A head never splits
/// (its transforms have no slack).
const MAX_SUB_FFT: usize = 1024;

/// A non-uniform layout replaces the uniform one only when its modelled cost
/// is at most this fraction of it: the model leaves out each level's
/// bookkeeping, and an IR short enough for the two to be close measures
/// slightly worse split than whole.
const MIN_GAIN: f64 = 0.7;

/// The cost model's figures, in nanoseconds, measured on a desktop core: one
/// complex multiply-accumulate of a bin (spectra read from memory), and one
/// real transform per `n * log2(n)`. They decide between layouts and nothing
/// else, so only their ratio matters.
const MAC_NS: f64 = 0.7;
const FFT_NS: f64 = 0.29;

/// One level of a layout: `count` partitions of `part` frames covering the
/// IR taps from `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    part: usize,
    start: usize,
    count: usize,
}

/// The modelled cost per frame of `plan`, in nanoseconds: the multiply-
/// accumulates of every partition and the two transforms of every block.
fn plan_cost(plan: &[Plan]) -> f64 {
    plan.iter()
        .map(|l| {
            let p = l.part as f64;
            let n = 2 * l.part;
            // Every partition is a power of two, so the integer log is exact.
            let n_log_n = n as f64 * f64::from(n.ilog2());
            l.count as f64 * (p + 1.0) * MAC_NS / p + 2.0 * FFT_NS * n_log_n / p
        })
        .sum()
}

/// The levels for partitions `parts` (ascending, `parts[0]` the head) over an
/// IR of `ir_len` taps. A level starts at its own partition size, which is
/// what leaves a tail level `B` frames of slack: the head takes `parts[1] / B`
/// partitions, and each later level the number that reaches the next one.
fn build_plan(ir_len: usize, parts: &[usize]) -> Vec<Plan> {
    let mut plan = Vec::new();
    let mut start = 0;
    for (j, &part) in parts.iter().enumerate() {
        if start >= ir_len {
            break;
        }
        let want = match parts.get(j + 1) {
            Some(&next) if j == 0 => next / part,
            Some(&next) => next / part - 1,
            None => usize::MAX,
        };
        let count = want.min((ir_len - start).div_ceil(part));
        plan.push(Plan { part, start, count });
        start += count * part;
    }
    plan
}

/// The layout with the lowest modelled cost: the uniform one (a single
/// head level), or the head and tail levels whose partitions are powers of
/// two up to [`MAX_TAIL_RATIO`] times the head's, if that costs at most
/// [`MIN_GAIN`] of the uniform. Empty for an empty IR.
fn plan_levels(ir_len: usize, head: usize) -> Vec<Plan> {
    if ir_len == 0 {
        return Vec::new();
    }
    let uniform = vec![Plan {
        part: head,
        start: 0,
        count: ir_len.div_ceil(head),
    }];
    let uniform_cost = plan_cost(&uniform);
    let mut best = (uniform_cost * MIN_GAIN, None);
    let reach = MAX_PARTITION.min(head.saturating_mul(MAX_TAIL_RATIO));
    let sizes: Vec<usize> = std::iter::successors(head.checked_mul(2), |p| p.checked_mul(2))
        .take_while(|&p| p <= reach)
        .collect();
    for mask in 1u32..(1 << sizes.len()) {
        let parts: Vec<usize> = std::iter::once(head)
            .chain(
                sizes
                    .iter()
                    .enumerate()
                    .filter(|(k, _)| mask >> k & 1 == 1)
                    .map(|(_, &p)| p),
            )
            .collect();
        let cand = build_plan(ir_len, &parts);
        let cost = plan_cost(&cand);
        if cost < best.0 {
            best = (cost, Some(cand));
        }
    }
    best.1.unwrap_or(uniform)
}

/// Zeroed complex spectrum of `bins` bins.
fn zeros<F: Float>(bins: usize) -> Vec<Complex<F>> {
    vec![Complex::new(F::ZERO, F::ZERO); bins]
}

/// `dst[..src.len()] = src`, the rest zero.
#[inline]
fn copy_and_pad<F: Float>(dst: &mut [F], src: &[F]) {
    dst[..src.len()].copy_from_slice(src);
    dst[src.len()..].fill(F::ZERO);
}

/// `result += a × b`, bin by bin: the complex multiply-accumulate of the
/// partitioned convolution, in `fft-convolver`'s operand order.
#[inline]
fn complex_multiply_accumulate<F: Float>(
    result: &mut [Complex<F>],
    a: &[Complex<F>],
    b: &[Complex<F>],
) {
    debug_assert!(result.len() == a.len() && result.len() == b.len());
    for ((r, a), b) in result.iter_mut().zip(a).zip(b) {
        r.re += a.re * b.re - a.im * b.im;
        r.im += a.re * b.im + a.im * b.re;
    }
}

/// One level of an impulse response, transformed: `segments.len()` spectra of
/// `part` frames each, and the two transforms of `2 × part` points that
/// produced them and that every convolver over it runs.
struct Level<F: FftFloat> {
    part: usize,
    /// Frames from the level's uniform output (latency `part`) to its due
    /// time (latency `B`): `start + B - part`. Zero for the head.
    delay: usize,
    /// Frames after a block of `part` inputs is complete at which each step
    /// of its transforms runs: the forward transform's steps, then the
    /// multiply and the inverse transform's. Non-decreasing, ending at most
    /// `delay` and below `part`. `forward_steps` of them are the forward's.
    steps: Vec<usize>,
    forward_steps: usize,
    /// `part + 1` bins each.
    segments: Vec<Vec<Complex<F>>>,
    /// Both directions, `2 × part` points, in steps.
    fft: StagedRealFft<F>,
    /// `1 / (2 part)`: the inverse transform is unnormalised. A power of two, so exact.
    norm: F,
}

/// An impulse response, transformed: its levels, built once and
/// read-only after that. Nothing on the audio thread (or anywhere) writes it,
/// so any number of convolvers (a node's channels, a clone, a fork) share one
/// behind an `Arc`.
pub struct IrSpectra<F: FftFloat = f32> {
    block_size: Samples,
    ir_len: Samples,
    levels: Vec<Level<F>>,
    /// The first `block_size` taps as a direct-form FIR, in a zero-latency IR (the levels
    /// then cover the taps after them); `None` otherwise.
    head: Option<HeadTaps<F>>,
    /// Power-of-two lengths of the input ring (the newest `part` frames plus
    /// the first step's offset, of every level) and the output ring (the
    /// furthest frame a level writes ahead of the read cursor).
    input_ring: usize,
    output_ring: usize,
}

impl<F: FftFloat> std::fmt::Debug for IrSpectra<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrSpectra")
            .field("block_size", &self.block_size)
            .field("ir_len", &self.ir_len)
            .field("head", &self.head)
            .field("input_ring", &self.input_ring)
            .field("output_ring", &self.output_ring)
            .finish_non_exhaustive()
    }
}

impl<F: FftFloat> IrSpectra<F> {
    /// Partition and transform `ir` at `block_size`, which must be a power
    /// of two. Allocates; control thread.
    fn new(ir: &[F], block_size: Samples) -> Self {
        Self::split(ir, block_size, false)
    }

    /// [`new`](Self::new), and with `zero_latency` the first `block_size` taps
    /// are the direct head and the levels cover the rest.
    fn split(ir: &[F], block_size: Samples, zero_latency: bool) -> Self {
        debug_assert!(block_size.get().is_power_of_two());
        let split = if zero_latency {
            block_size.get().min(ir.len())
        } else {
            0
        };
        let (head, tail) = ir.split_at(split);
        let mut spectra = Self::from_plan(
            tail,
            block_size,
            &plan_levels(tail.len(), block_size.get()),
            MAX_SUB_FFT,
        );
        spectra.ir_len = Samples::new(ir.len());
        spectra.head = zero_latency.then(|| HeadTaps::new(head));
        spectra
    }

    /// `ir` cut into the levels of `plan`, whose tail transforms are cut into
    /// sub-FFTs of at most `max_sub` points.
    fn from_plan(ir: &[F], block_size: Samples, plan: &[Plan], max_sub: usize) -> Self {
        let head = block_size.get();
        // A tail level's transforms (`2 * part` real points, `part` complex)
        // run in `part / max_sub` sub-FFTs and the passes around them; a
        // head's run whole, at its start.
        let ffts: Vec<StagedRealFft<F>> = plan
            .iter()
            .enumerate()
            .map(|(j, l)| {
                StagedRealFft::new(
                    2 * l.part,
                    if j == 0 { 1 } else { l.part.div_ceil(max_sub) },
                )
            })
            .collect();
        // The tail levels' steps in the head partition's cycle: the head's
        // transforms are at its start, and the steps of each tail level (its
        // forward steps, then its inverse's) at slots that no other step
        // uses, spaced evenly through the `block_size` frames of slack, so
        // that a host block no longer than one slot holds at most one of
        // them.
        let tail_steps: usize = ffts[1.min(ffts.len())..]
            .iter()
            .map(|f| f.forward_steps() + f.inverse_steps())
            .sum();
        let slot = (head / (tail_steps + 1)).max(1);
        let mut taken = 0;
        let levels: Vec<Level<F>> = plan
            .iter()
            .zip(ffts)
            .enumerate()
            .map(|(j, (l, fft))| {
                let mut scratch = fft.new_scratch();
                let mut buffer = vec![F::ZERO; 2 * l.part];
                let segments = (0..l.count)
                    .map(|i| {
                        let lo = l.start + i * l.part;
                        let hi = ir.len().min(lo + l.part);
                        copy_and_pad(&mut buffer, &ir[lo..hi]);
                        let mut spectrum = zeros(l.part + 1);
                        for step in 0..fft.forward_steps() {
                            fft.forward_step(step, Mono::new(&buffer), &mut spectrum, &mut scratch);
                        }
                        spectrum
                    })
                    .collect();
                let delay = l.start + head - l.part;
                let (forward_steps, steps) = (
                    fft.forward_steps(),
                    fft.forward_steps() + fft.inverse_steps(),
                );
                let steps = (0..steps)
                    .map(|_| {
                        if j == 0 {
                            return 0;
                        }
                        taken += 1;
                        (taken * slot).min(delay)
                    })
                    .collect();
                Level {
                    part: l.part,
                    delay,
                    steps,
                    forward_steps,
                    segments,
                    fft,
                    norm: F::from_design(0.5 / l.part as f64),
                }
            })
            .collect();
        let input_ring = levels
            .iter()
            .map(|l| l.part + l.steps[0])
            .max()
            .unwrap_or(1)
            .next_power_of_two();
        let output_ring =
            (levels.iter().map(|l| l.delay + l.part).max().unwrap_or(0) + 1).next_power_of_two();
        Self {
            block_size,
            ir_len: Samples::new(ir.len()),
            levels,
            head: None,
            input_ring,
            output_ring,
        }
    }

    /// The partition size and partition count of each level, head first.
    #[cfg(test)]
    #[must_use]
    pub(super) fn shape(&self) -> Vec<(usize, usize)> {
        self.levels
            .iter()
            .map(|l| (l.part, l.segments.len()))
            .collect()
    }
}

/// One convolver's running state for one [`Level`]: everything here is this
/// convolver's own; the spectra are only read.
#[derive(Clone)]
struct LevelState<F: FftFloat> {
    /// The input's spectra, one per partition: a ring whose write slot is
    /// `current`; the block before is at `current + 1`.
    history: Vec<Vec<Complex<F>>>,
    current: usize,
    /// `2 × part`: the padded input going in, the product coming out.
    buffer: Vec<F>,
    /// Both transforms' scratch, which holds a transform between its steps.
    scratch: Vec<Complex<F>>,
    /// The older partitions' products for the *next* block, summed a few
    /// partitions at a time ([`accumulate_to`](Self::accumulate_to)):
    /// partitions `1..=accumulated` are in, and `begun` says the sum was
    /// zeroed for this block.
    building: Vec<Complex<F>>,
    accumulated: usize,
    begun: bool,
    /// The finished sum for the block whose multiply is pending.
    ready: Vec<Complex<F>>,
    conv: Vec<Complex<F>>,
    /// The second half of the last block's product, added to the next.
    overlap: Vec<F>,
    /// The frame (frames pushed so far) at which the block being transformed
    /// was complete, and the next of the level's steps to run for it
    /// (`Level::steps`, which are offsets from `base`).
    base: u64,
    step: usize,
    /// The frame of the last forward step, once the block's forward
    /// transform is done and until the next one starts: the spread of the sum
    /// is measured from it.
    spread_from: Option<u64>,
    /// A forward transform has finished, so a sum for the next block exists.
    seeded: bool,
    /// A step of a forward transform ran in the current call.
    crossed: bool,
}

impl<F: FftFloat> std::fmt::Debug for LevelState<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LevelState")
            .field("current", &self.current)
            .field("accumulated", &self.accumulated)
            .field("begun", &self.begun)
            .field("base", &self.base)
            .field("step", &self.step)
            .field("spread_from", &self.spread_from)
            .field("seeded", &self.seeded)
            .field("crossed", &self.crossed)
            .finish_non_exhaustive()
    }
}

impl<F: FftFloat> LevelState<F> {
    /// A level's state in its freshly built state. Allocates.
    fn new(lv: &Level<F>) -> Self {
        let zero = Complex::new(F::ZERO, F::ZERO);
        Self {
            history: vec![vec![zero; lv.part + 1]; lv.segments.len()],
            current: 0,
            buffer: vec![F::ZERO; 2 * lv.part],
            scratch: lv.fft.new_scratch(),
            building: zeros(lv.part + 1),
            accumulated: 0,
            begun: false,
            ready: zeros(lv.part + 1),
            conv: zeros(lv.part + 1),
            overlap: vec![F::ZERO; lv.part],
            base: lv.part as u64,
            step: 0,
            spread_from: None,
            seeded: false,
            crossed: false,
        }
    }

    /// The frame at which the next step runs.
    fn next_frame(&self, lv: &Level<F>) -> u64 {
        self.base + lv.steps[self.step] as u64
    }

    /// The step due: a step of the block's forward transform, then of its
    /// multiply and inverse transform, then the next block's.
    fn run_step(&mut self, lv: &Level<F>, input: &[F], output: &mut [F]) {
        let at = self.next_frame(lv);
        if self.step < lv.forward_steps {
            self.forward(lv, input, at);
        } else {
            self.inverse(lv, self.step - lv.forward_steps, output);
        }
        self.step += 1;
        if self.step == lv.steps.len() {
            self.step = 0;
            self.base += lv.part as u64;
        }
    }

    /// Partitions `1..=upto` of the older products summed into `building`.
    /// Everything they read (the spectra, and the history slots other than
    /// `current`, which the next forward transform overwrites) is fixed
    /// between two blocks, and each bin sums its partitions in ascending
    /// order from zero, as one whole-block pass would, so any split of the
    /// partitions into calls gives the same bits.
    fn accumulate_to(&mut self, lv: &Level<F>, upto: usize) {
        let count = lv.segments.len();
        if !self.begun {
            self.building.fill(Complex::new(F::ZERO, F::ZERO));
            self.begun = true;
        }
        let upto = upto.min(count - 1);
        let from = self.accumulated + 1;
        for (i, segment) in lv.segments.iter().enumerate().take(upto + 1).skip(from) {
            let slot = (self.current + i) % count;
            complex_multiply_accumulate(&mut self.building, segment, &self.history[slot]);
        }
        self.accumulated = self.accumulated.max(upto);
    }

    /// Forward step, at frame `at`. The first finishes the sum for the block,
    /// takes the block of `part` frames that ended `steps[0]` frames ago from
    /// the input ring, and starts its transform; the last writes the spectrum
    /// into the write slot.
    fn forward(&mut self, lv: &Level<F>, input: &[F], at: u64) {
        if self.step == 0 {
            if self.seeded {
                self.accumulate_to(lv, usize::MAX);
                std::mem::swap(&mut self.building, &mut self.ready);
            }
            self.accumulated = 0;
            self.begun = false;
            self.spread_from = None;
            let mask = input.len() - 1;
            let first = (self.base as usize).wrapping_sub(lv.part);
            for (r, b) in self.buffer[..lv.part].iter_mut().enumerate() {
                *b = input[first.wrapping_add(r) & mask];
            }
            self.buffer[lv.part..].fill(F::ZERO);
        }
        lv.fft.forward_step(
            self.step,
            Mono::new(&self.buffer),
            &mut self.history[self.current],
            &mut self.scratch,
        );
        if self.step + 1 == lv.forward_steps {
            self.current = if self.current > 0 {
                self.current - 1
            } else {
                lv.segments.len() - 1
            };
            self.spread_from = Some(at);
            self.seeded = true;
        }
    }

    /// Inverse step `k`. The first is the block's product: the older
    /// partitions' sum plus the newest partition. The last
    /// normalises the result, adds it to the overlap, and sums it into the
    /// output ring where the level's output is due.
    fn inverse(&mut self, lv: &Level<F>, k: usize, output: &mut [F]) {
        if k == 0 {
            let count = lv.segments.len();
            let newest = (self.current + 1) % count;
            self.conv.copy_from_slice(&self.ready);
            complex_multiply_accumulate(&mut self.conv, &self.history[newest], &lv.segments[0]);
        }
        // A non-zero imaginary part at DC or Nyquist, which the spectrum of a
        // real input has only when the input held NaN or ±∞, is ignored by
        // the inverse and the transform completes, so the block is used as
        // computed: a non-finite input propagates as through any linear
        // filter, the same in debug and release, and leaves with the
        // partition that carried it.
        lv.fft.inverse_step(
            k,
            &mut self.conv,
            MonoMut::new(&mut self.buffer),
            &mut self.scratch,
        );
        if k + 1 < lv.steps.len() - lv.forward_steps {
            return;
        }
        // The inverse is unnormalised.
        for x in &mut self.buffer {
            *x *= lv.norm;
        }
        let mask = output.len() - 1;
        let due = (self.base as usize).wrapping_add(lv.delay);
        for (r, (&y, &v)) in self.buffer[..lv.part].iter().zip(&self.overlap).enumerate() {
            output[due.wrapping_add(r) & mask] += y + v;
        }
        self.overlap.copy_from_slice(&self.buffer[lv.part..]);
    }

    /// Clear the running state; the spectra stay.
    fn reset(&mut self, lv: &Level<F>) {
        let zero = Complex::new(F::ZERO, F::ZERO);
        self.buffer.fill(F::ZERO);
        for h in &mut self.history {
            h.fill(zero);
        }
        self.building.fill(zero);
        self.ready.fill(zero);
        self.conv.fill(zero);
        self.accumulated = 0;
        self.begun = false;
        self.overlap.fill(F::ZERO);
        self.current = 0;
        self.base = lv.part as u64;
        self.step = 0;
        self.spread_from = None;
        self.seeded = false;
        self.crossed = false;
    }
}

/// Real-time-safe mono convolver.
///
/// Composes the levels of the IR's [`IrSpectra`], which it shares (a `Clone`
/// shares them, and so does [`fresh`](Self::fresh)), with its own running
/// state, all sized at construction. The hot path (`process_sample`,
/// `process`) never allocates.
#[derive(Clone)]
pub struct Convolver<F: FftFloat = f32> {
    spectra: Arc<IrSpectra<F>>,
    levels: Vec<LevelState<F>>,
    /// The newest input frames, a ring of `spectra.input_ring`.
    input: Vec<F>,
    /// The output not yet read: every level's blocks summed at the frames they
    /// are due, a ring of `spectra.output_ring`, zeroed as it is read.
    output: Vec<F>,
    /// The input history of the direct head, for a zero-latency IR.
    head: Option<HeadState<F>>,
    /// Frames consumed since construction or the last reset.
    pushed: u64,
}

impl<F: FftFloat> std::fmt::Debug for Convolver<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Convolver")
            .field("spectra", &self.spectra)
            .field("levels", &self.levels)
            .field("pushed", &self.pushed)
            .finish_non_exhaustive()
    }
}

impl<F: FftFloat> Convolver<F> {
    /// Creates a new convolver from an impulse response in any sample format,
    /// converted to `F`.
    ///
    /// `block_size` is rounded up to the next power of two; it is the head
    /// partition and the latency.
    #[must_use]
    pub fn new<S: Sample>(ir: Mono<'_, S>, block_size: Samples) -> Self {
        let (ir, block_size) = Self::prepared(ir, block_size);
        Self::over(Arc::new(IrSpectra::new(&ir, block_size)))
    }

    /// [`new`](Self::new) with no latency: the IR's first `block_size` taps run as a
    /// direct-form FIR and the partitioned levels cover the rest, so the output at frame
    /// `n` is the convolution at frame `n`.
    ///
    /// The head costs `block_size` multiply-adds per frame (fewer for a shorter IR), on top
    /// of the levels'; its taps are in the shared [`IrSpectra`].
    #[must_use]
    pub fn zero_latency<S: Sample>(ir: Mono<'_, S>, block_size: Samples) -> Self {
        let (ir, block_size) = Self::prepared(ir, block_size);
        Self::over(Arc::new(IrSpectra::split(&ir, block_size, true)))
    }

    /// `ir` in `F` and `block_size` rounded up to a power of two.
    fn prepared<S: Sample>(ir: Mono<'_, S>, block_size: Samples) -> (Vec<F>, Samples) {
        let mut converted = vec![F::ZERO; ir.samples().len()];
        convert_equal(ir.samples(), &mut converted);
        (
            converted,
            Samples::new(block_size.get().next_power_of_two().max(2)),
        )
    }

    /// A convolver over `ir` at `block_size` (a power of two) cut by `parts`
    /// (the partition of each level, ascending, the head first) rather than
    /// by the cost model.
    #[cfg(test)]
    fn with_parts(ir: &[F], block_size: usize, parts: &[usize]) -> Self {
        Self::with_parts_split(ir, block_size, parts, MAX_SUB_FFT)
    }

    /// [`with_parts`](Self::with_parts), with the tail transforms cut into
    /// sub-FFTs of at most `max_sub` points.
    #[cfg(test)]
    fn with_parts_split(ir: &[F], block_size: usize, parts: &[usize], max_sub: usize) -> Self {
        assert_eq!(parts[0], block_size);
        let block_size = Samples::new(block_size);
        let plan = build_plan(ir.len(), parts);
        Self::over(Arc::new(IrSpectra::from_plan(
            ir, block_size, &plan, max_sub,
        )))
    }

    /// A convolver over `spectra`, in its freshly built state.
    fn over(spectra: Arc<IrSpectra<F>>) -> Self {
        Self {
            levels: spectra.levels.iter().map(LevelState::new).collect(),
            head: spectra.head.as_ref().map(HeadState::new),
            input: vec![F::ZERO; spectra.input_ring],
            output: vec![F::ZERO; spectra.output_ring],
            pushed: 0,
            spectra,
        }
    }

    /// A convolver over this one's IR, sharing its spectra, in the state of
    /// a freshly built one: nothing of this one's running state is copied.
    /// Allocates the new state (the spectra are not copied).
    #[must_use]
    pub fn fresh(&self) -> Self {
        Self::over(Arc::clone(&self.spectra))
    }

    /// The IR's spectra this convolver reads, shared.
    pub fn spectra(&self) -> &Arc<IrSpectra<F>> {
        &self.spectra
    }

    /// [`new`](Self::new) at the default block size (`512`).
    #[must_use]
    pub fn with_ir<S: Sample>(ir: Mono<'_, S>) -> Self {
        Self::new(ir, DEFAULT_BLOCK_SIZE)
    }

    /// IR length in samples.
    pub fn ir_length(&self) -> Samples {
        self.spectra.ir_len
    }

    /// Latency introduced by the partitioned FFT: the block size, or zero for a
    /// [`zero_latency`](Self::zero_latency) convolver.
    pub fn latency(&self) -> Samples {
        if self.spectra.head.is_some() {
            Samples::ZERO
        } else {
            self.spectra.block_size
        }
    }

    /// Produces one convolved sample. Returns `0.0` for the first
    /// [`latency`](Self::latency) samples (fill-up latency).
    #[inline]
    pub fn process_sample(&mut self, input: F) -> F {
        let mut out = [F::ZERO];
        self.process(Mono::new(&[input]), MonoMut::new(&mut out));
        out[0]
    }

    /// Convolve a block: `output[i]` is what `process_sample(input[i])` would
    /// have returned, for every `i`, bit for bit, and the same for any split
    /// of the input into calls. `input` and `output` must be the same length,
    /// which may be anything.
    ///
    /// The call is cut into runs at the frames where a level has a transform
    /// to run, which it does as soon as its frame is reached, so each runs
    /// before the output frame that needs it. A call costs the transforms
    /// whose frames it contains, the share of every level's older partitions
    /// its length is due, and the copy.
    pub fn process(&mut self, input: Mono<'_, F>, mut output: MonoMut<'_, F>) {
        let (input, output) = (input.samples(), output.samples_mut());
        debug_assert_eq!(input.len(), output.len());
        let Self {
            spectra,
            levels,
            input: ring,
            output: out_ring,
            head,
            pushed,
        } = self;
        let start = *pushed;
        let (in_mask, out_mask) = (ring.len() - 1, out_ring.len() - 1);
        let mut done = 0;
        while done < input.len() {
            // The frames to the next transform, or the end of the call.
            let next = levels
                .iter()
                .zip(&spectra.levels)
                .map(|(st, lv)| st.next_frame(lv))
                .min()
                .unwrap_or(u64::MAX);
            let run = (next - *pushed).min((input.len() - done) as u64) as usize;
            let at = *pushed as usize;
            for (r, o) in output[done..done + run].iter_mut().enumerate() {
                let slot = &mut out_ring[at.wrapping_add(r) & out_mask];
                *o = *slot;
                *slot = F::ZERO;
            }
            for (r, &x) in input[done..done + run].iter().enumerate() {
                ring[at.wrapping_add(r) & in_mask] = x;
            }
            *pushed += run as u64;
            done += run;
            for (lv, st) in spectra.levels.iter().zip(levels.iter_mut()) {
                // What is due at this frame, in the order of the frames and,
                // at one frame, a level's forward transform before its
                // inverse and the levels in order: the sums into the output
                // ring add in an order that does not depend on the calls.
                while st.next_frame(lv) <= *pushed {
                    st.crossed |= st.step < lv.forward_steps;
                    st.run_step(lv, ring, out_ring);
                }
                // Spread the older partitions' sum over the block: by the
                // time `horizon` frames have passed since the level's forward
                // transform finished every partition is in. The horizon stops
                // short of the next transform's first step by this call's
                // length (at most half the span), so the call that holds it
                // pays for the transforms and the newest partition only. A
                // call that has run a step does no spread work for the next
                // block: a later call or the next transform does it.
                let older = lv.segments.len() - 1;
                if let (Some(last), true) = (st.spread_from, older > 0 && !st.crossed) {
                    let span = lv.part - (lv.steps[lv.forward_steps - 1] - lv.steps[0]);
                    let horizon = (span - input.len().min(span / 2)).max(1) as u64;
                    let target = older as u64 * (*pushed - last) / horizon;
                    st.accumulate_to(lv, target.min(older as u64) as usize);
                }
            }
        }
        for st in levels.iter_mut() {
            st.crossed = false;
        }
        if let (Some(state), Some(taps)) = (head, &spectra.head) {
            state.process(taps, start, input, output);
        }
    }

    /// Zeros the working buffers without discarding the IR. Safe to
    /// call on a stream discontinuity.
    ///
    /// **Everything** that holds signal has to be cleared: the input ring,
    /// the output ring, and every level's history, sums and overlap. Clearing
    /// only the input leaves the convolution's own partition buffers holding
    /// the previous signal's tail, which then bleeds into whatever plays
    /// next: with an 8-tap IR the first sample after a reset comes back at
    /// 3.5 instead of silence. `reset` is called precisely at stream
    /// discontinuities (a seek, a region change), so that leak lands exactly
    /// where a ghost of the previous audio is most audible.
    pub fn reset(&mut self) {
        self.input.fill(F::ZERO);
        self.output.fill(F::ZERO);
        self.pushed = 0;
        if let Some(head) = &mut self.head {
            head.reset();
        }
        for (lv, st) in self.spectra.levels.iter().zip(&mut self.levels) {
            st.reset(lv);
        }
    }
}

mod head;
mod matrix;
pub use matrix::{MatrixConvolver, MatrixConvolverConfig, MatrixConvolverError};

#[cfg(test)]
mod tests;
