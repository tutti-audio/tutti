// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: the comb and all-pass
// filters are rustysynth's reverb.rs (a Freeverb), moved here as pure algorithms over
// slices.

//! [`FreeverbBank`]: Freeverb's combs and all-passes over buffers views, with the two
//! coefficients that move as [`Ramp`]s ([`FreeverbRamps`]).
//!
//! Per channel eight parallel damped comb filters then four all-pass filters, fed by one mono
//! input and giving a stereo wet signal. The right channel's lines are 23 frames longer than
//! the left's at 44.1 kHz (the stereo spread), and the tunings are scaled to the rate. The
//! input is scaled by a fixed gain ([`FIXED_GAIN`](FreeverbBank::FIXED_GAIN)), so a caller feeds
//! it at the level it wants wet.
//!
//! A **comb** of `L` frames, feedback `g` and damping `d` is the loop `v[n] = x[n] + g s[n]`
//! with `s` the one-pole `s[n] = (1 - d) y[n] + d s[n - 1]` of the comb's output
//! `y[n] = v[n - L]`: an impulse comes back at `L` and then, for echo `k >= 2`, at `kL` through
//! `k - 1` passes of the one-pole, each pass scaled by `g`. An **all-pass** of `D` frames and
//! feedback `a` is Freeverb's own `H(z) = ((1 + a) z^-D - 1) / (1 - a z^-D)` (not the
//! unit-magnitude lattice): `-1` at frame 0 and `a^(k-1)` at `kD`.
//!
//! **Chunks.** The render counts [`CHUNK`](FreeverbBank::CHUNK) frames from the last reset
//! and works once at each chunk's start: the two [`Ramp`]s take a step and the combs read
//! their coefficients from them. The chunks are counted from the first frame since the last
//! reset, not from a call's edge, so the output does not depend on how the input is cut into
//! calls. The design is not the bank's: the caller [retargets](FreeverbBank::retarget) the ramps
//! when its controls move, on a grid of its own. They move in 20 ms of whole chunks, landing
//! exactly (a size or a damping that jumped would click).
//!
//! **Format.** The state and the views are the bank's float `F`. The feedback and the damping
//! cross to `F` once per `retarget`, from the design in `f64`. Neither is stored as a complement:
//! the damping pole is at most 0.4 (its complement is at least 0.6), and the comb loop gain is at
//! most 0.98, so `1 - g` is at least 0.02 and `F`'s rounding of `g` (3e-8 in `f32`) moves it, and
//! the RT60 with it, by 1.5e-6 of itself; a loop gain multiplies a line's sample, which rounds by
//! about `u` a trip whatever the form.

use core::num::NonZeroUsize;
use tutti_buffers::{Float, Planes, PlanesMut};
use tutti_units::{Feedback, Gain, Mix, SampleRate, Seconds, Tail};

use super::design::FreeverbControls;
use crate::delay::{ring_seconds, trips};
use crate::reverb::{CHUNK, ramp};
use crate::{AllPassFilter, AllPassForm, CombFilter, CombForm, Lerp, Ramp, Steps, TAIL_FLOOR};

/// The all-pass feedback.
const ALLPASS_FEEDBACK: Feedback = Feedback::new(0.5);
/// How many frames longer the right channel's lines are, at 44.1 kHz.
const STEREO_SPREAD: usize = 23;
/// The comb lengths of the left channel at 44.1 kHz.
const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
/// The all-pass lengths of the left channel at 44.1 kHz.
const ALL_PASSES: [usize; 4] = [556, 441, 341, 225];

/// The longest line, in frames. No device rate reaches it (768 kHz needs 28,560 for the longest
/// comb); a rate no device has saturates here, so a prepare cannot allocate without bound.
const MAX_LINE: usize = 1 << 16;

/// A tuning scaled to `rate`, in whole frames: at least one (a line of none would never
/// advance) and at most [`MAX_LINE`].
fn scale_tuning(rate: SampleRate, tuning: usize) -> NonZeroUsize {
    let frames = ((rate.get() / 44100_f64 * (tuning as f64)).round() as usize).min(MAX_LINE);
    NonZeroUsize::new(frames).unwrap_or(NonZeroUsize::MIN)
}

/// The comb loop gain and damping Freeverb starts at: the General MIDI figures.
const START: FreeverbControls = FreeverbControls::GENERAL_MIDI;

/// The comb loop gain and damping of `controls`, each crossed to `F`.
fn design<F: Float>(controls: &FreeverbControls) -> (F, F) {
    (
        F::from_unit(controls.feedback()),
        F::from_design(controls.damping()),
    )
}

