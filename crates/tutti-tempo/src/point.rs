use tutti_types::{Beat, Bpm};

/// How the tempo gets from a point to the next one.
///
/// Named for the transition between two points (a segment of the map), not for
/// a drawing: it is the law of the tempo across that segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transition {
    /// Hold this tempo until the next point.
    Step,
    /// Move to the next point's tempo, tempo linear in time.
    ///
    /// This is CLAP's `tempo_inc` (a constant increment per sample), and the
    /// default for import and export.
    RampTime,
    /// Move to the next point's tempo, tempo linear in beats.
    ///
    /// Time is a logarithm of the beat. This is the ramp Ardour draws.
    RampBeats,
}

/// A tempo from `beat` on.
///
/// One beat carries two tempos only where a ramp ends in a step: `arrive` is
/// the tempo the ramp before reaches at `beat`, and `tempo` is the one the
/// point departs with. A queried tempo at `beat` is the departing one (the map is
/// right-continuous), and the time of `beat` is the ramp's, so a step changes
/// the tempo and never the time.
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
    /// The tempo the ramp before this point ends at, when that is not `tempo`:
    /// the ramp arrives at `arrive` and the tempo steps to `tempo` at `beat`.
    /// `None` reaches `tempo`. Only a point after a ramp may set it
    /// ([`TempoError::ArriveWithoutRamp`](crate::TempoError::ArriveWithoutRamp)).
    pub arrive: Option<Bpm>,
}

impl TempoPoint {
    /// `tempo` from `beat`, held until the next point.
    #[must_use]
    pub const fn step(beat: Beat, tempo: Bpm) -> Self {
        Self {
            beat,
            tempo,
            transition: Transition::Step,
            arrive: None,
        }
    }

    /// `tempo` at `beat`, moving to the next point's tempo with tempo linear
    /// in time ([`Transition::RampTime`]). There must be a next point.
    #[must_use]
    pub const fn ramp_time(beat: Beat, tempo: Bpm) -> Self {
        Self {
            beat,
            tempo,
            transition: Transition::RampTime,
            arrive: None,
        }
    }

    /// `tempo` at `beat`, moving to the next point's tempo with tempo linear
    /// in beats ([`Transition::RampBeats`]). There must be a next point.
    #[must_use]
    pub const fn ramp_beats(beat: Beat, tempo: Bpm) -> Self {
        Self {
            beat,
            tempo,
            transition: Transition::RampBeats,
            arrive: None,
        }
    }

    /// The tempo the segment before this point reaches at `beat`: `arrive`,
    /// else `tempo`.
    #[must_use]
    pub fn arrival(&self) -> Bpm {
        self.arrive.unwrap_or(self.tempo)
    }
}
