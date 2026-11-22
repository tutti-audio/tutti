//! [`Distance`]: a source at a distance, heard through the time its sound takes, the level
//! it loses and the highs the air takes.
//!
//! A source at distance `r` is the input delayed by `r / c` (the speed of sound `c`), scaled
//! by `r_ref / r` for the reference distance `r_ref` (the inverse-distance law: 6 dB a
//! doubling), with `r` held at `r_ref` or more for the level and the absorption, so a source
//! nearer than the reference is not boosted.
//!
//! **Air absorption** is optional: a one-pole low-pass whose cutoff at distance `r` is
//! `fc_ref * r_ref / r`, so it falls in the proportion the level does and is the cutoff
//! `fc_ref` at the reference distance. Its DC gain is 1.
//!
//! **Doppler is the delay's own motion.** The distance is the path length of the sound that
//! arrives now, and the delay follows it, so a distance that grows at `u` metres a second
//! reads the input at `1 - u / c` of its rate and a sine of `f` comes out at `f (1 - u / c)`.
//! A source receding at `v` along the line of sight, whose distance at emission is
//! `r0 + v t`, has `u = v c / (c + v)` at the listener, which is the textbook shift
//! `f c / (c + v)`; one approaching at `v` has `u = -v c / (c - v)` and shifts to
//! `f c / (c - v)`. The delay moves by at most [`DELAY_SLEW`] frames a frame, so a jump in
//! distance is a bounded glide (a pitch bend of about two semitones), never a click; a
//! distance that changes faster than `DELAY_SLEW` frames of delay a frame is followed at
//! that rate.
//!
//! The delay is musical, not processing latency: the node reports none.

use crate::BlockMismatch;
use crate::block::expect_widths;
use tutti_buffers::{ChannelLayout, Float, InputOutput};
use tutti_units::{Amplitude, Hz, Metres, ReadPosition, SampleRate, SampleSpan, Samples, Speed};

use crate::onepole::{cutoff_complement, cutoff_pole, ring_frames};
use crate::{DELAY_SLEW, DelayLine, InterpolationMode, Ramp, TAIL_FLOOR};

/// The frames a change of distance takes at least: a retarget reaches its distance in this
/// many frames, so a distance sent once a block or less is a straight line between them.
pub const DISTANCE_GLIDE: Samples = Samples::new(64);

tutti_units::ranged! {
    /// The distance at which a [`Distance`]'s level is 1: at least 1 mm, since the level is
    /// the reference over the distance.
    ReferenceDistance(Metres), ReferenceDistanceError, "a distance of at least 1 mm",
    |raw| raw.get() >= 0.001
}

impl ReferenceDistance {
    /// 1 m, the default reference.
    pub const DEFAULT: Self = Self(Metres::of(1.0));
}

impl Default for ReferenceDistance {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What a [`Distance`] is built for: the farthest source, the reference, the medium and the
/// air's absorption.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DistanceSpec {
    max: Metres,
    reference: ReferenceDistance,
    speed: Speed,
    absorption: Option<Hz>,
}

impl DistanceSpec {
    /// A distance of up to `max` (it sets the delay line's length), a reference of 1 m, the
    /// speed of sound in air at 20 degrees Celsius and no absorption.
    #[must_use]
    pub fn new(max: Metres) -> Self {
        Self {
            max,
            reference: ReferenceDistance::DEFAULT,
            speed: Speed::SOUND_IN_AIR_20C,
            absorption: None,
        }
    }

    /// The distance at which the level is 1.
    #[must_use]
    pub fn reference(self, reference: ReferenceDistance) -> Self {
        Self { reference, ..self }
    }

    /// The speed of the wave the delay is computed from.
    #[must_use]
    pub fn speed(self, speed: Speed) -> Self {
        Self { speed, ..self }
    }

    /// Air absorption: a low-pass whose cutoff is `cutoff` at the reference distance and
    /// falls as the reference over the distance.
    #[must_use]
    pub fn air_absorption(self, cutoff: Hz) -> Self {
        Self {
            absorption: Some(cutoff),
            ..self
        }
    }

    /// The farthest distance.
    #[must_use]
    pub fn max(&self) -> Metres {
        self.max
    }

    /// The reference distance.
    #[must_use]
    pub fn reference_distance(&self) -> Metres {
        self.reference.get()
    }

    /// The delay of `distance` at `rate`, as a position, held to the farthest distance.
    fn position(&self, distance: Metres, rate: SampleRate) -> ReadPosition {
        let far = self.far(rate);
        ReadPosition::of(distance / self.speed, rate).min(far)
    }

    /// The delay of the farthest distance.
    fn far(&self, rate: SampleRate) -> ReadPosition {
        ReadPosition::of(self.max / self.speed, rate)
    }
}

/// The controls of a [`Distance`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DistanceControls {
    /// How far the source is: the delay `r / c`, and the level `r_ref / r` for `r` of at
    /// least the reference. A distance past the spec's farthest is held there.
    pub distance: Metres,
}

impl DistanceControls {
    /// 1 m, the default reference distance: a delay of about 2.9 ms and a level of 1.
    pub const DEFAULT: Self = Self {
        distance: Metres::of(1.0),
    };
}

/// A source at a distance, of any width: every channel is delayed by the one delay, scaled by
/// the one level and, under absorption, filtered by one cutoff.
///
/// Built on the control side for one rate; nothing it holds grows.
#[derive(Clone, Debug)]
pub struct Distance<F: Float> {
    spec: DistanceSpec,
    rate: SampleRate,
    lines: Vec<DelayLine<F>>,
    /// Each channel's one-pole state.
    lowpass: Vec<F>,
    delay: Ramp<ReadPosition>,
    /// The delay of the reference distance, in frames.
    reference: SampleSpan,
}

