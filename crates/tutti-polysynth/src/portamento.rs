//! Portamento: one pitch glide, stepped per sample.
//!
//! Each voice has its own, so a note's glide moves that voice and no other (M1
//! p.16). A glide is a factor on the voice's pitch
//! ([`Portamento::ratio`]), not a pitch of its own, so it composes with a
//! per-note pitch and the channel's bend instead of replacing them.
//!
//! Interpolation is logarithmic, so a glide is even in musical interval rather
//! than in [`Hz`]; the [`Law`] of [`PortamentoConfig::curve`] reshapes only how far along
//! that interval the glide has travelled.

use tutti_curve::Law;
use tutti_units::{FrequencyRatio, Hz, Normalized, SampleRate, Seconds, Semitones};

/// When a new note glides from the previous pitch rather than jumping to its
/// own.
#[derive(Debug, Clone, Copy, PartialEq, Hash, Eq, Default)]
#[non_exhaustive]
pub enum PortamentoMode {
    /// Glide into every note, whether or not the previous one is still held.
    Always,
    /// Glide only when the new note overlaps a sounding one. Pairs with
    /// [`VoiceMode::Legato`](crate::VoiceMode), which is what makes a note
    /// overlap reach the same voice.
    LegatoOnly,
    /// No glide: every note starts at its own pitch. The default, and the value
    /// that makes `retarget` snap rather than ramp.
    #[default]
    Off,
}

/// How long a glide takes: finite and above zero.
///
/// A glide of no time is [`PortamentoMode::Off`], not a time, so zero is not a
/// value this type holds. `new` refuses and never clamps; `of` is for a literal
/// or a constant.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct GlideTime(Seconds);

impl GlideTime {
    /// `time` as a glide time, or `None` when it is NaN, infinite, zero or
    /// negative.
    #[must_use]
    pub const fn new(time: Seconds) -> Option<Self> {
        if time.get() > 0.0 && time.get() <= f64::MAX {
            Some(Self(time))
        } else {
            None
        }
    }

    /// `time` as a glide time, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `time` is NaN, infinite, zero or negative.
    #[must_use]
    #[track_caller]
    pub const fn of(time: Seconds) -> Self {
        match Self::new(time) {
            Some(time) => time,
            None => panic!("a glide time is finite and above zero"),
        }
    }

    /// The time.
    #[must_use]
    pub const fn get(self) -> Seconds {
        self.0
    }
}

/// The refused time of [`GlideTime`]'s `TryFrom<Seconds>`.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("a glide time is finite and above zero, but was {0:?}")]
pub struct GlideTimeError(pub Seconds);

impl TryFrom<Seconds> for GlideTime {
    type Error = GlideTimeError;

    fn try_from(time: Seconds) -> Result<Self, GlideTimeError> {
        Self::new(time).ok_or(GlideTimeError(time))
    }
}

impl From<GlideTime> for Seconds {
    fn from(time: GlideTime) -> Seconds {
        time.0
    }
}

/// How pitch glide behaves between notes.
///
/// A [`PolySynth`](crate::PolySynth) built without a glide has no glide state
/// at all and cannot gain one later.
#[derive(Debug, Clone, PartialEq)]
pub struct PortamentoConfig {
    pub(crate) mode: PortamentoMode,
    pub(crate) curve: Law,
    pub(crate) time: GlideTime,
    pub(crate) constant_time: bool,
}

