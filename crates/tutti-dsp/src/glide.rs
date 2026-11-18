//! [`Glide`]: a linear approach to a target in a number of steps, read in closed form;
//! [`Lerp`], the blend a glided value defines; [`Steps`], how many steps a glide takes.

use tutti_buffers::Float;

/// A value that can be blended toward another.
///
/// A glide assumes the space is affine: a blend of two values is a value the reader can
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
        crate::lerp(*self, *to, k as f32 / n as f32)
    }
}

impl Lerp for f64 {
    #[inline]
    fn lerp(&self, to: &f64, k: u32, n: u32) -> f64 {
        crate::lerp(*self, *to, f64::from(k) / f64::from(n))
    }
}

/// How many steps a glide takes: a number, or from how far it goes at a rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Steps {
    /// This many (`Fixed(0)` lands at once).
    Fixed(u32),
    /// No faster than this much a step: `ceil(distance / rate)` steps, so a length
    /// whose sweep is a pitch shift has its speed bounded whatever the distance.
    Rate(f32),
}

/// A linear approach to a target in `n` steps, from where it was when it was aimed.
///
/// Step `k` is `from.lerp(to, k, n)`, read in closed form by [`at`](Self::at), so it
/// lands exactly and any step can be read without stepping through the ones before. A
/// new aim starts from the value in flight. `n = 0` lands at once.
///
/// A reader steps before it reads: the first frame after an aim reads step 1, so a
/// segment of frames that starts at [`taken`](Self::taken) `= k` reads `at(k + 1)`,
/// `at(k + 2)`, …, and [`advance`](Self::advance) moves `k` past it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glide<T: Lerp> {
    pub(crate) from: T,
    pub(crate) to: T,
    pub(crate) k: u32,
    pub(crate) n: u32,
    /// The steps an aim by [`aim`](Self::aim) takes.
    pub(crate) steps: u32,
}

impl<T: Lerp> Glide<T> {
    /// A glide at rest on `value`, whose [`aim`](Self::aim) lands at once (see
    /// [`by`](Self::by)).
    #[must_use]
    pub fn landed(value: T) -> Self {
        Self {
            from: value,
            to: value,
            k: 0,
            n: 0,
            steps: 0,
        }
    }

    /// The same glide, taking `steps` steps to each target it is [aimed](Self::aim) at.
    #[must_use]
    pub fn by(self, steps: u32) -> Self {
        Self { steps, ..self }
    }

    /// Aims at `to` in the glide's own number of steps ([`by`](Self::by)), from the value
    /// in flight.
    pub fn aim(&mut self, to: T) {
        self.aim_in(to, self.steps);
    }

    /// Aims at `to`, arriving in `n` steps from the value in flight.
    pub fn aim_in(&mut self, to: T, n: u32) {
        *self = Self {
            from: self.now(),
            to,
            k: 0,
            n,
            steps: self.steps,
        };
    }

    /// Lands on the target now.
    pub fn settle(&mut self) {
        self.from = self.to;
        self.k = self.n;
    }

    /// The value it is moving to.
    #[must_use]
    pub fn target(&self) -> T {
        self.to
    }

    /// Whether steps remain: the last one has not been taken. A glide aimed at the value
    /// it holds has steps left here, though every one of them reads the target and
    /// [`advance`](Self::advance) takes them all at once.
    #[must_use]
    pub fn moving(&self) -> bool {
        self.k < self.n
    }

    /// The steps taken since the last aim.
    #[must_use]
    pub fn taken(&self) -> u32 {
        self.k
    }

    /// How many of the next `frames` frames read a blend, each short of the last step;
    /// the frames after them read the target. None when the glide starts on its target.
    #[must_use]
    pub fn blended(&self, frames: usize) -> usize {
        if self.from == self.to {
            return 0;
        }
        let left = self.n.saturating_sub(self.k).saturating_sub(1);
        usize::try_from(left).map_or(frames, |left| left.min(frames))
    }

    /// The value at step `k`: the target from step `n` on, and at every step when the
    /// glide starts on its target.
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

impl<F: Float + Lerp> Glide<F> {
    /// Aims at `to` in `steps`. A target already aimed at leaves the glide as it is (a
    /// value sent again every block does not stretch it).
    pub fn aim_by(&mut self, to: F, steps: Steps) {
        if to == self.to {
            return;
        }
        let n = match steps {
            Steps::Fixed(n) => n,
            Steps::Rate(rate) => ((to - self.now()).abs() / F::from_f32(rate))
                .ceil()
                .to_f64() as u32,
        };
        self.aim_in(to, n);
    }
}

#[cfg(test)]
mod tests;