/// What a Freeverb reads while it runs: the comb feedback and the damping, each retargeted by
/// [`FreeverbBank::retarget`] and stepped once a chunk by [`FreeverbBank::process`].
///
/// A ramp steps before its chunk runs: the first chunk after a `retarget` reads step 1. Held in the
/// bank's float `F`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FreeverbRamps<F: Float> {
    feedback: Ramp<F>,
    damp: Ramp<F>,
    /// The steps of the 20 ms ramp, at the rate prepared for.
    ramp: Steps,
}

impl<F: Float + Lerp> FreeverbRamps<F> {
    /// Jumps to the targets now.
    pub fn jump_to_target(&mut self) {
        self.feedback.jump_to_target();
        self.damp.jump_to_target();
    }

    /// Takes one step of each ramp.
    fn step(&mut self) {
        self.feedback.advance(1);
        self.damp.advance(1);
    }
}

/// Freeverb at one rate: mono in, stereo out, all in the state float `F`.
///
/// Built on the control side for one rate, where it allocates the lines; a clone keeps their
/// size, and [`reset`](Self::reset) zeroes them without allocating. Its comb and all-pass
/// lengths are Freeverb's, tuned at 44.1 kHz and scaled to the rate as given.
#[derive(Clone, Debug)]
pub struct FreeverbBank<F: Float> {
    combs: [Vec<CombFilter<F>>; 2],
    all_passes: [Vec<AllPassFilter<F>>; 2],
    /// The rate the lines were tuned for.
    rate: SampleRate,
    /// Frames of the current chunk already run: 0 at a chunk's start.
    into: usize,
    /// [`FIXED_GAIN`](Self::FIXED_GAIN), crossed once.
    gain: F,
}

impl<F: Float + Lerp> FreeverbBank<F> {
    /// The gain the input is scaled by.
    pub const FIXED_GAIN: Gain = Gain::new(0.015);

    /// The frames between ramp steps: the chunk. A caller that designs on a grid of its own
    /// can align it here, so a new target lands on a chunk's start.
    pub const CHUNK: usize = CHUNK;

