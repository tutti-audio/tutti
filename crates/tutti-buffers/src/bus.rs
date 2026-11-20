//! [`BusTopology`] — a [`ChannelTopology`] small enough to be `Copy`.
//!
//! A node's `Shape` is `Copy` and a [`ChannelTopology`] owns a `Vec`, so the
//! speakers of one bus of a node live inline here, up to [`MAX_CHANNELS`]. The
//! two convert both ways; the one direction that can refuse is the wider one.

use crate::{
    AmbisonicFormat, Bed, ChannelLayout, ChannelTopology, MAX_CHANNELS, Speaker, Speakers,
};

/// What a bus holds: speakers inline, or an ambisonic format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Repr {
    Speakers {
        len: u8,
        speakers: [Speaker; MAX_CHANNELS],
    },
    Ambisonic(AmbisonicFormat),
}

/// The channels of one bus, held inline: speakers in channel order, or an
/// ambisonic format.
///
/// The same set a [`ChannelTopology`] is, bounded by [`MAX_CHANNELS`] and
/// `Copy`. It may be empty (a bus the plugin declares with no channels), and
/// then a fold from or to it is a `tutti_buffers::FoldError::Buffer` of a zero width,
/// as for any empty topology.
///
/// `new` refuses a list wider than a bus can hold, and a repeated named
/// speaker; it never truncates.
/// The slots past the length are always [`Speaker::Unassigned`], so the derived
/// equality compares the bus and nothing else.
///
/// ```
/// use tutti_buffers::{Bed, BusTopology, ChannelLayout, ChannelTopology, Speaker};
///
/// let stereo = BusTopology::new([Speaker::FrontLeft, Speaker::FrontRight]).unwrap();
/// assert_eq!(stereo.layout(), ChannelLayout::STEREO);
/// assert_eq!(ChannelTopology::from(stereo), Bed::Stereo.topology());
/// assert!(BusTopology::new([Speaker::Unassigned; 17]).is_none());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BusTopology(Repr);

/// A topology too wide for one bus, refused by `BusTopology`'s `TryFrom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{width} channels is wider than a bus ({})", MAX_CHANNELS)]
pub struct BusTooWide {
    /// How many channels the refused list has.
    pub width: usize,
}

impl BusTopology {
    /// A bus with no channels.
    pub const EMPTY: Self = Self(Repr::Speakers {
        len: 0,
        speakers: [Speaker::Unassigned; MAX_CHANNELS],
    });

    /// One channel at the front centre: a mono source.
    pub const MONO: Self = {
        let mut speakers = [Speaker::Unassigned; MAX_CHANNELS];
        speakers[0] = Speaker::FrontCenter;
        Self(Repr::Speakers { len: 1, speakers })
    };

    /// A stereo pair: front left, front right.
    pub const STEREO: Self = {
        let mut speakers = [Speaker::Unassigned; MAX_CHANNELS];
        speakers[0] = Speaker::FrontLeft;
        speakers[1] = Speaker::FrontRight;
        Self(Repr::Speakers { len: 2, speakers })
    };

    /// The bus of these speakers, or `None` past [`MAX_CHANNELS`] or when a
    /// named speaker repeats (as [`Speakers::new`] refuses it).
    pub fn new(positions: impl IntoIterator<Item = Speaker>) -> Option<Self> {
        let mut speakers = [Speaker::Unassigned; MAX_CHANNELS];
        let mut len = 0u8;
        for speaker in positions {
            let repeats = !matches!(speaker, Speaker::Unassigned | Speaker::Unknown(_))
                && speakers[..usize::from(len)].contains(&speaker);
            if repeats {
                return None;
            }
            *speakers.get_mut(usize::from(len))? = speaker;
            len += 1;
        }
        Some(Self(Repr::Speakers { len, speakers }))
    }

    /// An ambisonic bus of `format`. Every format fits: third order is
    /// [`MAX_CHANNELS`] channels.
    #[must_use]
    pub const fn ambisonic(format: AmbisonicFormat) -> Self {
        Self(Repr::Ambisonic(format))
    }

