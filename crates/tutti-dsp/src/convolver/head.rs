//! The direct-form head of a zero-latency [`Convolver`](super::Convolver): the IR's first
//! `B` taps as a time-domain FIR, the taps shared and the running state per convolver.

use crate::FftFloat;

/// The first taps of an IR, stored reversed so that a frame's dot product reads the input
/// ring forwards. Built once with the [`IrSpectra`](super::IrSpectra) that owns it and
/// read-only after that, so every convolver over the IR shares one copy.
#[derive(Debug)]
pub(super) struct HeadTaps<F: FftFloat> {
    /// `h[len - 1], ..., h[0]`.
    pub(super) reversed: Vec<F>,
}

impl<F: FftFloat> HeadTaps<F> {
    /// The head over `taps` (`h[0]` first).
    pub(super) fn new(taps: &[F]) -> Self {
        Self {
            reversed: taps.iter().rev().copied().collect(),
        }
    }

    /// The taps held.
    pub(super) fn len(&self) -> usize {
        self.reversed.len()
    }
}

/// One convolver's input history for its [`HeadTaps`].
#[derive(Clone, Debug)]
pub(super) struct HeadState<F: FftFloat> {
    /// The newest input frames, a power-of-two ring at least as long as the head.
    ring: Vec<F>,
}

impl<F: FftFloat> HeadState<F> {
    /// Silence, sized for `taps`. Allocates.
    pub(super) fn new(taps: &HeadTaps<F>) -> Self {
        Self {
            ring: vec![F::ZERO; taps.len().next_power_of_two()],
        }
    }

    /// Adds the head's output to `output`: `output[i] += sum_k h[k] x[start + i - k]`, for
    /// `input` the frames from absolute frame `start`, with `x` zero before frame 0.
    ///
    /// Each frame's sum is in one fixed order, so the bits do not depend on how the stream is
    /// split into calls. No allocation.
    pub(super) fn process(
        &mut self,
        taps: &HeadTaps<F>,
        start: u64,
        input: &[F],
        output: &mut [F],
    ) {
        let head = taps.reversed.as_slice();
        let (len, mask) = (head.len(), self.ring.len() - 1);
        for (i, (&x, out)) in input.iter().zip(output.iter_mut()).enumerate() {
            let at = (start as usize).wrapping_add(i) & mask;
            self.ring[at] = x;
            // The oldest of the `len` frames, then forwards to the newest at `at`, which wraps
            // the ring at most once: two contiguous runs of taps.
            let oldest = (at + 1).wrapping_sub(len) & mask;
            let first = (self.ring.len() - oldest).min(len);
            let (a, b) = head.split_at(first);
            *out += dot(a, &self.ring[oldest..oldest + first]) + dot(b, &self.ring[..len - first]);
        }
    }

    /// Silences the history.
    pub(super) fn reset(&mut self) {
        self.ring.fill(F::ZERO);
    }
}

/// `sum a[j] * b[j]`, ascending.
#[inline]
fn dot<F: FftFloat>(a: &[F], b: &[F]) -> F {
    a.iter().zip(b).fold(F::ZERO, |acc, (&p, &q)| acc + p * q)
}
