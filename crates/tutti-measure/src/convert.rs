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

/// Calls `f` with `data` converted to `f32`, in runs of whole frames of
/// `channels` samples, none longer than a stack block. Allocates nothing.
pub(crate) fn for_blocks<S: Sample>(data: &[S], channels: usize, mut f: impl FnMut(&[f32])) {
    let run = (BLOCK / channels.max(1)).max(1) * channels.max(1);
    let mut buffer = [0.0f32; BLOCK];
    for part in data.chunks(run) {
        let out = &mut buffer[..part.len()];
        for (slot, s) in out.iter_mut().zip(part) {
            *slot = s.convert_to::<f32>();
        }
        f(out);
    }
}