impl PortamentoConfig {
    /// A glide that is off: [`Off`](PortamentoMode::Off), [`Linear`](Law::Linear), 0.1 s and
    /// constant-time.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mode: PortamentoMode::Off,
            curve: Law::Linear,
            time: GlideTime::of(Seconds::new(0.1)),
            constant_time: true,
        }
    }

    /// When to glide. [`Off`](PortamentoMode::Off) by default, which makes the
    /// other options inert.
    #[must_use]
    pub const fn mode(mut self, mode: PortamentoMode) -> Self {
        self.mode = mode;
        self
    }

    /// The shape of the glide: the [`Law`] of its progress over its duration.
    ///
    /// The pitch itself is interpolated logarithmically (the glide is even in
    /// musical interval, not in [`Hz`]) and every law ends exactly on the
    /// target; the law only reshapes how the progress fraction advances.
    /// [`Linear`](Law::Linear) is the constant rate of interval change;
    /// `PowerIn(2.0)` lingers near the old note and arrives abruptly;
    /// `PowerIn(0.5)` leaves the old note at once and eases into the new one.
    #[must_use]
    pub const fn curve(mut self, curve: Law) -> Self {
        self.curve = curve;
        self
    }

    /// The glide duration, or the *base* duration when
    /// [`constant_time`](Self::constant_time) is false.
    #[must_use]
    pub const fn time(mut self, time: GlideTime) -> Self {
        self.time = time;
        self
    }

    /// Whether every glide takes the same time regardless of how far it travels.
    ///
    /// `true` (the default) is constant-*time*: an octave and a semitone both
    /// take [`time`](Self::time). `false` is constant-*rate*: the duration
    /// scales with the interval in octaves, floored at 10% of `time` so a tiny
    /// interval still glides audibly rather than snapping.
    #[must_use]
    pub const fn constant_time(mut self, constant_time: bool) -> Self {
        self.constant_time = constant_time;
        self
    }
}

impl Default for PortamentoConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// The shortest constant-rate glide, as a fraction of the base time: the
/// interval in octaves is floored here.
const MIN_INTERVAL_OCTAVES: f64 = 0.1;

/// Where a voice's pitch is: at rest, or on its way to a new note.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Glide {
    /// At rest. `current` is the pitch the last step computed, which an
    /// arrival leaves within rounding of `target`, the pitch last glided to.
    Idle { current: Hz, target: Hz },
    /// Between `from` and `to`, `done` frames into a glide `length` frames
    /// long (`done < length`). `current` is the pitch at `done / length` of
    /// the way.
    Gliding {
        from: Hz,
        to: Hz,
        current: Hz,
        done: f64,
        length: f64,
    },
}

#[derive(Debug, Clone)]
pub struct Portamento {
    config: PortamentoConfig,
    glide: Glide,
    sample_rate: SampleRate,
}

impl Portamento {
    /// A glider at rest on `at`.
    pub(crate) fn new(
        config: PortamentoConfig,
        sample_rate: impl Into<SampleRate>,
        at: Hz,
    ) -> Self {
        Self {
            config,
            glide: Glide::Idle {
                current: at,
                target: at,
            },
            sample_rate: sample_rate.into(),
        }
    }

    /// Moves to `freq`: a glide from where it stands when the config's mode asks for one
    /// (`is_legato` says the note overlaps the last), a jump otherwise.
    pub(crate) fn retarget(&mut self, freq: impl Into<Hz>, is_legato: bool) {
        let freq = freq.into();
        let should_glide = match self.config.mode {
            PortamentoMode::Off => false,
            PortamentoMode::Always => true,
            PortamentoMode::LegatoOnly => is_legato,
        };

        if should_glide {
            let from = self.current();

            let glide_time = if self.config.constant_time {
                self.config.time.get().get()
            } else {
                // The interval in octaves; a pitch with no ratio (not above
                // zero) is no interval, not a NaN glide time.
                let interval = FrequencyRatio::new(freq / from).map_or(0.0, |ratio| {
                    (Semitones::from_pitch_ratio(ratio).get() / 12.0).abs()
                });
                self.config.time.get().get() * interval.max(MIN_INTERVAL_OCTAVES)
            };

            // The glide length in frames, fractional: progress is `done / length`.
            let glide_samples = glide_time * self.sample_rate.get();
            let length = if glide_samples > 0.0 {
                glide_samples
            } else {
                1.0
            };
            self.glide = Glide::Gliding {
                from,
                to: freq,
                current: from,
                done: 0.0,
                length,
            };
        } else {
            self.glide = Glide::Idle {
                current: freq,
                target: freq,
            };
        }
    }

    /// Starts a glide from `from` to `to` over `time`, whatever the config's mode:
    /// a glide the MIDI stream asked for (Portamento On/Off and Portamento
    /// Control, which `tutti-voicing` resolves to a source key and a time). A
    /// glide of no time, or between equal pitches, is a jump.
    pub(crate) fn begin(&mut self, from: Hz, to: Hz, time: Seconds) {
        let glide_samples = time.get() * self.sample_rate.get();
        self.glide = if from == to || glide_samples <= 0.0 {
            Glide::Idle {
                current: to,
                target: to,
            }
        } else {
            Glide::Gliding {
                from,
                to,
                current: from,
                done: 0.0,
                length: glide_samples,
            }
        };
    }

