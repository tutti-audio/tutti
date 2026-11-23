//! The controls of the equalizer designs: the matched bell, the tilt and the graphic bands,
//! each with the ranges its controls are read within.

use tutti_units::{Db, Hz, Q, SampleRate};

use super::{GraphicBands, MatchedBell, Sos, SosError, Tilt};
use crate::DecayTime;

/// The lowest center a bell or tilt takes: 20 Hz.
pub const MIN_EQ_CENTER: Hz = Hz::new(20.0);

/// The largest gain magnitude a bell, tilt or graphic band takes: 24 dB.
pub const MAX_EQ_GAIN: Db = Db::new(24.0);

/// The quality factors a bell takes: 0.1 to 30.
const BELL_Q_RANGE: (Q, Q) = (Q::new(0.1), Q::new(30.0));

/// `center` held within [`MIN_EQ_CENTER`] and 0.99 of the Nyquist of `rate`.
fn held_center(center: Hz, rate: SampleRate) -> Hz {
    Hz::new(
        center
            .get()
            .clamp(MIN_EQ_CENTER.get(), rate.nyquist_scaled(0.99).get()),
    )
}

/// `gain` held within [`MAX_EQ_GAIN`].
fn held_gain(gain: Db) -> Db {
    Db::new(gain.get().clamp(-MAX_EQ_GAIN.get(), MAX_EQ_GAIN.get()))
}

/// The controls of a [`MatchedBell`] equalizer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BellFilterControls {
    /// The center, where the gain is reached. Read within 20 Hz and 0.99 of the Nyquist.
    pub frequency: Hz,
    /// The quality factor: the narrower, the higher. Read within 0.1 and 30.
    pub q: Q,
    /// The gain at the center, a boost above 0 dB and a cut below. Read within 24 dB.
    pub gain: Db,
}

impl BellFilterControls {
    /// 1 kHz, [`Q::BUTTERWORTH`] and 0 dB.
    pub const DEFAULT: Self = Self {
        frequency: Hz::new(1_000.0),
        q: Q::BUTTERWORTH,
        gain: Db::UNITY,
    };

    /// These controls held within the ranges they are read in at `rate`.
    #[must_use]
    pub fn held(&self, rate: SampleRate) -> Self {
        Self {
            frequency: held_center(self.frequency, rate),
            q: self.q.clamp(BELL_Q_RANGE.0, BELL_Q_RANGE.1),
            gain: held_gain(self.gain),
        }
    }

    /// The design of these controls as they stand (see [`held`](Self::held)) into `into`: no
    /// allocation.
    ///
    /// # Errors
    ///
    /// [`MatchedBell::design_into`]'s refusal of the controls.
    pub fn design_into(&self, rate: SampleRate, into: &mut Sos) -> Result<(), SosError> {
        MatchedBell::new(self.q, self.gain).design_into(self.frequency, rate, into)
    }

    /// The ring-out of the design at these controls ([`MatchedBell::decay_time`]).
    ///
    /// # Errors
    ///
    /// As [`MatchedBell::decay_time`].
    pub fn decay_time(&self, rate: SampleRate, floor: Db) -> Result<DecayTime, SosError> {
        MatchedBell::new(self.q, self.gain).decay_time(self.frequency, rate, floor)
    }

    /// The ring-out over every control: unbounded. The boost's poles are `exp(-zeta w0)` with
    /// `zeta = 1 / (2 A Q)`, slowest at the lowest center, highest Q and largest boost, but
    /// the cut is the boost's inverse, whose poles are the boost's zeros, and the matched
    /// zeros have no closed-form bound on their radius (the numerator is fixed by three
    /// magnitudes, not by its zeros). A bound at the controls in force is
    /// [`decay_time`](Self::decay_time).
    ///
    /// # Errors
    ///
    /// Never.
    pub fn decay_ceiling(_: SampleRate, _: Db) -> Result<DecayTime, SosError> {
        Ok(DecayTime::Unbounded)
    }
}

/// The controls of a [`Tilt`] equalizer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TiltFilterControls {
    /// The pivot, where the gain is 0 dB. Read within 20 Hz and 0.99 of the Nyquist.
    pub frequency: Hz,
    /// The tilt: the gain at Nyquist is half of it and the gain at DC minus half. Read
    /// within 24 dB.
    pub gain: Db,
}

impl TiltFilterControls {
    /// 1 kHz and 0 dB.
    pub const DEFAULT: Self = Self {
        frequency: Hz::new(1_000.0),
        gain: Db::UNITY,
    };

    /// These controls held within the ranges they are read in at `rate`.
    #[must_use]
    pub fn held(&self, rate: SampleRate) -> Self {
        Self {
            frequency: held_center(self.frequency, rate),
            gain: held_gain(self.gain),
        }
    }

    /// The design of these controls as they stand (see [`held`](Self::held)) into `into`: no
    /// allocation.
    ///
    /// # Errors
    ///
    /// [`Tilt::design_into`]'s refusal of the controls.
    pub fn design_into(&self, rate: SampleRate, into: &mut Sos) -> Result<(), SosError> {
        Tilt::new(self.gain).design_into(self.frequency, rate, into)
    }

    /// The ring-out of the design at these controls ([`Tilt::decay_time`]).
    ///
    /// # Errors
    ///
    /// As [`Tilt::decay_time`].
    pub fn decay_time(&self, rate: SampleRate, floor: Db) -> Result<DecayTime, SosError> {
        Tilt::new(self.gain).decay_time(self.frequency, rate, floor)
    }

    /// The ring-out over every control at `rate`: [`Tilt::decay_time_over`] the pivots and
    /// gains the controls are read within.
    ///
    /// # Errors
    ///
    /// As [`Tilt::decay_time_over`].
    pub fn decay_ceiling(rate: SampleRate, floor: Db) -> Result<DecayTime, SosError> {
        let top = held_center(rate.nyquist_scaled(0.99), rate);
        Tilt::decay_time_over(MIN_EQ_CENTER, top, MAX_EQ_GAIN, rate, floor)
    }
}

/// The controls of a graphic equalizer: the gain of each band, lowest first. The bands past
/// the spacing's [`count`](GraphicBands::count) are not read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphicEqControls {
    /// The gain at each band's center, a boost above 0 dB and a cut below. Read within 24 dB.
    pub gains: [Db; GraphicBands::MAX_COUNT],
}

impl GraphicEqControls {
    /// Every band at 0 dB: a flat response.
    pub const FLAT: Self = Self {
        gains: [Db::UNITY; GraphicBands::MAX_COUNT],
    };

    /// These controls with every gain held within 24 dB.
    #[must_use]
    pub fn held(&self) -> Self {
        Self {
            gains: self.gains.map(held_gain),
        }
    }
}

#[cfg(test)]
mod tests;
