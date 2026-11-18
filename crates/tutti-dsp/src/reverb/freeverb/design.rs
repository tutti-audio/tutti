//! [`FreeverbControls`]: what Freeverb is told, and the coefficients they make.
//!
//! Freeverb has two controls, as positions on `0..1`. The comb loop gain is `room * 0.28 +
//! 0.7` and the damping `damp * 0.4`, Jezar's own scalings. A control outside `0..1` is
//! clamped and one that is not finite takes the General MIDI value, so every control gives a
//! stable loop.

use tutti_units::{Feedback, Mix};

/// Freeverb's two controls, as positions on `0..1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FreeverbControls {
    /// The room size: scales the comb loop gain, `room * 0.28 + 0.7`.
    pub room: Mix,
    /// The damping: the comb's one-pole, `damp * 0.4`.
    pub damp: Mix,
}

impl FreeverbControls {
    /// The General MIDI reverb: room 0.5, damping 0.5.
    pub const GENERAL_MIDI: FreeverbControls = FreeverbControls {
        room: Mix(0.5),
        damp: Mix(0.5),
    };

    /// The comb loop gain these controls make, `room * 0.28 + 0.7`: in `0.7..=0.98`.
    #[must_use]
    pub fn feedback(&self) -> Feedback {
        Feedback(position(self.room, Self::GENERAL_MIDI.room) * 0.28 + 0.7)
    }

    /// The comb's damping, `damp * 0.4`: the one-pole's own pole, in `0..=0.4`, designed in
    /// `f64` for the bank to narrow once.
    #[must_use]
    pub(crate) fn damping(&self) -> f64 {
        f64::from(position(self.damp, Self::GENERAL_MIDI.damp)) * 0.4
    }
}

impl Default for FreeverbControls {
    fn default() -> Self {
        Self::GENERAL_MIDI
    }
}

/// `mix` clamped into `0..=1`, or `default` when it is not finite.
fn position(mix: Mix, default: Mix) -> f32 {
    let v = mix.get();
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        default.get()
    }
}

#[cfg(test)]
mod tests;
