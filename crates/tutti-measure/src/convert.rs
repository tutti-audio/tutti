//! Reading any sample format as `f32` without allocating.

use tutti_buffers::Sample;

/// Samples converted per step of [`for_blocks`].
pub(crate) const BLOCK: usize = 4096;

/// `data` as `f32`, one whole buffer. Allocates: for the batch entry points.
pub(crate) fn to_f32<S: Sample>(data: &[S]) -> Vec<f32> {
    data.iter().map(|s| s.convert_to::<f32>()).collect()
}

/// Whether every sample is finite.
pub(crate) fn all_finite<S: Sample>(data: &[S]) -> bool {
    data.iter().all(|s| s.to_f64().is_finite())
}

/// The length of a run of whole frames of `channels` samples that fits in
/// `capacity` samples, at least one frame.
pub(crate) fn run_len(capacity: usize, channels: usize) -> usize {
    let channels = channels.max(1);
    (capacity / channels).max(1) * channels
}

/// Calls `f` with `data` converted to `f32`, in runs of whole frames of
/// `channels` samples, none longer than a stack block unless one frame is: a
/// frame wider than the block is converted into a heap buffer of its own width,
/// so a run is never less than a frame.
pub(crate) fn for_blocks<S: Sample>(data: &[S], channels: usize, mut f: impl FnMut(&[f32])) {
    let channels = channels.max(1);
    let run = run_len(BLOCK, channels);
    let mut stack = [0.0f32; BLOCK];
    let mut wide = Vec::new();
    let buffer: &mut [f32] = if run > BLOCK {
        wide.resize(run, 0.0);
        &mut wide
    } else {
        &mut stack
    };
    for part in data.chunks(run) {
        let out = &mut buffer[..part.len()];
        for (slot, s) in out.iter_mut().zip(part) {
            *slot = s.convert_to::<f32>();
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
mod tests {
    use super::*;

    /// Every run is whole frames and the runs, joined, are the data converted in
    /// order: for odd and even widths over a block, and for a frame wider than
    /// the stack block. A run cut off a frame boundary fails the first check.
    #[test]
    fn runs_are_whole_frames_that_join_to_the_converted_data() {
        for channels in [1usize, 3, 6, BLOCK + 3] {
            let frames = if channels > BLOCK { 3 } else { 5000 };
            let data: Vec<i16> = (0..channels * frames)
                .map(|i| (i % 30_000) as i16)
                .collect();
            let mut joined = Vec::new();
            for_blocks(&data, channels, |run| {
                assert_eq!(run.len() % channels, 0, "{channels} channels");
                assert!(!run.is_empty());
                joined.extend_from_slice(run);
            });
            let want: Vec<f32> = data.iter().map(|s| s.convert_to::<f32>()).collect();
            assert_eq!(joined, want, "{channels} channels");
        }
    }
}
