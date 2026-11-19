//! Reading any sample format as `f64` without allocating.

use tutti_buffers::Sample;

/// Samples converted per step of [`for_blocks`].
pub(crate) const BLOCK: usize = 4096;

/// `data` as `f64`, one whole buffer. Allocates: for the batch entry points.
pub(crate) fn widen<S: Sample>(data: &[S]) -> Vec<f64> {
    data.iter().map(|s| s.convert_to::<f64>()).collect()
}

/// Whether every sample is finite.
pub(crate) fn all_finite<S: Sample>(data: &[S]) -> bool {
    data.iter().all(|s| s.convert_to::<f64>().is_finite())
}

/// The length of a run of whole frames of `channels` samples that fits in
/// `capacity` samples, at least one frame.
pub(crate) fn run_len(capacity: usize, channels: usize) -> usize {
    let channels = channels.max(1);
    (capacity / channels).max(1) * channels
}

/// Calls `f` with `data` converted to `f64`, in runs of whole frames of
/// `channels` samples, none longer than a stack block unless one frame is: a
/// frame wider than the block is converted into a heap buffer of its own width,
/// so a run is never less than a frame.
pub(crate) fn for_blocks<S: Sample>(data: &[S], channels: usize, mut f: impl FnMut(&[f64])) {
    let channels = channels.max(1);
    let run = run_len(BLOCK, channels);
    let mut stack = [0.0f64; BLOCK];
    let mut wide = Vec::new();
    let buffer: &mut [f64] = if run > BLOCK {
        wide.resize(run, 0.0);
        &mut wide
    } else {
        &mut stack
    };
    for part in data.chunks(run) {
        let out = &mut buffer[..part.len()];
        for (slot, s) in out.iter_mut().zip(part) {
            *slot = s.convert_to::<f64>();
        }
        f(out);
    }
}

/// An `out` that drops every item: the one-shot entry points feed a meter
/// through it and keep only its output.
pub(crate) struct Discard;

impl<T> Extend<T> for Discard {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        items.into_iter().for_each(drop);
    }
}

#[cfg(test)]
mod tests;
