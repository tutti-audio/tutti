//! The downmix levels of a [`Fold`] and the spec that states them.

use core::marker::PhantomData;

use super::{Fold, FoldEnd, FoldError};
use crate::Float;
use crate::{ChannelSet, ChannelTopology, Speaker};
use tutti_units::Amplitude;

/// The gains a fold applies where a speaker the destination lacks folds into
/// the nearest ones.
///
/// The default is ITU-R BS.775's: the centre and the surrounds at -3 dB, the
/// heights at -3 dB into the layer below, and the LFE omitted. A deliverable
/// that states its own (-4.5 or -6 dB, the LFE in) builds the fold with
/// [`Fold::spec`] and [`FoldSpec::levels`].
///
/// ```
/// use tutti_buffers::{Fold, FoldLevels};
/// use tutti_units::Amplitude;
/// use tutti_buffers::Bed;
///
/// let levels = FoldLevels {
///     centre: Amplitude::new(0.5),
///     ..FoldLevels::default()
/// };
/// let fold: Fold = Fold::spec(&Bed::Surround51.topology(), &Bed::Stereo.topology())
///     .levels(levels)
///     .build()
///     .unwrap();
/// // Left takes the centre (channel 2) at 0.5.
/// assert_eq!(fold.contributions(0)[1], (2, 0.5));
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FoldLevels {
    /// The front centre (and the centre pair) folded into the front pair, or a
    /// missing front speaker into its neighbour.
    pub centre: Amplitude,
    /// A side or rear speaker the destination lacks, folded into the speakers
    /// it has. A pure reposition (5.1 to quad) stays at unity.
    pub surround: Amplitude,
    /// An overhead speaker the destination lacks, folded into the layer below.
    pub height: Amplitude,
    /// `None` omits the LFE where the destination has none. `Some(g)` feeds it
    /// to the front left and right at `g`, or to the one channel of a mono
    /// destination at `g`.
    pub lfe: Option<Amplitude>,
}

impl Default for FoldLevels {
    /// ITU-R BS.775: -3 dB for the centre, the surrounds and the heights, and
    /// no LFE.
    fn default() -> Self {
        let minus_3_db = Amplitude::new(f64::MINUS_3_DB);
        Self {
            centre: minus_3_db,
            surround: minus_3_db,
            height: minus_3_db,
            lfe: None,
        }
    }
}

/// One of the [`FoldLevels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FoldLevel {
    /// [`FoldLevels::centre`].
    Centre,
    /// [`FoldLevels::surround`].
    Surround,
    /// [`FoldLevels::height`].
    Height,
    /// [`FoldLevels::lfe`].
    Lfe,
}

impl FoldLevels {
    /// Whether every level is a gain.
    ///
    /// # Errors
    ///
    /// [`FoldError::NotAGain`] naming the first level that is NaN, infinite or
    /// negative.
    pub fn check(&self) -> Result<(), FoldError> {
        let gain = |a: Amplitude| a.get().is_finite() && a.get() >= 0.0;
        let levels = [
            (FoldLevel::Centre, Some(self.centre)),
            (FoldLevel::Surround, Some(self.surround)),
            (FoldLevel::Height, Some(self.height)),
            (FoldLevel::Lfe, self.lfe),
        ];
        match levels
            .into_iter()
            .find(|&(_, a)| a.is_some_and(|a| !gain(a)))
        {
            Some((level, _)) => Err(FoldError::NotAGain { level }),
            None => Ok(()),
        }
    }
}

/// A fold from one topology into another, with its levels stated and not yet
/// built ([`Fold::spec`], [`Fold::to_channels`]).
#[derive(Debug, Clone, Copy)]
#[must_use = "a spec builds nothing until `build`"]
pub struct FoldSpec<'a, F: Float = f32> {
    src: &'a ChannelTopology,
    dst: Dest<'a>,
    levels: FoldLevels,
    float: PhantomData<F>,
}

/// What the spec folds into, and so which rules `build` applies.
#[derive(Debug, Clone, Copy)]
enum Dest<'a> {
    /// A topology, by [`Fold::between`]'s rules alone.
    Topology(&'a ChannelTopology),
    /// A set of output channels, by [`Fold::to_channels`]'s rules.
    Channels(&'a ChannelSet),
}

impl<'a, F: Float> FoldSpec<'a, F> {
    pub(super) fn new(src: &'a ChannelTopology, dst: &'a ChannelTopology) -> Self {
        Self::of(src, Dest::Topology(dst))
    }

    pub(super) fn to_channels(src: &'a ChannelTopology, dst: &'a ChannelSet) -> Self {
        Self::of(src, Dest::Channels(dst))
    }

    fn of(src: &'a ChannelTopology, dst: Dest<'a>) -> Self {
        Self {
            src,
            dst,
            levels: FoldLevels::default(),
            float: PhantomData,
        }
    }

    /// This spec with `levels` in place of the defaults.
    pub fn levels(mut self, levels: FoldLevels) -> Self {
        self.levels = levels;
        self
    }

    /// Builds the fold. Control side: it allocates.
    ///
    /// # Errors
    ///
    /// As [`Fold::between`] or [`Fold::to_channels`], whichever made the spec,
    /// and [`FoldError::NotAGain`] for a level that is NaN, infinite or
    /// negative.
    pub fn build(self) -> Result<Fold<F>, FoldError> {
        let (src, levels) = (self.src, self.levels);
        match self.dst {
            Dest::Topology(dst) => Fold::build(src, dst, levels),
            Dest::Channels(set) => {
                levels.check()?;
                let unknown = src
                    .positions()
                    .and_then(|list| list.iter().position(|p| matches!(p, Speaker::Unknown(_))));
                if let Some(channel) = unknown {
                    return Err(FoldError::Unnamed {
                        end: FoldEnd::Source,
                        channel,
                    });
                }
                match set.topology() {
                    // An ambisonic source is as unplaced as unassigned channels.
                    Some(dst)
                        if !(src.is_unassigned()
                            || src.ambisonic().is_some()
                            || dst.is_unassigned()) =>
                    {
                        Fold::build(src, dst, levels)
                    }
                    _ => Fold::by_index(src.width(), set.width()),
                }
            }
        }
    }
}
