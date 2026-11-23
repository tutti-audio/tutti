//! [`ChannelSet`]: a signal's channels, their width and, where a topology
//! holds them, their placement.

use crate::{ChannelLayout, ChannelTopology};

/// A signal's channels: how many, and which speaker each feeds where a
/// [`ChannelTopology`] holds them.
///
/// One value for the two facts, so they cannot disagree. A width up to
/// [`MAX_CHANNELS`](crate::MAX_CHANNELS) always has a topology (built from a width, it is
/// [`ChannelTopology::unassigned`] or [`ChannelTopology::default_for`]); a
/// width past it has none, and its channels are read by index.
///
/// # Examples
///
/// ```
/// use tutti_buffers::{Bed, ChannelLayout, ChannelSet};
///
/// let surround = ChannelSet::from(Bed::Surround51.topology());
/// assert_eq!(surround.width(), ChannelLayout::from(6u16));
///
/// let wide = ChannelSet::unassigned(ChannelLayout::from(24u16));
/// assert_eq!(wide.width().count(), 24);
/// assert!(wide.topology().is_none());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "wire::ChannelSetWire", into = "wire::ChannelSetWire")
)]
pub struct ChannelSet(Repr);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Repr {
    /// Placed by a topology: every width up to [`MAX_CHANNELS`](crate::MAX_CHANNELS).
    Placed(ChannelTopology),
    /// Past [`MAX_CHANNELS`](crate::MAX_CHANNELS), where no topology holds the channels.
    Wide(ChannelLayout),
}

impl ChannelSet {
    /// `width` channels that feed no speaker: [`ChannelTopology::unassigned`]
    /// up to [`MAX_CHANNELS`](crate::MAX_CHANNELS), no topology past it.
    #[must_use]
    pub fn unassigned(width: ChannelLayout) -> Self {
        ChannelTopology::unassigned(width).map_or(Self(Repr::Wide(width)), Self::from)
    }

    /// What a bare width is taken to mean: [`ChannelTopology::default_for`] up
    /// to [`MAX_CHANNELS`](crate::MAX_CHANNELS), no topology past it.
    #[must_use]
    pub fn default_for(width: ChannelLayout) -> Self {
        ChannelTopology::default_for(width).map_or(Self(Repr::Wide(width)), Self::from)
    }

    /// The channel count.
    #[must_use]
    pub fn width(&self) -> ChannelLayout {
        match &self.0 {
            Repr::Placed(topology) => topology.width(),
            Repr::Wide(width) => *width,
        }
    }

    /// Which speaker each channel feeds, or `None` past [`MAX_CHANNELS`](crate::MAX_CHANNELS).
    #[must_use]
    pub fn topology(&self) -> Option<&ChannelTopology> {
        match &self.0 {
            Repr::Placed(topology) => Some(topology),
            Repr::Wide(_) => None,
        }
    }
}

impl From<ChannelTopology> for ChannelSet {
    fn from(topology: ChannelTopology) -> Self {
        Self(Repr::Placed(topology))
    }
}

#[cfg(feature = "serde")]
mod wire {
    use super::{ChannelSet, Repr};
    use crate::{ChannelLayout, ChannelTopology, MAX_CHANNELS};

    /// The wire form: a topology, or a bare width past [`MAX_CHANNELS`].
    #[derive(serde::Serialize, serde::Deserialize)]
    pub(super) enum ChannelSetWire {
        Placed(ChannelTopology),
        Wide(ChannelLayout),
    }

    impl From<ChannelSet> for ChannelSetWire {
        fn from(set: ChannelSet) -> Self {
            match set.0 {
                Repr::Placed(topology) => Self::Placed(topology),
                Repr::Wide(width) => Self::Wide(width),
            }
        }
    }

    impl TryFrom<ChannelSetWire> for ChannelSet {
        type Error = &'static str;

        fn try_from(wire: ChannelSetWire) -> Result<Self, Self::Error> {
            match wire {
                ChannelSetWire::Placed(topology) => Ok(Self::from(topology)),
                ChannelSetWire::Wide(width) if width.count() as usize > MAX_CHANNELS => {
                    Ok(Self(Repr::Wide(width)))
                }
                ChannelSetWire::Wide(_) => {
                    Err("a width with no topology is wider than MAX_CHANNELS")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bed, MAX_CHANNELS};

    /// Up to the cap a width has a topology of that width; past it, none.
    #[test]
    fn a_width_has_a_topology_exactly_up_to_the_cap() {
        for n in [1u16, 2, 6, 8, 12, 16, 17, 24] {
            let width = ChannelLayout::from(n);
            for set in [
                ChannelSet::unassigned(width),
                ChannelSet::default_for(width),
            ] {
                assert_eq!(set.width(), width);
                assert_eq!(set.topology().is_some(), usize::from(n) <= MAX_CHANNELS);
                if let Some(topology) = set.topology() {
                    assert_eq!(topology.width(), width);
                }
            }
        }
        assert!(
            ChannelSet::unassigned(ChannelLayout::from(6u16))
                .topology()
                .is_some_and(ChannelTopology::is_unassigned)
        );
        assert_eq!(
            ChannelSet::default_for(ChannelLayout::from(6u16)).topology(),
            Some(&Bed::Surround51.topology())
        );
    }

    /// The wire form round-trips, and refuses a bare width a topology holds.
    #[cfg(feature = "serde")]
    #[test]
    fn the_wire_form_round_trips_and_refuses_a_narrow_bare_width() {
        for set in [
            ChannelSet::from(Bed::Surround51.topology()),
            ChannelSet::unassigned(ChannelLayout::from(24u16)),
        ] {
            let json = serde_json::to_string(&set).unwrap();
            assert_eq!(serde_json::from_str::<ChannelSet>(&json).unwrap(), set);
        }
        assert!(serde_json::from_str::<ChannelSet>(r#"{"Wide":8}"#).is_err());
    }
}
