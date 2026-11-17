use tutti_types::{Beat, Bpm};

/// How the tempo gets from a point to the next one.
///
/// Named for the transition between two points (a segment of the map), not for
/// a drawing: it is the law of the tempo across that segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Transition {
    /// Hold this tempo until the next point.
    Step,
    /// Move to the next point's tempo, tempo linear in time.
    ///
    /// This is CLAP's `tempo_inc` (a constant increment per sample), and the
    /// default for import and export.
    RampTime {
        /// The tempo the ramp reaches at the next point's beat. `None` reaches
        /// the next point's own tempo; `Some` ends there and the next point's
        /// tempo is a step from it: one beat holds the ramp's end and the
        /// step, with no plateau between. [`TempoMap::new`](crate::TempoMap::new)
        /// folds an end equal to the next tempo into `None`.
        end: Option<Bpm>,
    },
    /// Move to the next point's tempo, tempo linear in beats.
    ///
    /// Time is a logarithm of the beat. This is the ramp Ardour draws.
    RampBeats {
        /// The tempo the ramp reaches, as for [`RampTime`](Self::RampTime).
        end: Option<Bpm>,
    },
}

/// A tempo from `beat` on.
///
/// A plain value: [`TempoMap::new`](crate::TempoMap::new) is where it is
/// checked. A ramp's target is the next point's own tempo, so a ramp point
/// takes no tempo but its own: [`step`](Self::step), [`ramp_time`](Self::ramp_time)
/// and [`ramp_beats`](Self::ramp_beats) say which transition leaves it. A list
/// whose last point ramps is refused by the map
/// ([`TempoError::RampWithoutEnd`](crate::TempoError::RampWithoutEnd)); a
/// [`TempoMapBuilder`](crate::TempoMapBuilder) cannot write one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoPoint {
    /// Where it starts.
    pub beat: Beat,
    /// The tempo at `beat`.
    pub tempo: Bpm,
    /// How it gets to the next point.
    pub transition: Transition,
}

impl TempoPoint {
    /// `tempo` from `beat`, held until the next point.
    #[must_use]
    pub const fn step(beat: Beat, tempo: Bpm) -> Self {
        Self {
            beat,
            tempo,
            transition: Transition::Step,
        }
    }

    /// `tempo` at `beat`, moving to the next point's tempo with tempo linear
    /// in time ([`Transition::RampTime`]). There must be a next point.
    #[must_use]
    pub const fn ramp_time(beat: Beat, tempo: Bpm) -> Self {
        Self {
            beat,
            tempo,
            transition: Transition::RampTime { end: None },
        }
    }

    /// `tempo` at `beat`, moving to the next point's tempo with tempo linear
    /// in beats ([`Transition::RampBeats`]). There must be a next point.
    #[must_use]
    pub const fn ramp_beats(beat: Beat, tempo: Bpm) -> Self {
        Self {
            beat,
            tempo,
            transition: Transition::RampBeats { end: None },
        }
    }
}