    /// The glide's pitch over the pitch it is gliding to: what a voice multiplies
    /// its own pitch (a per-note tuning, the bend) by. 1 once it has arrived.
    #[inline]
    pub(crate) fn ratio(&self) -> f64 {
        match self.glide {
            Glide::Idle { .. } => 1.0,
            Glide::Gliding { to, current, .. } => current / to,
        }
    }

    /// One frame of the glide: the reference [`advance`](Self::advance) is
    /// pinned against (`advance_is_that_many_ticks`).
    #[cfg(test)]
    pub(crate) fn tick(&mut self) -> Hz {
        match self.glide {
            Glide::Idle { target, .. } => target,
            Glide::Gliding {
                from,
                to,
                done,
                length,
                ..
            } => {
                let done = done + 1.0;
                let current = glide_at(self.config.curve, from, to, done / length);
                self.glide = Self::after_step(from, to, current, done, length);
                current
            }
        }
    }

    /// Advance the glide `frames` frames and return the pitch it reaches, as
    /// `frames` one-frame steps would (`tick`, the tests' reference).
    ///
    /// Steps the progress per frame exactly as `tick` does, but interpolates
    /// the pitch once, at the progress the last computing tick reached: the
    /// two logarithms and the exponential a tick evaluates are kept only by
    /// the tick after them, so the intermediate ones are dead work.
    pub(crate) fn advance(&mut self, frames: usize) -> Hz {
        let Glide::Gliding {
            from,
            to,
            mut current,
            mut done,
            length,
        } = self.glide
        else {
            return self.target();
        };
        let mut stepped = false;
        let mut arrived_early = false;
        for _ in 0..frames {
            if done >= length {
                // `tick` returns the target here, and every later tick too.
                arrived_early = true;
                break;
            }
            done += 1.0;
            stepped = true;
        }
        if stepped {
            current = glide_at(self.config.curve, from, to, done / length);
        }
        self.glide = Self::after_step(from, to, current, done, length);
        if arrived_early { to } else { current }
    }

    /// The state after a step that left the glide `done` frames into `length`.
    fn after_step(from: Hz, to: Hz, current: Hz, done: f64, length: f64) -> Glide {
        if done >= length {
            Glide::Idle {
                current,
                target: to,
            }
        } else {
            Glide::Gliding {
                from,
                to,
                current,
                done,
                length,
            }
        }
    }

    /// The pitch the glide is at.
    #[inline]
    pub(crate) fn current(&self) -> Hz {
        match self.glide {
            Glide::Idle { current, .. } | Glide::Gliding { current, .. } => current,
        }
    }

    /// The pitch the glide is on its way to, or last arrived at.
    #[inline]
    fn target(&self) -> Hz {
        match self.glide {
            Glide::Idle { target, .. } => target,
            Glide::Gliding { to, .. } => to,
        }
    }

    #[inline]
    pub(crate) fn is_gliding(&self) -> bool {
        matches!(self.glide, Glide::Gliding { .. })
    }

    pub(crate) fn reset(&mut self, freq: impl Into<Hz>) {
        let freq = freq.into();
        self.glide = Glide::Idle {
            current: freq,
            target: freq,
        };
    }

    pub(crate) fn prepare(&mut self, sample_rate: impl Into<SampleRate>) {
        self.sample_rate = sample_rate.into();
    }
}

/// The pitch at `progress` along the glide from `from` to `to`, through
/// `curve`.
#[inline]
fn glide_at(curve: Law, from: Hz, to: Hz, progress: f64) -> Hz {
    let t = curve.eval(Normalized::clamped(progress));
    // Equal steps in pitch: `from` moved `t` of the way along its interval to `to`.
    // A pitch with no ratio (not above zero) stays at `from`.
    let interval =
        FrequencyRatio::new(to / from).map_or(Semitones::UNISON, Semitones::from_pitch_ratio);
    from * (interval * t.get()).to_pitch_ratio()
}

#[cfg(test)]
mod tests;
