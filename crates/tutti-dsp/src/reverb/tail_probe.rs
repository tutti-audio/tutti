//! Test support for the reverbs' `tail_of`: the stimuli a ring-out is measured from and the
//! check that a reported tail covers it.
//!
//! The reference is the render's **own peak**: [`TAIL_FLOOR`] is the reverberation time's -60 dB,
//! "floor below what it carried" ([`System::tail`](crate::System::tail)), and what a reverb
//! carries is its own response, not the input (a bank scales its input, and a loop builds a
//! step up far past it). A frame at or past the tail is under the floor when it is at most
//! `TAIL_FLOOR` of the largest absolute sample of the whole render, every channel.

use tutti_buffers::Float;
use tutti_fixtures::signal::{impulse, peak};
use tutti_units::Samples;

use crate::TAIL_FLOOR;

/// The seed of the full-scale noise burst.
const SEED: u32 = 0x7a11;

/// The inputs a ring-out is measured after, each ending at its last frame and followed by
/// silence: a unit impulse, `burst` frames of full-scale noise in `[-1, 1)` (which fills every
/// delay line with energy of every phase) and `burst` frames of a full-scale step (every line
/// gets the same sign at DC, the loop gain's highest, and the response rises monotonically so
/// it stops at its peak). A burst longer than the longest loop is the caller's.
pub(crate) fn stimuli<F: Float>(burst: usize) -> [(&'static str, Vec<F>); 3] {
    [
        ("impulse", impulse(Samples::new(1))),
        ("noise burst", tutti_fixtures::noise(SEED, burst)),
        ("full-scale step", vec![F::ONE; burst]),
    ]
}

/// The frames to render for `input_frames` of input and a reported `tail`: the input, the
/// tail, and then `period` more or a fifth of the tail, whichever is longer, to see a violation.
pub(crate) fn run_length(input_frames: usize, tail: usize, period: usize) -> usize {
    input_frames + tail + period.max(tail / 5)
}

/// `input` followed by silence to `frames`.
pub(crate) fn padded<F: Float>(input: &[F], frames: usize) -> Vec<F> {
    let mut x = input.to_vec();
    x.resize(frames, F::ZERO);
    x
}

/// Asserts that every frame of `channels` at and after `tail` frames past the last of
/// `input_frames` of input is at most [`TAIL_FLOOR`] of the render's peak.
///
/// # Panics
///
/// When the render is silent (it would prove nothing), or a frame at or past the tail is above
/// the floor or not a number.
pub(crate) fn assert_tail_covers<F: Float>(
    what: &str,
    input_frames: usize,
    tail: usize,
    channels: &[&[F]],
) {
    let top = channels.iter().map(|c| peak(c)).fold(F::ZERO, F::max);
    let top = top.to_f64();
    assert!(top > 0.0, "{what}: the render is silent");
    let floor = TAIL_FLOOR.to_amplitude().get() * top;
    let from = input_frames - 1 + tail;
    let above = |y: &F| y.abs().to_f64().is_nan() || y.abs().to_f64() > floor;
    let last = channels
        .iter()
        .filter_map(|c| c.iter().rposition(above))
        .max();
    assert!(
        last.is_none_or(|l| l < from),
        "{what}: reported tail {tail} frames after {input_frames} of input (frame {from}), but frame {} is above the floor ({floor:e} of a peak {top:e})",
        last.unwrap_or(0)
    );
}

/// Whether some frame of `channels` at or after `after` frames past the last of
/// `input_frames` of input is above [`TAIL_FLOOR`] of the render's peak: the ring-out outlasts
/// `after`, which is what a tail reported as unbounded claims.
///
/// # Panics
///
/// When the render is silent or ends before that frame.
pub(crate) fn rings_past<F: Float>(
    what: &str,
    input_frames: usize,
    after: usize,
    channels: &[&[F]],
) -> bool {
    let top = channels.iter().map(|c| peak(c)).fold(F::ZERO, F::max);
    let top = top.to_f64();
    assert!(top > 0.0, "{what}: the render is silent");
    let floor = TAIL_FLOOR.to_amplitude().get() * top;
    let from = input_frames - 1 + after;
    assert!(
        channels.iter().all(|c| c.len() > from),
        "{what}: the render ends before frame {from}"
    );
    channels
        .iter()
        .any(|c| c[from..].iter().any(|y| y.abs().to_f64() > floor))
}

/// What a node's tail clock reports when the settings move to a longer
/// tail `current` frames after the last input, while `owed` of the old ring-out is left: the
/// longer of the two, measured from the last input frame, held under the bank's `ceiling`.
pub(crate) fn raised_tail(owed: usize, current: usize, ceiling: usize) -> usize {
    owed.max(current).min(ceiling)
}

/// [`assert_tail_covers`] over a render that ran in two parts (the settings moved between them):
/// `first` and `second` are the channels each part wrote, joined end to end.
pub(crate) fn assert_raised_tail_covers<F: Float>(
    what: &str,
    input_frames: usize,
    tail: usize,
    first: &[&[F]],
    second: &[&[F]],
) {
    let joined: Vec<Vec<F>> = first
        .iter()
        .zip(second)
        .map(|(a, b)| [*a, *b].concat())
        .collect();
    let refs: Vec<&[F]> = joined.iter().map(Vec::as_slice).collect();
    assert_tail_covers(what, input_frames, tail, &refs)
}