    /// `width` channels that feed no speaker, or `None` past [`MAX_CHANNELS`].
    /// A width is not a placement: this is the bus a node that states only a
    /// count has.
    #[must_use]
    pub const fn unassigned(width: ChannelLayout) -> Option<Self> {
        if width.count() as usize > MAX_CHANNELS {
            return None;
        }
        Some(Self(Repr::Speakers {
            len: width.count() as u8,
            speakers: [Speaker::Unassigned; MAX_CHANNELS],
        }))
    }

    /// What a bare `width` is taken to mean, as a bus: the placement
    /// [`ChannelTopology::default_for`] gives it. `None` past [`MAX_CHANNELS`].
    ///
    /// For a node or a design built for a width and nothing more (a filter of
    /// `n` channels): what its bus states is the engine's default layout, said
    /// once there.
    #[must_use]
    pub fn default_for(width: ChannelLayout) -> Option<Self> {
        Self::try_from(&ChannelTopology::default_for(width)).ok()
    }

    /// The speaker each channel feeds, in channel order, or `None` for an
    /// ambisonic bus.
    #[must_use]
    pub fn positions(&self) -> Option<&[Speaker]> {
        match &self.0 {
            Repr::Speakers { len, speakers } => Some(&speakers[..usize::from(*len)]),
            Repr::Ambisonic(_) => None,
        }
    }

    /// The ambisonic format, or `None` for speakers.
    #[must_use]
    pub const fn ambisonic_format(&self) -> Option<AmbisonicFormat> {
        match self.0 {
            Repr::Speakers { .. } => None,
            Repr::Ambisonic(format) => Some(format),
        }
    }

    /// The channel count.
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        match &self.0 {
            Repr::Speakers { len, .. } => ChannelLayout::from_count(*len as u16),
            Repr::Ambisonic(format) => format.width(),
        }
    }
}

impl From<Bed> for BusTopology {
    fn from(bed: Bed) -> Self {
        // Every bed is at most twelve channels, inside the cap.
        Self::try_from(&bed.topology()).expect("a bed fits a bus")
    }
}

impl From<BusTopology> for ChannelTopology {
    fn from(bus: BusTopology) -> Self {
        match bus.0 {
            Repr::Speakers { len, speakers } => {
                // `new` refuses a repeated named speaker, so the list is valid.
                Self::Speakers(Speakers::from_format(
                    speakers[..usize::from(len)].iter().copied(),
                ))
            }
            Repr::Ambisonic(format) => Self::Ambisonic(format),
        }
    }
}

impl TryFrom<&ChannelTopology> for BusTopology {
    type Error = BusTooWide;

