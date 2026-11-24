//! Test support for a bank's `tail_of` (the reverbs' and the delay's): the stimuli a ring-out
//! is measured from and the check that a reported tail covers it.
//!
//! The reference is the render's **own peak**: [`TAIL_FLOOR`] is the reverberation time's -60 dB,
//! "floor below what it carried" ([`System::tail`](crate::System::tail)), and what a reverb
//! carries is its own response, not the input (a bank scales its input, and a loop builds a
//! step up far past it). A frame at or past the tail is under the floor when it is at most
//! `TAIL_FLOOR` of the largest absolute sample of the whole render, every channel.
//!
//! The peak is taken over the whole render, the input period included, so a quiet tail after a
//! loud build-up is judged against the build-up. That is the audible reference, and it is the
//! lenient one for a step, whose response peaks as the input stops. The impulse and the noise
//! burst, whose responses peak early or at random, keep the check honest, which is why all
//! three stimuli run.

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
/// tail, and then `period` more or a fifth of the tail, whichever is longer. That window past the
/// tail is the probe's limit: a violation later than it is not seen.
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
/// A tail of `T` after a last input frame at index `n - 1` keeps frames `n ..= n - 1 + T`. The
/// check starts at frame `n - 1 + T`, the last frame the tail keeps, so it is one frame stricter
/// than the node's cut, which silences from frame `n + T`.
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

/// What a node's tail clock reports when the settings move to a longer tail while `owed` frames
/// of the old ring-out are left: the larger of `owed` and the new settings' tail `current`,
/// from the last input frame, held under the bank's `ceiling`.
///
/// This restates the rule of the node's tail clock (`TailClock::advance` in
/// `tutti-nodes/src/tail_clock.rs`: the ring is the larger of what is owed and the current
/// settings' tail, under the ceiling, from the last non-silent input frame). tutti-dsp cannot
/// name tutti-nodes, so a change to the clock needs a change here; the node-level test
/// `the_node_reports_a_raised_decay_as_the_larger_of_what_is_owed_and_the_new_tail` checks that
/// the node applies the rule.
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