    /// A silent Freeverb at `rate`, as given: a fractional rate scales the tunings as it is. Allocates the filter lines: each is
    /// at least 1 frame and at most 65,536 (a rate no device has saturates).
    #[must_use]
    pub fn new(rate: SampleRate) -> Self {
        let side = |spread: usize| {
            (
                COMBS
                    .iter()
                    .map(|&t| {
                        CombFilter::new(
                            CombForm::Feedback,
                            scale_tuning(rate, t + spread),
                            Feedback::NONE,
                        )
                    })
                    .collect::<Vec<_>>(),
                ALL_PASSES
                    .iter()
                    .map(|&t| {
                        AllPassFilter::new(
                            AllPassForm::Freeverb,
                            scale_tuning(rate, t + spread),
                            ALLPASS_FEEDBACK,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        };
        let (cl, al) = side(0);
        let (cr, ar) = side(STEREO_SPREAD);
        let mut bank = Self {
            combs: [cl, cr],
            all_passes: [al, ar],
            rate,
            into: 0,
            gain: F::from_unit(Self::FIXED_GAIN),
        };
        let (feedback, damp) = design(&START);
        bank.set_combs(feedback, damp);
        bank
    }

    /// The ramps of this bank, at rest on the General MIDI figures: its first
    /// [`retarget`](Self::retarget) is the caller's to land.
    #[must_use]
    pub fn ramps(&self) -> FreeverbRamps<F> {
        let (feedback, damp) = design(&START);
        FreeverbRamps {
            feedback: Ramp::new(feedback),
            damp: Ramp::new(damp),
            ramp: ramp(self.rate),
        }
    }

    /// Back to silence, the chunks counted from here. No allocation.
    pub fn reset(&mut self) {
        for cf in self.combs.iter_mut().flatten() {
            cf.reset();
        }
        for apf in self.all_passes.iter_mut().flatten() {
            apf.reset();
        }
        self.into = 0;
    }

    /// Aims `ramps` at the comb loop gain and damping of `controls`, each moving in 20 ms.
    /// No allocation.
    pub fn retarget(&self, ramps: &mut FreeverbRamps<F>, controls: &FreeverbControls) {
        let (feedback, damp) = design(controls);
        ramps.feedback.retarget_by(feedback, ramps.ramp);
        ramps.damp.retarget_by(damp, ramps.ramp);
    }

    /// Sets every comb's loop gain and damping. The damping's complement is `1 - damp`.
    fn set_combs(&mut self, feedback: F, damp: F) {
        for cf in self.combs.iter_mut().flatten() {
            cf.retune(feedback, damp);
        }
    }

    /// How long it rings at its worst: the room at its largest, whatever the damping (the
    /// loop's DC gain is the comb's feedback alone). See [`tail_of`](Self::tail_of).
    #[must_use]
    pub fn tail(&self) -> Tail {
        let room_one = FreeverbControls {
            room: Mix::new(1.0),
            damp: Mix::new(0.0),
        };
        self.tail_of(&room_one)
    }

    /// How long `controls` ring after the input stops, to [`TAIL_FLOOR`], erring long. Each
    /// trip round the longest comb keeps the loop gain's fraction of what it carried (at the
    /// lowest frequency the damping leaves it: the loop's DC gain), so falling to the floor
    /// takes the [Schroeder RT60](crate::rt60_from_gain)'s trips, and the all-pass chain adds
    /// its own echoes: each stage's `1 + trips` periods of the all-pass feedback. Not
    /// followed by the damping: damping only shortens it.
    #[must_use]
    pub fn tail_of(&self, controls: &FreeverbControls) -> Tail {
        self.ring(controls.feedback())
    }

    fn ring(&self, loop_gain: Feedback) -> Tail {
        let longest = self.combs[1]
            .iter()
            .map(|c| c.delay().get())
            .max()
            .unwrap_or(0);
        let comb = ring_seconds(longest, trips(loop_gain.get(), TAIL_FLOOR), self.rate);
        let chain = self.all_passes[1]
            .iter()
            .map(|a| a.delay().get())
            .map(|d| ring_seconds(d, trips(ALLPASS_FEEDBACK.get(), TAIL_FLOOR), self.rate));
        let total = chain.fold(comb, |sum, s| Seconds::new(sum.get() + s.get()));
        Tail::Finite(total.to_samples_ceil(self.rate))
    }

    /// Runs `input` into `output`, the same number of frames: the input's channels averaged
    /// (a mono input is itself) and scaled by [`FIXED_GAIN`](Self::FIXED_GAIN), through the combs
    /// and the all-passes of the left channel into plane 0 and of the right into plane 1. A plane
    /// past the second is not touched, and a view with one plane is its left alone. Each
    /// chunk's ramp step is taken at its first frame ([`CHUNK`](Self::CHUNK)). No
    /// allocation.
    pub fn process(
        &mut self,
        ramps: &mut FreeverbRamps<F>,
        input: Planes<'_, F>,
        mut output: PlanesMut<'_, '_, F>,
    ) {
        let frames = input.frames().get().min(output.frames().get());
        let width = input.channels();
        let scale = F::from_design(1.0 / width.max(1) as f64);
        let mut planes = output.iter_mut();
        let mut left = planes.next().map(|p| &mut p.into_samples_mut()[..frames]);
        let mut right = planes.next().map(|p| &mut p.into_samples_mut()[..frames]);
        let (mut x, mut l, mut r) = ([F::ZERO; CHUNK], [F::ZERO; CHUNK], [F::ZERO; CHUNK]);
        let mut at = 0;
        while at < frames {
            if self.into == 0 {
                ramps.step();
                self.set_combs(ramps.feedback.now(), ramps.damp.now());
            }
            let run = (CHUNK - self.into).min(frames - at);
            x[..run].fill(F::ZERO);
            for c in 0..width {
                let plane = &input.plane(c).samples()[at..at + run];
                for (slot, v) in x.iter_mut().zip(plane) {
                    *slot += *v;
                }
            }
            for slot in &mut x[..run] {
                *slot *= scale;
            }
            self.run(&x[..run], &mut l[..run], &mut r[..run]);
            if let Some(y) = left.as_deref_mut() {
                y[at..at + run].copy_from_slice(&l[..run]);
            }
            if let Some(y) = right.as_deref_mut() {
                y[at..at + run].copy_from_slice(&r[..run]);
            }
            self.into = (self.into + run) % CHUNK;
            at += run;
        }
    }

    /// One run of a chunk: `input` scaled by [`FIXED_GAIN`](Self::FIXED_GAIN) through each
    /// channel's combs and all-passes into `left` and `right`, which are overwritten.
    fn run(&mut self, input: &[F], left: &mut [F], right: &mut [F]) {
        for (out, (combs, all_passes)) in [left, right]
            .into_iter()
            .zip(self.combs.iter_mut().zip(self.all_passes.iter_mut()))
        {
            out.fill(F::ZERO);
            for cf in combs {
                for (o, &x) in out.iter_mut().zip(input) {
                    *o += cf.tick(x * self.gain);
                }
            }
            for apf in all_passes {
                apf.process(out);
            }
        }
    }
}

#[cfg(test)]
mod tests;