    fn try_from(topology: &ChannelTopology) -> Result<Self, BusTooWide> {
        match topology {
            ChannelTopology::Speakers(speakers) => Self::new(speakers.as_slice().iter().copied())
                .ok_or(BusTooWide {
                    width: speakers.len(),
                }),
            ChannelTopology::Ambisonic(format) => Ok(Self::ambisonic(*format)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AmbisonicOrder;

    /// A bare width is the default layout for its width, or unassigned where there is
    /// none, and refused past the cap.
    #[test]
    fn a_default_bus_is_the_default_topology() {
        assert_eq!(
            BusTopology::default_for(ChannelLayout::MONO),
            Some(BusTopology::MONO)
        );
        assert_eq!(
            BusTopology::default_for(ChannelLayout::STEREO),
            Some(BusTopology::STEREO)
        );
        assert_eq!(
            BusTopology::default_for(ChannelLayout::from_count(12)),
            Some(BusTopology::from(Bed::Surround714))
        );
        let nine = ChannelLayout::from_count(9);
        assert!(
            ChannelTopology::default_for(nine).is_unassigned(),
            "the case needs a width with no order"
        );
        assert_eq!(
            BusTopology::default_for(nine),
            BusTopology::unassigned(nine)
        );
        assert_eq!(
            BusTopology::default_for(ChannelLayout::from_count(17)),
            None
        );
    }

    /// `new` and `TryFrom` agree on the edge of the range, a width of 16 is a
    /// bus and 17 is refused with its width, and the round trip through
    /// `ChannelTopology` is the identity.
    #[test]
    fn a_bus_holds_sixteen_channels_and_refuses_more() {
        let sixteen = ChannelTopology::unassigned(ChannelLayout::from(16u16));
        let bus = BusTopology::try_from(&sixteen).unwrap();
        assert_eq!(bus.layout().count(), 16);
        assert_eq!(ChannelTopology::from(bus), sixteen);
        assert_eq!(
            BusTopology::unassigned(ChannelLayout::from(16u16)),
            Some(bus)
        );

        let seventeen = ChannelTopology::unassigned(ChannelLayout::from(17u16));
        assert_eq!(
            BusTopology::try_from(&seventeen),
            Err(BusTooWide { width: 17 })
        );
        assert_eq!(BusTopology::unassigned(ChannelLayout::from(17u16)), None);
    }

    /// Placement survives, and the slots past the length do not leak into
    /// equality: a 2-wide bus built two ways is equal.
    #[test]
    fn speakers_keep_their_slots_and_spare_slots_are_canonical() {
        use Speaker::*;
        let a = BusTopology::new([FrontRight, FrontLeft]).unwrap();
        assert_eq!(a.positions().unwrap(), &[FrontRight, FrontLeft]);
        let wide = BusTopology::new([FrontRight, FrontLeft, Unassigned]).unwrap();
        assert_ne!(a, wide);
        assert_eq!(BusTopology::new([]).unwrap(), BusTopology::EMPTY);
        assert_eq!(
            BusTopology::unassigned(ChannelLayout::STEREO),
            BusTopology::new([Unassigned, Unassigned])
        );
    }

    /// An ambisonic bus keeps its format, has no speaker list, and its width is
    /// (order + 1)², up to the cap's 16 at third order.
    #[test]
    fn an_ambisonic_bus_keeps_its_format_and_derives_its_width() {
        for (order, width) in [(0u8, 1u16), (1, 4), (2, 9), (3, 16)] {
            let format = AmbisonicFormat::ambix(AmbisonicOrder::of(order));
            let bus = BusTopology::ambisonic(format);
            assert_eq!(bus.layout().count(), width);
            assert_eq!(bus.positions(), None);
            assert_eq!(bus.ambisonic_format(), Some(format));
            let topology = ChannelTopology::from(bus);
            assert_eq!(topology, ChannelTopology::Ambisonic(format));
            assert_eq!(BusTopology::try_from(&topology), Ok(bus));
        }
        assert_ne!(
            BusTopology::ambisonic(AmbisonicFormat::ambix(AmbisonicOrder::of(1))),
            BusTopology::ambisonic(AmbisonicFormat::fuma(AmbisonicOrder::of(1)))
        );
    }

    /// A repeated named speaker is no bus, as it is no topology; `Unassigned`
    /// and `Unknown` repeat.
    #[test]
    fn a_bus_refuses_a_named_speaker_twice() {
        use Speaker::*;
        assert_eq!(BusTopology::new([FrontLeft, FrontLeft]), None);
        assert!(BusTopology::new([Unassigned, Unassigned, Unknown(3), Unknown(3)]).is_some());
        let bus = BusTopology::new([FrontLeft, FrontRight, Unknown(1), Unknown(1)]).unwrap();
        let topology = ChannelTopology::from(bus);
        assert_eq!(BusTopology::try_from(&topology), Ok(bus));
    }

    /// Every bed fits a bus and converts back to its topology.
    #[test]
    fn every_bed_is_a_bus() {
        for bed in Bed::ALL {
            let bus = BusTopology::from(bed);
            assert_eq!(ChannelTopology::from(bus), bed.topology());
            assert_eq!(bus.layout(), bed.width());
        }
    }
}
