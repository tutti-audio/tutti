//! A voice's per-note (MPE) expression, and the one place it is applied.

use tutti_units::Normalized;

/// Slide (CC74) centre: "no timbre shift". The cutoff factor is
/// `4^(slide - SLIDE_CENTER)`, so a note with no CC74 must start here.
pub(crate) const SLIDE_CENTER: Normalized = Normalized::of(0.5);

/// One per-note expression value, `0..=1`, as a message addresses it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MpeControl {
    /// Key pressure.
    Pressure(Normalized),
    /// Slide (CC74), centred at [`SLIDE_CENTER`].
    Slide(Normalized),
    /// Per-note volume.
    Gain(Normalized),
}

/// A voice's per-note expression received so far. Which messages reach a voice (Detach, the bend's
/// sensitivity) is `tutti-voicing`'s decision; the values arrive resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MpeVoiceState {
    /// Per-note pressure.
    pub pressure: Normalized,
    /// Per-note slide, centred at [`SLIDE_CENTER`].
    pub slide: Normalized,
    /// Per-note gain.
    pub gain: Normalized,
}

impl MpeVoiceState {
    pub(crate) fn new() -> Self {
        Self {
            pressure: Normalized::of(0.0),
            slide: SLIDE_CENTER,
            gain: Normalized::of(1.0),
        }
    }

    /// Stores one controller.
    pub(crate) fn apply(&mut self, control: MpeControl) {
        match control {
            MpeControl::Pressure(p) => self.pressure = p,
            MpeControl::Slide(s) => self.slide = s,
            MpeControl::Gain(g) => self.gain = g,
        }
    }

    /// Returns the controllers to their defaults.
    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// The slide in effect: `None` at centre.
    pub(crate) fn effective_slide(&self) -> Option<Normalized> {
        ((self.slide.get() - SLIDE_CENTER.get()).abs() > 0.001).then_some(self.slide)
    }

    /// The gain factors in effect, `(pressure, note)`. The pressure is the
    /// larger of the note's own and the channel's, `channel_pressure`.
    pub(crate) fn gain_factors(&self, channel_pressure: Normalized) -> (f32, f32) {
        // The factors multiply an `f32` sample: narrowed once here.
        (
            (1.0 + self.pressure.get().max(channel_pressure.get()) * 0.5) as f32,
            self.gain.get() as f32,
        )
    }
}

#[cfg(test)]
mod tests;
