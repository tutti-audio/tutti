//! [`Ramp`]: a linear approach to a target in a number of steps, read in closed form;
//! [`Lerp`], the blend a ramped value defines; [`Steps`], how many steps a ramp takes.

use tutti_buffers::Float;

use crate::design::along;

/// A value that can be blended toward another.
///
/// A ramp assumes the space is affine: a blend of two values is a value the reader can
/// use. A coefficient set for which that is not true has no `Lerp`.
pub trait Lerp: Copy + PartialEq + core::fmt::Debug {
    /// The value `k` of `n` steps of the way from `self` to `to`, computed from the two
    /// ends and the step, never accumulated, so step `n` is `to` to the bit. Each type
    /// computes its own `k / n`, in the precision it runs in.
    #[must_use]
    fn lerp(&self, to: &Self, k: u32, n: u32) -> Self;
}

impl Lerp for f32 {
    #[inline]
    fn lerp(&self, to: &f32, k: u32, n: u32) -> f32 {
        crate::lerp(*self, *to, along(k, n))
    }
}

impl Lerp for f64 {
    #[inline]
    fn lerp(&self, to: &f64, k: u32, n: u32) -> f64 {
        crate::lerp(*self, *to, along(k, n))
    }
}

/// How many steps a ramp takes: a number, or from how far it goes at a rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Steps {
    /// This many (`Fixed(0)` lands at once).
    Fixed(u32),
    /// No faster than this much a step: `ceil(distance / rate)` steps, so a length
    /// whose sweep is a pitch shift has its speed bounded whatever the distance.
    ///
    /// The rate is in the ramped value's own units per step (frames of delay per frame, for a
    /// delay), a plain positive finite number: no unit covers a dimensionless slope. A rate
    /// of zero is the slowest ramp, `u32::MAX` steps; one that is negative or NaN lands at
    /// once.
    Rate(f64),
}

/// A linear approach to a target in `n` steps, from where it was when its target was set.
///
/// Step `k` is `from.lerp(to, k, n)`, read in closed form by [`at`](Self::at), so it
/// lands exactly and any step can be read without stepping through the ones before. A
/// new target starts from the value in flight. `n = 0` lands at once.
///
/// A reader steps before it reads: the first frame after a new target reads step 1, so a
/// segment of frames that starts at [`steps_taken`](Self::steps_taken) `= k` reads `at(k + 1)`,
/// `at(k + 2)`, …, and [`advance`](Self::advance) moves `k` past it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ramp<T> {
    pub(crate) from: T,
    pub(crate) to: T,
    pub(crate) k: u32,
    pub(crate) n: u32,
    /// The steps a [`retarget`](Self::retarget) takes.
    pub(crate) steps: u32,
}

impl<T: Lerp> Ramp<T> {
    /// A ramp at rest on `value`, whose [`retarget`](Self::retarget) lands at once (see
    /// [`steps`](Self::steps)).
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            from: value,
            to: value,
            k: 0,
            n: 0,
            steps: 0,
        }
    }

    /// The same ramp, taking `steps` steps to each target it is given by
    /// [`retarget`](Self::retarget).
    #[must_use]
    pub fn steps(self, steps: u32) -> Self {
        Self { steps, ..self }
    }

    /// Sets the target to `to`, reached in the ramp's own number of steps
    /// ([`steps`](Self::steps)), from the value in flight.
    pub fn retarget(&mut self, to: T) {
        self.retarget_in(to, self.steps);
    }

    /// Sets the target to `to`, reached in `n` steps from the value in flight.
    pub fn retarget_in(&mut self, to: T, n: u32) {
        *self = Self {
            from: self.now(),
            to,
            k: 0,
            n,
            steps: self.steps,
        };
    }

    /// Jumps to the target now.
    pub fn jump_to_target(&mut self) {
        self.from = self.to;
        self.k = self.n;
    }

    /// The value it is moving to.
    #[must_use]
    pub fn target(&self) -> T {
        self.to
    }

    /// Whether steps remain: the last one has not been taken. A ramp whose target is the value
    /// it holds has steps left here, though every one of them reads the target and
    /// [`advance`](Self::advance) takes them all at once.
    #[must_use]
    pub fn moving(&self) -> bool {
        self.k < self.n
    }

    /// The steps taken since the last target was set.
    #[must_use]
    pub fn steps_taken(&self) -> u32 {
        self.k
    }

    /// How many of the next `frames` frames read a blend, each short of the last step;
    /// the frames after them read the target. None when the ramp starts on its target.
    #[must_use]
    pub fn blended_frames(&self, frames: usize) -> usize {
        if self.from == self.to {
            return 0;
        }
        let left = self.n.saturating_sub(self.k).saturating_sub(1);
        usize::try_from(left).map_or(frames, |left| left.min(frames))
    }

    /// The value at step `k`: the target from step `n` on, and at every step when the
    /// ramp starts on its target.
    #[inline]
    #[must_use]
    pub fn at(&self, k: u32) -> T {
        if k >= self.n || self.from == self.to {
            self.to
        } else {
            self.from.lerp(&self.to, k, self.n)
        }
    }

    /// The value at the current step.
    #[inline]
    #[must_use]
    pub fn now(&self) -> T {
        self.at(self.k)
    }

    /// Takes `frames` steps (at most to the last).
    #[inline]
    pub fn advance(&mut self, frames: u32) {
        self.k = if self.from == self.to {
            self.n
        } else {
            self.k.saturating_add(frames).min(self.n)
        };
    }
}

impl<F: Float + Lerp> Ramp<F> {
    /// Sets the target to `to`, reached in `steps`. A target already set leaves the ramp as it is
    /// (a value sent again every block does not stretch it).
    pub fn retarget_by(&mut self, to: F, steps: Steps) {
        if to == self.to {
            return;
        }
        let n = match steps {
            Steps::Fixed(n) => n,
            Steps::Rate(rate) => {
                let steps: f64 = ((to - self.now()).abs() / F::from_design(rate))
                    .ceil()
                    .into();
                // `as` saturates at `u32::MAX`, so a rate of zero is the slowest ramp.
                steps as u32
            }
        };
        self.retarget_in(to, n);
    }
}

#[cfg(test)]
mod tests;
