//! [`LevelFollower`]: an audio level, followed by a one-pole attack and release.

use tutti_units::{Ballistics, Frame, SampleRate, Samples};

use super::{CONTROL_GRID, ControlSource, Leg, PlayGrid};

/// Below this the envelope is 0: a release decays to silence in finite time
/// (`ln(1e7) ≈ 16.1` release time constants from full scale, at any `τ`)
/// rather than through subnormals forever, and a silent follower sends
/// nothing.
const FLOOR: f64 = 1e-7;

/// An **envelope follower**: the level its node hears each frame
/// ([`follow`](Self::follow)), followed by a one-pole attack and release, and
/// sent on the [control grid](CONTROL_GRID), one step late.
///
/// - **One-pole attack and release**, per frame:
///   `env ← (1 − c) · env + c · level`, `c = 1 − exp(−1 / (τ · rate))`
///   ([`Ballistics::coeffs`]), the envelope in `f64`, with `τ` the attack
///   while the level is above the envelope and the release otherwise. A `τ` of
///   zero follows the level exactly. Below `1e-7` the envelope is 0, so a
///   release reaches silence in finite time.
/// - **On the grid, one step late.** At a grid point `g` it sends a linear leg
///   from `env(g − step)` to `env(g)` over the step to the next point, where
///   `env(g)` is the envelope after the audio before `g`. So what it sends
///   depends only on the audio up to each grid point, never on where blocks
///   fall. The step's lag is musical delay, not latency.
/// - **A play start** sets the envelope to 0.
#[derive(Clone, Debug)]
pub struct LevelFollower {
    ballistics: Ballistics,
    /// One-pole coefficients at the prepared rate; 0 until `prepare`, so an
    /// unprepared follower's envelope stays at 0.
    attack_coeff: f32,
    release_coeff: f32,
    /// In `f64`, so a long time constant decays at its own rate: in `f32`,
    /// `1 − c` would round to 1 past about 640 s at 48 kHz and the envelope
    /// would never decay.
    env: f64,
    /// `env` at the last grid point: the next leg's start.
    from: f64,
}

impl LevelFollower {
    /// A follower with the attack and release of `ballistics`, at 0. The rate
    /// comes at [`prepare`](ControlSource::prepare).
    #[must_use]
    pub fn new(ballistics: Ballistics) -> Self {
        Self {
            ballistics,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            env: 0.0,
            from: 0.0,
        }
    }

    /// One frame's level into the envelope: at least 0 and not NaN (a node
    /// reads a NaN sample as 0); above 1 reads 1.
    #[inline]
    pub fn follow(&mut self, level: f32) {
        let level = f64::from(level.min(1.0));
        let c = f64::from(if level > self.env {
            self.attack_coeff
        } else {
            self.release_coeff
        });
        let env = (1.0 - c) * self.env + c * level;
        self.env = if env < FLOOR { 0.0 } else { env };
    }
}

impl ControlSource for LevelFollower {
    const GRID: Option<PlayGrid> = Some(CONTROL_GRID);

    fn prepare(&mut self, rate: SampleRate) {
        (self.attack_coeff, self.release_coeff) = self.ballistics.coeffs(rate);
    }

    /// The envelope at 0. It sends nothing of its own here: play frame 0 is a
    /// grid point, and its leg there starts from 0.
    fn start(&mut self) -> Option<Leg> {
        self.env = 0.0;
        self.from = 0.0;
        None
    }

    fn grid(&mut self, _t: Frame, frames: Samples) -> Option<Leg> {
        let leg = Leg::linear(self.from as f32, self.env as f32, frames);
        self.from = self.env;
        Some(leg)
    }
}
