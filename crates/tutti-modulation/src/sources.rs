//! Builders for the built-in sources: an [`Lfo`] and a [`Macro`] converting into the
//! [`TimeSource`] the compiler reads, and a [`NoteEnvelope`] (valid stages) and a
//! `Ballistics` converting into the [`SignalKind`] it reads.
//!
//! These are the source half only. A follower's and a note envelope's input
//! (the audio or the MIDI that feeds their node) is the host's to wire, keyed
//! by whatever the host keys its nodes by, so it is not here.

use tutti_units::{Ballistics, BeatsPerCycle, Hz, PhaseIncrement};

use crate::{LayerClock, LfoShape, NoteEnvelope, SignalKind, TimeSource, Waveform};

/// How an [`Lfo`] keeps time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LfoRate {
    /// One cycle per this many beats: bar-locked, follows seeks, holds while
    /// the transport is stopped.
    Beats(BeatsPerCycle),
    /// Cycles per second on the graph's play clock, running while stopped.
    Hz(Hz),
}

impl From<BeatsPerCycle> for LfoRate {
    /// One cycle per this many beats.
    fn from(per_cycle: BeatsPerCycle) -> Self {
        Self::Beats(per_cycle)
    }
}

impl From<Hz> for LfoRate {
    /// This many cycles per second.
    fn from(rate: Hz) -> Self {
        Self::Hz(rate)
    }
}

/// A low-frequency oscillator: a closed-form wave on a clock, no node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lfo {
    /// The wave.
    pub shape: LfoShape,
    /// The random shapes' seed; ignored by the others.
    pub seed: u64,
    /// Its clock.
    pub rate: LfoRate,
    /// Added to the cycle position.
    pub phase: PhaseIncrement,
}

impl Lfo {
    /// `shape` at one cycle per second.
    #[must_use]
    pub const fn new(shape: LfoShape) -> Self {
        Self {
            shape,
            seed: 0,
            rate: LfoRate::Hz(Hz::new(1.0)),
            phase: PhaseIncrement::new(0.0),
        }
    }

    /// A sine.
    pub const fn sine() -> Self {
        Self::new(LfoShape::Sine)
    }

    /// A triangle.
    pub const fn triangle() -> Self {
        Self::new(LfoShape::Triangle)
    }

    /// A square.
    pub const fn square() -> Self {
        Self::new(LfoShape::Square)
    }

    /// A rising saw.
    pub const fn saw() -> Self {
        Self::new(LfoShape::Sawtooth)
    }

    /// Its rate: a [`BeatsPerCycle`] (one cycle per that many beats, bar-locked)
    /// or an `Hz` (cycles per second): `lfo.rate(BeatsPerCycle::of(4.0))`,
    /// `lfo.rate(Hz::new(0.5))`.
    #[must_use]
    pub fn rate(mut self, rate: impl Into<LfoRate>) -> Self {
        self.rate = rate.into();
        self
    }

    /// Starting `phase` cycles in.
    #[must_use]
    pub const fn phase(mut self, phase: PhaseIncrement) -> Self {
        self.phase = phase;
        self
    }

    /// The random shapes' seed.
    #[must_use]
    pub const fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }
}

impl From<Lfo> for TimeSource {
    /// A [`TimeSource::Lfo`]: the shape and seed as the wave, the rate and phase
    /// as the clock, with no anchor (the compiler's to set).
    fn from(lfo: Lfo) -> Self {
        let clock = match lfo.rate {
            LfoRate::Beats(per_cycle) => LayerClock::Beats {
                per_cycle,
                phase: lfo.phase,
            },
            LfoRate::Hz(rate) => LayerClock::Seconds {
                rate,
                phase: lfo.phase,
                anchor: None,
            },
        };
        TimeSource::Lfo {
            wave: Waveform::Shape {
                shape: lfo.shape,
                seed: lfo.seed,
            },
            clock,
        }
    }
}

/// A constant the user moves: a raw value, saturating past ±1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Macro(pub f64);

impl From<Macro> for TimeSource {
    /// A [`TimeSource::Macro`] at the value.
    fn from(m: Macro) -> Self {
        TimeSource::Macro { value: m.0 }
    }
}

impl From<NoteEnvelope> for SignalKind {
    /// A [`SignalKind::NoteEnvelope`] of the stages.
    fn from(e: NoteEnvelope) -> Self {
        SignalKind::NoteEnvelope(e)
    }
}

impl From<Ballistics> for SignalKind {
    /// A [`SignalKind::Follower`] with the times.
    fn from(b: Ballistics) -> Self {
        SignalKind::Follower(b)
    }
}

#[cfg(test)]
mod tests;