impl<F: Float> Distance<F> {
    /// A distance of `width` channels at `rate`, at rest on `controls`, silent. Allocates the
    /// delay lines.
    #[must_use]
    pub fn new(
        spec: DistanceSpec,
        width: ChannelLayout,
        rate: SampleRate,
        controls: &DistanceControls,
    ) -> Self {
        let capacity = Samples::new(spec.far(rate).frames().get().saturating_add(1));
        let width = usize::from(width.count());
        let reference = ReadPosition::of(spec.reference.get() / spec.speed, rate).span();
        Self {
            spec,
            rate,
            lines: (0..width).map(|_| DelayLine::new(capacity)).collect(),
            lowpass: vec![F::ZERO; width],
            delay: Ramp::new(spec.position(controls.distance, rate)),
            reference,
        }
    }

    /// Aims the delay at `controls`: it reaches the new distance in [`DISTANCE_GLIDE`] frames
    /// or, when the distance is far, at [`DELAY_SLEW`] frames of delay a frame. A distance
    /// already aimed at leaves the glide as it is. No allocation.
    pub fn retarget(&mut self, controls: &DistanceControls) {
        let to = self.spec.position(controls.distance, self.rate);
        if to == self.delay.target() {
            return;
        }
        let moved = ReadPosition::from_count(to.get().abs_diff(self.delay.now().get()))
            .span()
            .get();
        let glide = DISTANCE_GLIDE.get() as f64;
        // `as` saturates at `u32::MAX`.
        let steps = (moved / DELAY_SLEW).ceil().max(glide) as u32;
        self.delay.retarget_in(to, steps);
    }

    /// Lands the delay on its target now, with no glide: for a source nobody has heard move,
    /// such as the first distance after a reset. No allocation.
    pub fn land(&mut self) {
        self.delay.jump_to_target();
    }

    /// Silences the lines and the filters and lands the delay on its target. No allocation.
    pub fn reset(&mut self) {
        self.lines.iter_mut().for_each(DelayLine::reset);
        self.lowpass.fill(F::ZERO);
        self.delay.jump_to_target();
    }

    /// The frames the line keeps sounding after its input stops, at `controls`, erring long:
    /// the longer of the delay in flight and the delay aimed at, and the absorption's ring
    /// out to [`TAIL_FLOOR`] at the lowest cutoff of the two.
    #[must_use]
    pub fn tail(&self, controls: &DistanceControls) -> Samples {
        let far = self
            .delay
            .now()
            .max(self.spec.position(controls.distance, self.rate));
        let ring = self.spec.absorption.map_or(0.0, |cutoff| {
            let pole = cutoff_pole(cutoff * level(self.reference, far), self.rate);
            ring_frames(pole, TAIL_FLOOR).ceil()
        });
        // `as` saturates.
        Samples::new(
            far.frames()
                .get()
                .saturating_add(2)
                .saturating_add(ring as usize),
        )
    }

    /// The most [`tail`](Self::tail) is at any distance: the farthest.
    #[must_use]
    pub fn tail_ceiling(&self) -> Samples {
        self.tail(&DistanceControls {
            distance: self.spec.max,
        })
    }

    /// Runs `input` into `output`, channel `c` of one into channel `c` of the other. Frame `i`
    /// of the block reads the delay's step after the steps taken. No allocation.
    ///
    /// # Errors
    ///
    /// [`BlockMismatch`] when the block's input or output is not the bank's width in channels.
    pub fn process(&mut self, block: &mut InputOutput<'_, F>) -> Result<(), BlockMismatch> {
        let width = self.lines.len();
        expect_widths(block, width, width)?;
        let frames = block.frames().get();
        let (input, mut output) = block.split();
        let (ramp, reference) = (self.delay, self.reference);
        let step = |i: usize| {
            ramp.steps_taken()
                .saturating_add(1)
                .saturating_add(u32::try_from(i).unwrap_or(u32::MAX))
        };
        let delay_at = |i: usize| ramp.at(step(i));
        // The absorption's coefficient crosses into the data's float once per block, at the
        // distance of the block's last frame.
        let coefficient = self.spec.absorption.filter(|_| frames > 0).map(|cutoff| {
            let at = cutoff * level(reference, delay_at(frames - 1));
            F::from_design(cutoff_complement(at, self.rate))
        });
        for c in 0..width {
            let x = &input.plane(c).samples()[..frames];
            let mut plane = output.plane_mut(c);
            let y = &mut plane.samples_mut()[..frames];
            let (line, state) = (&mut self.lines[c], &mut self.lowpass[c]);
            for (i, (y, &x)) in y.iter_mut().zip(x).enumerate() {
                line.push_sample(x);
                let delay = delay_at(i);
                let mut heard = line.read_sample(line.held(delay), InterpolationMode::Linear);
                if let Some(k) = coefficient {
                    *state += k * (heard - *state);
                    heard = *state;
                }
                *y = heard * F::from_unit(Amplitude::new(level(reference, delay)));
            }
        }
        self.delay
            .advance(u32::try_from(frames).unwrap_or(u32::MAX));
        Ok(())
    }
}

/// The level of a delay: `r_ref / r` past the reference delay, 1 within it.
fn level(reference: SampleSpan, delay: ReadPosition) -> f64 {
    let span = delay.span();
    if span <= reference {
        1.0
    } else {
        reference / span
    }
}

#[cfg(test)]
mod tests;
