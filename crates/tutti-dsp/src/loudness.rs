//! The channel weights of ITU-R BS.1770-5 loudness: [`ChannelWeight`], what a speaker counts
//! for in the summed mean square, and [`channel_weights`], the weight of every channel of a
//! topology.

use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_units::Direction;

/// What one channel counts for in the loudness sum (ITU-R BS.1770-5, Annex 3, Table 4): the
/// weight `G` its mean square is multiplied by.
///
/// A speaker beside the listener (elevation under 30 degrees, azimuth 60 to 120 degrees either
/// side) weighs 1.41, about +1.5 dB, every other speaker 1.0, and the LFE adds no loudness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelWeight {
    /// Counts for nothing: the LFE.
    Silent,
    /// Weighs 1.0: the front, the back and every upper speaker.
    Unity,
    /// Weighs 1.41: a speaker beside the listener.
    Surround,
}

impl ChannelWeight {
    /// The weight `G` of a speaker standing at `direction`: [`Surround`](Self::Surround) when
    /// its elevation is within 30 degrees (exclusive) of the horizontal and its azimuth is 60
    /// to 120 degrees (inclusive) to either side, else [`Unity`](Self::Unity).
    #[must_use]
    pub fn of(direction: Direction) -> Self {
        let beside = direction.elevation.get().abs() < 30.0
            && (60.0..=120.0).contains(&direction.azimuth.get().abs());
        if beside { Self::Surround } else { Self::Unity }
    }

    /// The weight `G` a mean square is multiplied by: 0, 1.0 or 1.41, a power ratio.
    #[must_use]
    pub const fn power(self) -> f64 {
        match self {
            Self::Silent => 0.0,
            Self::Unity => 1.0,
            Self::Surround => 1.41,
        }
    }
}

/// A topology [`channel_weights`] cannot weigh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ChannelWeightError {
    /// An ambisonic topology has no speaker angles to weigh by.
    #[error("an ambisonic topology has no speaker placement to weigh loudness by")]
    Ambisonic,
    /// Speakers that are not a known bed's have no angles to weigh by.
    #[error("{width:?} speakers are not a known bed, so their placement is unknown")]
    Unplaced {
        /// The channel count of the refused topology.
        width: ChannelLayout,
    },
}

/// The [`ChannelWeight`] of every channel of `topology`, in channel order: the LFE
/// [`Silent`](ChannelWeight::Silent), every other speaker by [`ChannelWeight::of`] the
/// direction its bed gives it. A side or back speaker the bed spells the other way stands for
/// the bed's own (a VST3 quad's `FL FR SL SR` is the bed's `M+-135` backs, weight 1.0).
///
/// # Errors
///
/// [`ChannelWeightError::Ambisonic`] for an ambisonic topology and
/// [`ChannelWeightError::Unplaced`] for speakers that are not a [`Bed`](tutti_buffers::Bed)'s.
pub fn channel_weights(
    topology: &ChannelTopology,
) -> Result<Box<[ChannelWeight]>, ChannelWeightError> {
    let speakers = topology.positions().ok_or(ChannelWeightError::Ambisonic)?;
    let unplaced = ChannelWeightError::Unplaced {
        width: topology.width(),
    };
    let bed = topology.bed().ok_or(unplaced)?;
    speakers
        .iter()
        .map(|&speaker| {
            if speaker.is_lfe() {
                return Ok(ChannelWeight::Silent);
            }
            bed.direction(speaker)
                .or_else(|| speaker.counterpart().and_then(|c| bed.direction(c)))
                .map(ChannelWeight::of)
                .ok_or(unplaced)
        })
        .collect()
}

#[cfg(test)]
mod tests;
