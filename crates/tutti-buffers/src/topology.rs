//! [`ChannelTopology`]: *what* a set of channels is, as opposed to
//! [`ChannelLayout`]'s *how many*.
//!
//! A channel set is speakers (an ordered list, each channel feeding one
//! [`Speaker`]) or ambisonic (a spherical-harmonic encoding, not speaker
//! feeds). The width of either is derived from it, never stored beside it.
//!
//! `ChannelLayout` is a count, and a foreign layout type converts to and from
//! it at a crate boundary losing the placement on purpose. Reach for this type
//! only when the placement is the thing needed; a caller that wants a width
//! keeps the count.
//!
//! # Why an ordered list rather than a set of speakers
//!
//! Surveyed at the four plugin formats' own headers, because they do not agree:
//!
//! - **VST3** is a **set** — a 64-bit mask. Channel order is not stored, it is
//!   *derived*: `getSpeakerIndex` returns the popcount of set bits below a
//!   speaker, so buffer order is strictly ascending bit index. VST3 therefore
//!   cannot express two different orders of the same speaker set.
//! - **AU**, **CLAP** and **VST2** are **ordered lists**. Apple defines
//!   `MPEG_5_1_A/B/C/D` as four *different orders* of the identical six
//!   speakers; CLAP's `channel_map[i]` is literally the speaker fed by channel
//!   `i`.
//!
//! A list is strictly the more expressive of the two: every VST3 arrangement
//! converts to a list without loss, while `MPEG_5_1_B` cannot be represented as
//! a set at all. So the shared type is a list, and the one lossy direction is
//! *out* to VST3 — which is where a conversion has to decide whether to reorder
//! the caller's buffers or refuse.
//!
//! # Ambisonics is not speaker positions
//!
//! B-format `W X Y Z` is not four speaker feeds, and is not modelled as four
//! invented [`Speaker`]s: it is [`ChannelTopology::Ambisonic`].

use crate::{AmbisonicFormat, ChannelLayout, MAX_CHANNELS};
use alloc::vec::Vec;

/// Which speaker one channel of a bus feeds.
///
/// # Why an open catalog
///
/// The formats keep adding positions — CLAP's list grew a top-side pair, Apple
/// ships new spatial tags per release — so a closed enum would turn a plugin
/// update into a decode failure. [`Unknown`](Self::Unknown) carries the value
/// verbatim instead.
///
/// That arm is not merely for logging. A topology is **positional**, so an
/// unnameable speaker must still occupy its channel's slot: dropping it would
/// renumber every channel after it and silently re-route the tail of the bus.
///
/// The variants are the positions common to more than one format. A
/// format-specific speaker (VST3's proximity pair, AU's matrix-total Lt/Rt)
/// stays `Unknown` until a second format names it too; inventing a shared name
/// for a thing one format means is how a vocabulary stops being shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Speaker {
    /// Front left — channel 0 of any stereo or surround bed.
    FrontLeft,
    /// Front right — channel 1 of any stereo or surround bed.
    FrontRight,
    /// Front centre, the dialogue channel in a film bed.
    FrontCenter,
    /// The `.1` — a band-limited effects channel, not a placed speaker.
    LowFrequency,
    /// Rear left, behind the listener.
    BackLeft,
    /// Rear right, behind the listener.
    BackRight,
    /// Front left-of-centre, between `FrontLeft` and `FrontCenter`.
    FrontLeftOfCenter,
    /// Front right-of-centre, between `FrontCenter` and `FrontRight`.
    FrontRightOfCenter,
    /// Rear centre — the single surround speaker of a 6.1 bed.
    BackCenter,
    /// Side left, level with the listener rather than behind.
    SideLeft,
    /// Side right, level with the listener rather than behind.
    SideRight,
    /// Overhead centre, directly above the listener.
    TopCenter,
    /// Overhead front left — the height layer of an Atmos-style bed.
    TopFrontLeft,
    /// Overhead front centre.
    TopFrontCenter,
    /// Overhead front right.
    TopFrontRight,
    /// Overhead rear left.
    TopBackLeft,
    /// Overhead rear centre.
    TopBackCenter,
    /// Overhead rear right.
    TopBackRight,
    /// Overhead side left, level with the listener in plan.
    TopSideLeft,
    /// Overhead side right, level with the listener in plan.
    TopSideRight,
    /// A position this vocabulary does not name, carried verbatim so it keeps
    /// its channel's slot. The payload is the *format's* raw value and is only
    /// meaningful to the format that produced it.
    Unknown(u16),
    /// A channel that feeds no speaker: one input of an interface, one
    /// channel of a multitrack file, an effect's numbered output. It is
    /// *known* to be unplaced, which is what separates it from
    /// [`Unknown`](Self::Unknown) (a position the format named and this
    /// vocabulary cannot). A fold maps unassigned channels one to one by index
    /// and never passes them through a surround matrix. Declared last so the
    /// wire index of every other variant is unchanged.
    Unassigned,
}

impl Speaker {
    /// Returns whether this is the LFE / `.1` channel.
    ///
    /// Worth a predicate rather than an `==` at each call site because the LFE
    /// is the one position with different *handling* rather than a different
    /// place: it is excluded from a consumer downmix (see `tutti_buffers::Fold`), and
    /// it is not a VBAP-panned speaker.
    #[must_use]
    pub const fn is_lfe(self) -> bool {
        matches!(self, Self::LowFrequency)
    }

    /// Returns whether this channel is deliberately speaker-less
    /// ([`Unassigned`](Self::Unassigned)).
    #[must_use]
    pub const fn is_unassigned(self) -> bool {
        matches!(self, Self::Unassigned)
    }

    /// Returns the surround speaker this one is exchanged with: side for back
    /// and back for side (`None` for any other speaker).
    ///
    /// The two spellings of one set of feeds (see [`Bed::Quad`](crate::Bed::Quad)):
    /// a `tutti_buffers::Fold` and an encoder's channel order both let a bed
    /// use either.
    #[must_use]
    pub const fn counterpart(self) -> Option<Self> {
        match self {
            Self::SideLeft => Some(Self::BackLeft),
            Self::SideRight => Some(Self::BackRight),
            Self::BackLeft => Some(Self::SideLeft),
            Self::BackRight => Some(Self::SideRight),
            _ => None,
        }
    }

    /// Returns whether this speaker is on the listener's left: one of the seven
    /// left speakers a [`mirror`](Self::mirror) pairs with a right one.
    #[must_use]
    pub const fn is_left(self) -> bool {
        matches!(
            self,
            Self::FrontLeft
                | Self::BackLeft
                | Self::FrontLeftOfCenter
                | Self::SideLeft
                | Self::TopFrontLeft
                | Self::TopBackLeft
                | Self::TopSideLeft
        )
    }

    /// Returns whether this speaker is on the listener's right: the
    /// [`mirror`](Self::mirror) of a left one.
    #[must_use]
    pub const fn is_right(self) -> bool {
        matches!(
            self,
            Self::FrontRight
                | Self::BackRight
                | Self::FrontRightOfCenter
                | Self::SideRight
                | Self::TopFrontRight
                | Self::TopBackRight
                | Self::TopSideRight
        )
    }

    /// Returns the speaker at the mirror image of this one across the
    /// listener's median plane: its left/right partner at the same depth and
    /// height (`FrontLeft` for `FrontRight`, `TopSideRight` for `TopSideLeft`).
    ///
    /// `None` for a speaker on the median plane (the centres and
    /// [`TopCenter`](Self::TopCenter)), for the LFE, which is not placed, and
    /// for [`Unknown`](Self::Unknown) and [`Unassigned`](Self::Unassigned).
    #[must_use]
    pub const fn mirror(self) -> Option<Self> {
        match self {
            Self::FrontLeft => Some(Self::FrontRight),
            Self::FrontRight => Some(Self::FrontLeft),
            Self::BackLeft => Some(Self::BackRight),
            Self::BackRight => Some(Self::BackLeft),
            Self::FrontLeftOfCenter => Some(Self::FrontRightOfCenter),
            Self::FrontRightOfCenter => Some(Self::FrontLeftOfCenter),
            Self::SideLeft => Some(Self::SideRight),
            Self::SideRight => Some(Self::SideLeft),
            Self::TopFrontLeft => Some(Self::TopFrontRight),
            Self::TopFrontRight => Some(Self::TopFrontLeft),
            Self::TopBackLeft => Some(Self::TopBackRight),
            Self::TopBackRight => Some(Self::TopBackLeft),
            Self::TopSideLeft => Some(Self::TopSideRight),
            Self::TopSideRight => Some(Self::TopSideLeft),
            Self::FrontCenter
            | Self::LowFrequency
            | Self::BackCenter
            | Self::TopCenter
            | Self::TopFrontCenter
            | Self::TopBackCenter
            | Self::Unknown(_)
            | Self::Unassigned => None,
        }
    }
}

/// A speaker list a topology cannot hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TopologyError {
    /// A named speaker appears more than once. [`Speaker::Unassigned`] and
    /// each distinct [`Speaker::Unknown`] may repeat; any other speaker feeds
    /// one channel.
    #[error("{0:?} feeds more than one channel")]
    Duplicate(Speaker),
    /// The list is wider than a topology holds, [`MAX_CHANNELS`].
    #[error("{width} channels is wider than a topology holds ({})", MAX_CHANNELS)]
    TooWide {
        /// How many channels the refused list has.
        width: usize,
    },
}

/// The speaker each channel of a bus feeds, in channel order, no named speaker
/// twice, at most [`MAX_CHANNELS`] of them.
///
/// `positions()[i]` is the speaker fed by channel `i`, so the length **is** the
/// channel count: it is derived, never stored beside the list, so the two
/// cannot drift. [`Speaker::Unassigned`] and each distinct
/// [`Speaker::Unknown`] may repeat, so a 16-channel discrete interface is
/// sixteen `Unassigned`. The speakers live inline and the slots past the
/// length are always [`Speaker::Unassigned`], so the derived equality and hash
/// compare the list and nothing else.
///
/// ```
/// use tutti_buffers::{Speaker, Speakers, TopologyError};
///
/// let stereo = Speakers::new([Speaker::FrontLeft, Speaker::FrontRight]).unwrap();
/// assert_eq!(stereo.index_of(Speaker::FrontRight), Some(1));
/// assert_eq!(
///     Speakers::new([Speaker::FrontLeft, Speaker::FrontLeft]),
///     Err(TopologyError::Duplicate(Speaker::FrontLeft)),
/// );
/// assert_eq!(
///     Speakers::new([Speaker::Unassigned; 17]),
///     Err(TopologyError::TooWide { width: 17 }),
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "Vec<Speaker>", into = "Vec<Speaker>")
)]
pub struct Speakers {
    len: u8,
    list: [Speaker; MAX_CHANNELS],
}

impl Speakers {
    /// No channels.
    pub const EMPTY: Self = Self {
        len: 0,
        list: [Speaker::Unassigned; MAX_CHANNELS],
    };

    /// The speakers of an ordered list.
    ///
    /// Takes any iterator so a decode loop can feed it directly.
    ///
    /// # Errors
    ///
    /// [`TopologyError::Duplicate`] when a named speaker appears twice, and
    /// [`TopologyError::TooWide`] past [`MAX_CHANNELS`].
    pub fn new(list: impl IntoIterator<Item = Speaker>) -> Result<Self, TopologyError> {
        let mut out = Self::EMPTY;
        let mut list = list.into_iter();
        while let Some(speaker) = list.next() {
            if out.is_full() {
                return Err(TopologyError::TooWide {
                    width: MAX_CHANNELS + 1 + list.count(),
                });
            }
            if !matches!(speaker, Speaker::Unassigned | Speaker::Unknown(_))
                && out.as_slice().contains(&speaker)
            {
                return Err(TopologyError::Duplicate(speaker));
            }
            out.push(speaker);
        }
        Ok(out)
    }

    /// The speakers a format reports, never refusing a repeat: a named speaker
    /// that repeats becomes [`Speaker::Unassigned`] in each slot after its
    /// first. `None` when the format reports more than [`MAX_CHANNELS`]
    /// channels, which no topology holds.
    ///
    /// For a format boundary, which does not get to drop a plugin's whole map
    /// because it names `FrontLeft` twice: every other channel keeps its slot
    /// and its speaker. A bus too wide for a topology is one with no topology,
    /// its width kept by the caller's [`ChannelLayout`].
    #[must_use]
    pub fn from_format(list: impl IntoIterator<Item = Speaker>) -> Option<Self> {
        let mut out = Self::EMPTY;
        for speaker in list {
            if out.is_full() {
                return None;
            }
            let repeated = !matches!(speaker, Speaker::Unassigned | Speaker::Unknown(_))
                && out.as_slice().contains(&speaker);
            out.push(if repeated {
                Speaker::Unassigned
            } else {
                speaker
            });
        }
        Some(out)
    }

    /// `width` channels that feed no speaker, or `None` past [`MAX_CHANNELS`]: a
    /// wider signal keeps its [`ChannelLayout`] and has no topology.
    #[must_use]
    pub const fn unassigned(width: ChannelLayout) -> Option<Self> {
        let count = width.count() as usize;
        if count > MAX_CHANNELS {
            return None;
        }
        Some(Self {
            len: count as u8,
            list: [Speaker::Unassigned; MAX_CHANNELS],
        })
    }

    const fn is_full(&self) -> bool {
        self.len as usize == MAX_CHANNELS
    }

    // The caller has checked there is room.
    fn push(&mut self, speaker: Speaker) {
        self.list[usize::from(self.len)] = speaker;
        self.len += 1;
    }

    /// The speaker each channel feeds, in channel order.
    #[must_use]
    pub fn as_slice(&self) -> &[Speaker] {
        &self.list[..usize::from(self.len)]
    }

    /// The channel count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether there are no channels.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Which channel feeds `speaker`, or `None` if none does.
    #[must_use]
    pub fn index_of(&self, speaker: Speaker) -> Option<usize> {
        self.as_slice().iter().position(|&p| p == speaker)
    }

    /// The LFE channel's index, if there is one.
    ///
    /// Named because it is the lookup with a consequence: the LFE is dropped
    /// from a consumer downmix and is fed by a separate bass-management send
    /// rather than a panner, so "which index is it" is asked by code that will
    /// treat that channel differently.
    #[must_use]
    pub fn lfe_index(&self) -> Option<usize> {
        self.as_slice().iter().position(|p| p.is_lfe())
    }

    /// Whether no position is [`Unknown`](Speaker::Unknown).
    ///
    /// A caller that intends to *route* by speaker needs this: an
    /// [`Unknown`](Speaker::Unknown) channel keeps its slot, but nothing can be
    /// said about where it should go. An [`Unassigned`](Speaker::Unassigned)
    /// channel counts as named: it is known to feed no speaker.
    #[must_use]
    pub fn is_fully_named(&self) -> bool {
        !self
            .as_slice()
            .iter()
            .any(|p| matches!(p, Speaker::Unknown(_)))
    }

    /// Whether the list is not empty and every channel is
    /// [`Unassigned`](Speaker::Unassigned).
    #[must_use]
    pub fn is_unassigned(&self) -> bool {
        !self.is_empty() && self.as_slice().iter().all(|p| p.is_unassigned())
    }
}

impl TryFrom<Vec<Speaker>> for Speakers {
    type Error = TopologyError;

    fn try_from(list: Vec<Speaker>) -> Result<Self, TopologyError> {
        Self::new(list)
    }
}

impl From<Speakers> for Vec<Speaker> {
    fn from(speakers: Speakers) -> Self {
        speakers.as_slice().to_vec()
    }
}

/// A channel set: speakers, or an ambisonic signal.
///
/// *What* the channels are, as opposed to [`ChannelLayout`]'s *how many*; the
/// [`width`](Self::width) is derived from either arm. At most [`MAX_CHANNELS`]
/// wide (third-order ambisonics is exactly that), and `Copy`: a node's `Shape`
/// holds one per bus. It may be empty (a bus a plugin declares with no
/// channels), and then a fold from or to it is a
/// `tutti_buffers::FoldError::Buffer` of a zero width.
///
/// # A width alone is not a placement
///
/// A [`ChannelLayout`] says how many channels, and nothing about what they
/// are: 4 is quad or B-format, 6 is 5.1 or six interface inputs, 12 is 7.1.4
/// or twelve tracks. Only an edge knows, so an edge states a topology, or says
/// it guessed with [`default_for`](Self::default_for) and reports a [`Placement`].
/// `tutti_buffers::Fold::between` is the one place two topologies meet.
///
/// A wider signal keeps its [`ChannelLayout`] and has no topology: a list past
/// [`MAX_CHANNELS`] is refused with [`TopologyError::TooWide`], never truncated,
/// and a width past it has none from [`unassigned`](Self::unassigned) or
/// [`default_for`](Self::default_for).
///
/// # Examples
///
/// ```
/// use tutti_buffers::{Bed, ChannelLayout, ChannelTopology, Speaker};
///
/// let bed = ChannelTopology::default_for(ChannelLayout::from(6u16)).unwrap();
/// assert_eq!(bed, Bed::Surround51.topology());
/// assert_eq!(bed.index_of(Speaker::FrontCenter), Some(2));
/// assert_eq!(bed.lfe_index(), Some(3));
/// assert_eq!(bed.width().count(), 6);
/// assert!(ChannelTopology::default_for(ChannelLayout::from(17u16)).is_none());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ChannelTopology {
    /// Speaker feeds, in channel order.
    Speakers(Speakers),
    /// A spherical-harmonic encoding.
    Ambisonic(AmbisonicFormat),
}

impl ChannelTopology {
    /// No channels.
    pub const EMPTY: Self = Self::Speakers(Speakers::EMPTY);

    /// One channel at the front centre: a mono source.
    pub const MONO: Self = {
        let mut list = [Speaker::Unassigned; MAX_CHANNELS];
        list[0] = Speaker::FrontCenter;
        Self::Speakers(Speakers { len: 1, list })
    };

    /// A stereo pair: front left, front right.
    pub const STEREO: Self = {
        let mut list = [Speaker::Unassigned; MAX_CHANNELS];
        list[0] = Speaker::FrontLeft;
        list[1] = Speaker::FrontRight;
        Self::Speakers(Speakers { len: 2, list })
    };

    /// A speaker topology from an ordered list.
    ///
    /// # Errors
    ///
    /// [`TopologyError::Duplicate`] when a named speaker appears twice, and
    /// [`TopologyError::TooWide`] past [`MAX_CHANNELS`].
    pub fn new(list: impl IntoIterator<Item = Speaker>) -> Result<Self, TopologyError> {
        Speakers::new(list).map(Self::Speakers)
    }

    /// The channel count, as a [`ChannelLayout`]: the speaker count, or
    /// `(order + 1)²`.
    #[must_use]
    pub const fn width(&self) -> ChannelLayout {
        match self {
            Self::Speakers(s) => ChannelLayout::from_count(s.len() as u16),
            Self::Ambisonic(f) => f.width(),
        }
    }

    /// The speakers, or `None` for an ambisonic set.
    #[must_use]
    pub const fn speakers(&self) -> Option<&Speakers> {
        match self {
            Self::Speakers(s) => Some(s),
            Self::Ambisonic(_) => None,
        }
    }

    /// The speaker each channel feeds, or `None` for an ambisonic set.
    #[must_use]
    pub fn positions(&self) -> Option<&[Speaker]> {
        self.speakers().map(Speakers::as_slice)
    }

    /// The ambisonic format, or `None` for speakers.
    #[must_use]
    pub const fn ambisonic(&self) -> Option<AmbisonicFormat> {
        match self {
            Self::Speakers(_) => None,
            Self::Ambisonic(f) => Some(*f),
        }
    }

    /// Which channel feeds `speaker`, or `None` if none does (and for an
    /// ambisonic set).
    #[must_use]
    pub fn index_of(&self, speaker: Speaker) -> Option<usize> {
        self.speakers()?.index_of(speaker)
    }

    /// The LFE channel's index, if there is one.
    #[must_use]
    pub fn lfe_index(&self) -> Option<usize> {
        self.speakers()?.lfe_index()
    }

    /// Whether no position is [`Unknown`](Speaker::Unknown); an ambisonic set
    /// has no positions to be unknown.
    #[must_use]
    pub fn is_fully_named(&self) -> bool {
        self.speakers().is_none_or(Speakers::is_fully_named)
    }

    /// Whether this is speakers, not empty, and every channel is
    /// [`Unassigned`](Speaker::Unassigned).
    #[must_use]
    pub fn is_unassigned(&self) -> bool {
        self.speakers().is_some_and(Speakers::is_unassigned)
    }

    /// `width` channels that feed no speaker, or `None` past [`MAX_CHANNELS`].
    ///
    /// For an edge whose channels are numbered rather than placed (a 16-input
    /// interface, a multitrack file), or a node that states only a count. A
    /// fold maps them one to one and never through a surround matrix. A signal
    /// wider than [`MAX_CHANNELS`] keeps its width and has no topology, which a
    /// fold reads as unassigned.
    #[must_use]
    pub const fn unassigned(width: ChannelLayout) -> Option<Self> {
        match Speakers::unassigned(width) {
            Some(speakers) => Some(Self::Speakers(speakers)),
            None => None,
        }
    }
}

impl From<Speakers> for ChannelTopology {
    fn from(speakers: Speakers) -> Self {
        Self::Speakers(speakers)
    }
}

impl From<AmbisonicFormat> for ChannelTopology {
    fn from(format: AmbisonicFormat) -> Self {
        Self::Ambisonic(format)
    }
}

/// Where a reported topology came from.
///
/// An edge that cannot know a placement (a device, a container that states a
/// count and no placement) calls [`ChannelTopology::default_for`] and says so with
/// [`Default`](Self::Default); an edge that read it from the format says
/// [`Stated`](Self::Stated).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Placement {
    /// The format, the device or the caller stated it.
    Stated,
    /// A count became a placement through [`ChannelTopology::default_for`].
    Default,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AmbisonicNormalization, AmbisonicOrder, AmbisonicOrdering, Bed};

    /// Every named speaker except `Unknown`, which carries a payload.
    const NAMED: [Speaker; 21] = [
        Speaker::FrontLeft,
        Speaker::FrontRight,
        Speaker::FrontCenter,
        Speaker::LowFrequency,
        Speaker::BackLeft,
        Speaker::BackRight,
        Speaker::FrontLeftOfCenter,
        Speaker::FrontRightOfCenter,
        Speaker::BackCenter,
        Speaker::SideLeft,
        Speaker::SideRight,
        Speaker::TopCenter,
        Speaker::TopFrontLeft,
        Speaker::TopFrontCenter,
        Speaker::TopFrontRight,
        Speaker::TopBackLeft,
        Speaker::TopBackCenter,
        Speaker::TopBackRight,
        Speaker::TopSideLeft,
        Speaker::TopSideRight,
        Speaker::Unassigned,
    ];

    /// The mirror is an involution with no fixed point, and pairs a left with
    /// a right: each paired speaker's name says one side and its partner's the
    /// other, so a pair never crosses depth or height.
    #[test]
    fn the_mirror_pairs_each_left_with_its_right_and_back() {
        let mut paired = 0;
        for s in NAMED {
            let Some(m) = s.mirror() else { continue };
            paired += 1;
            assert_ne!(m, s, "{s:?} is its own mirror");
            assert_eq!(m.mirror(), Some(s), "{s:?} -> {m:?} does not return");
            let (name, partner) = (format!("{s:?}"), format!("{m:?}"));
            let swapped = if name.contains("Left") {
                name.replace("Left", "Right")
            } else {
                name.replace("Right", "Left")
            };
            assert_eq!(swapped, partner, "{s:?} mirrors to {m:?}");
        }
        assert_eq!(paired, 14, "seven left/right pairs");
        assert_eq!(Speaker::Unknown(7).mirror(), None);
    }

    /// A speaker has a mirror exactly when it is on one side, and its mirror is
    /// on the other: the side predicates and the mirror describe one table.
    #[test]
    fn a_speaker_with_a_mirror_is_on_one_side_and_its_mirror_on_the_other() {
        let mut lefts = 0;
        for s in NAMED {
            assert!(!(s.is_left() && s.is_right()), "{s:?} on both sides");
            assert_eq!(
                s.mirror().is_some(),
                s.is_left() || s.is_right(),
                "{s:?}: a mirror iff a side"
            );
            if let Some(m) = s.mirror() {
                assert_eq!(s.is_left(), m.is_right(), "{s:?} / {m:?}");
            }
            lefts += usize::from(s.is_left());
        }
        assert_eq!(lefts, 7, "seven left speakers");
        assert!(!Speaker::Unknown(3).is_left() && !Speaker::Unknown(3).is_right());
    }

    /// Wherever a bed places a left speaker, its azimuth is positive (tutti's
    /// convention: 90 is left), and a right one's negative.
    #[test]
    fn a_bed_places_sided_speakers_at_the_azimuth_sign_of_their_side() {
        let mut checked = 0;
        for bed in Bed::ALL {
            for s in NAMED {
                let Some(d) = bed.direction(s) else { continue };
                if s.is_left() {
                    assert!(d.azimuth.get() > 0.0, "{bed:?}: {s:?} at {:?}", d.azimuth);
                    checked += 1;
                } else if s.is_right() {
                    assert!(d.azimuth.get() < 0.0, "{bed:?}: {s:?} at {:?}", d.azimuth);
                    checked += 1;
                }
            }
        }
        assert!(checked >= 20, "only {checked} placed sided speakers");
    }

    /// On the median plane or unplaced, a speaker has no mirror.
    #[test]
    fn centres_the_lfe_and_unplaced_channels_have_no_mirror() {
        for s in [
            Speaker::FrontCenter,
            Speaker::LowFrequency,
            Speaker::BackCenter,
            Speaker::TopCenter,
            Speaker::TopFrontCenter,
            Speaker::TopBackCenter,
            Speaker::Unassigned,
        ] {
            assert_eq!(s.mirror(), None, "{s:?}");
        }
    }

    /// Wherever a bed places both a speaker and its mirror, they sit at
    /// opposite azimuths and the same elevation: the mirror is the reflection
    /// the bed table draws.
    #[test]
    fn a_bed_places_a_speaker_and_its_mirror_at_opposite_azimuths() {
        let mut checked = 0;
        for bed in Bed::ALL {
            for s in NAMED {
                let (Some(m), Some(d)) = (s.mirror(), bed.direction(s)) else {
                    continue;
                };
                let Some(dm) = bed.direction(m) else { continue };
                assert_eq!(dm.azimuth.get(), -d.azimuth.get(), "{bed:?}: {s:?} / {m:?}");
                assert_eq!(
                    dm.elevation.get(),
                    d.elevation.get(),
                    "{bed:?}: {s:?} / {m:?}"
                );
                checked += 1;
            }
        }
        assert!(checked >= 20, "only {checked} placed pairs");
    }

    fn topology(list: &[Speaker]) -> ChannelTopology {
        ChannelTopology::new(list.iter().copied()).unwrap()
    }

    /// The width is the list's length, and is never stored separately.
    #[test]
    fn the_width_is_derived_from_the_position_count() {
        let t = topology(&[Speaker::FrontLeft, Speaker::FrontRight]);
        assert_eq!(t.width(), ChannelLayout::STEREO);
        assert_eq!(t.positions().unwrap().len(), 2);
    }

    /// An ambisonic set's width is (order + 1)², from its format alone.
    #[test]
    fn an_ambisonic_width_is_derived_from_its_order() {
        for (order, width) in [(0u8, 1u16), (1, 4), (2, 9), (3, 16)] {
            let t = ChannelTopology::from(AmbisonicFormat::ambix(AmbisonicOrder::of(order)));
            assert_eq!(t.width().count(), width);
            assert_eq!(t.positions(), None);
        }
    }

    /// An empty topology is a real answer (a disabled bus), not a failure.
    #[test]
    fn an_empty_topology_is_an_empty_width() {
        let t = topology(&[]);
        assert_eq!(t.width(), ChannelLayout::EMPTY);
        assert_eq!(t.lfe_index(), None);
    }

    /// A named speaker twice is refused with the speaker; `Unassigned` and each
    /// distinct `Unknown` repeat freely, and the same `Unknown` twice is one
    /// position twice.
    #[test]
    fn a_named_speaker_may_not_repeat_and_unassigned_may() {
        use Speaker::*;
        assert_eq!(
            Speakers::new([FrontLeft, FrontRight, FrontLeft]),
            Err(TopologyError::Duplicate(FrontLeft))
        );
        assert_eq!(
            Speakers::new([LowFrequency, LowFrequency]),
            Err(TopologyError::Duplicate(LowFrequency))
        );
        assert!(Speakers::new([Unassigned; 16]).is_ok());
        assert!(Speakers::new([Unknown(1), Unknown(2), FrontLeft, Unknown(1)]).is_ok());
    }

    /// At a format boundary a repeated named speaker becomes `Unassigned` in
    /// its later slot and the rest of the map keeps its speakers and slots.
    #[test]
    fn a_format_boundary_unassigns_a_repeat_and_keeps_the_rest() {
        use Speaker::*;
        let s = Speakers::from_format([FrontLeft, FrontRight, FrontLeft, FrontCenter, FrontRight])
            .unwrap();
        assert_eq!(
            s.as_slice(),
            &[FrontLeft, FrontRight, Unassigned, FrontCenter, Unassigned]
        );
        let clean = Speakers::from_format([FrontLeft, Unassigned, Unknown(7), Unassigned]).unwrap();
        assert_eq!(
            clean.as_slice(),
            &[FrontLeft, Unassigned, Unknown(7), Unassigned]
        );
    }

    /// A topology holds sixteen channels: `new` and the serde path build a list
    /// of 16 and refuse 17 with its width, and a format boundary reports a 17-wide
    /// map as no topology rather than truncating it.
    #[test]
    fn a_topology_holds_sixteen_speakers_and_refuses_more() {
        let sixteen = [Speaker::Unassigned; 16];
        let seventeen = [Speaker::Unassigned; 17];
        assert_eq!(Speakers::new(sixteen).unwrap().len(), 16);
        assert_eq!(
            ChannelTopology::new(sixteen).ok(),
            ChannelTopology::unassigned(ChannelLayout::from(16u16))
        );
        let refused = TopologyError::TooWide { width: 17 };
        assert_eq!(Speakers::new(seventeen), Err(refused));
        assert_eq!(ChannelTopology::new(seventeen), Err(refused));
        assert_eq!(
            Speakers::try_from(seventeen.to_vec()),
            Err(refused),
            "the wire path agrees with `new`"
        );
        assert_eq!(
            Speakers::new([Speaker::Unassigned; 40]),
            Err(TopologyError::TooWide { width: 40 })
        );
        assert_eq!(Speakers::from_format(sixteen).map(|s| s.len()), Some(16));
        assert_eq!(Speakers::from_format(seventeen), None);
        assert_eq!(Speakers::from_format([Speaker::FrontLeft; 17]), None);
    }

    /// The slots past the length do not leak into equality or hash: a list is
    /// the same however it was built.
    #[test]
    fn spare_slots_are_canonical() {
        use Speaker::*;
        let a = topology(&[FrontRight, FrontLeft]);
        assert_ne!(a, topology(&[FrontRight, FrontLeft, Unassigned]));
        assert_eq!(topology(&[]), ChannelTopology::EMPTY);
        assert_eq!(
            ChannelTopology::unassigned(ChannelLayout::STEREO),
            ChannelTopology::new([Unassigned, Unassigned]).ok()
        );
        assert_eq!(topology(&[FrontCenter]), ChannelTopology::MONO);
        assert_eq!(topology(&[FrontLeft, FrontRight]), ChannelTopology::STEREO);
        let mut hashes = std::collections::HashSet::new();
        hashes.insert(a);
        assert!(hashes.contains(&topology(&[FrontRight, FrontLeft])));
    }

    /// An ambisonic format is at most third order, so none is wider than a
    /// topology holds; every bed fits too.
    #[test]
    fn no_ambisonic_format_or_bed_is_wider_than_a_topology() {
        for order in 0..=3u8 {
            let format = AmbisonicFormat::ambix(AmbisonicOrder::of(order));
            assert!(ChannelTopology::from(format).width().count() as usize <= MAX_CHANNELS);
        }
        assert!(AmbisonicOrder::new(4).is_none());
        for bed in Bed::ALL {
            assert_eq!(bed.topology().width(), bed.width());
        }
    }

    /// A width past [`MAX_CHANNELS`] has no topology from the width-only
    /// constructors, and sixteen is the widest that has one.
    #[test]
    fn a_width_past_the_cap_has_no_topology() {
        for width in [17u16, 32, u16::MAX] {
            let w = ChannelLayout::from(width);
            assert_eq!(ChannelTopology::unassigned(w), None, "{width}");
            assert_eq!(ChannelTopology::default_for(w), None, "{width}");
            assert_eq!(Speakers::unassigned(w), None, "{width}");
        }
        let sixteen = ChannelLayout::from(16u16);
        assert_eq!(
            ChannelTopology::unassigned(sixteen).unwrap().width(),
            sixteen
        );
        assert_eq!(
            ChannelTopology::default_for(sixteen).unwrap().width(),
            sixteen
        );
    }

    /// The default 5.1 puts the LFE at index 3, pinned as an absolute index
    /// (the number the downmix skips) rather than through `lfe_index` alone.
    #[test]
    fn the_default_51_order_puts_lfe_at_index_three() {
        let t = crate::Bed::Surround51.topology();
        assert_eq!(
            t.positions().unwrap(),
            &[
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::FrontCenter,
                Speaker::LowFrequency,
                Speaker::SideLeft,
                Speaker::SideRight,
            ]
        );
        assert_eq!(t.lfe_index(), Some(3));
    }

    /// The count-to-placement map: 1 mono, 2 stereo, 4 quad, 6 5.1, 8 7.1,
    /// 12 7.1.4, every other width unassigned.
    #[test]
    fn default_for_maps_the_six_widths_and_leaves_the_rest_unassigned() {
        for (width, bed) in [
            (1u16, Bed::Mono),
            (2, Bed::Stereo),
            (4, Bed::Quad),
            (6, Bed::Surround51),
            (8, Bed::Surround71),
            (12, Bed::Surround714),
        ] {
            let t = ChannelTopology::default_for(ChannelLayout::from(width)).unwrap();
            assert_eq!(t, bed.topology(), "width {width}");
            assert_eq!(t.width().count(), width);
        }
        for width in [0u16, 3, 5, 7, 9, 10, 11, 13, 16] {
            let w = ChannelLayout::from(width);
            assert_eq!(
                ChannelTopology::default_for(w).unwrap(),
                ChannelTopology::unassigned(w).unwrap()
            );
        }
        assert!(
            !ChannelTopology::default_for(ChannelLayout::EMPTY)
                .unwrap()
                .is_unassigned()
        );
    }

    /// 7.1 extends 5.1 without moving its prefix; 7.1.4 extends 7.1 with the
    /// four heights.
    #[test]
    fn the_default_orders_extend_each_other() {
        let five = crate::Bed::Surround51.topology();
        let seven = crate::Bed::Surround71.topology();
        let atmos = crate::Bed::Surround714.topology();
        assert_eq!(&seven.positions().unwrap()[..6], five.positions().unwrap());
        assert_eq!(
            &seven.positions().unwrap()[6..],
            &[Speaker::BackLeft, Speaker::BackRight]
        );
        assert_eq!(&atmos.positions().unwrap()[..8], seven.positions().unwrap());
        assert_eq!(
            &atmos.positions().unwrap()[8..],
            &[
                Speaker::TopFrontLeft,
                Speaker::TopFrontRight,
                Speaker::TopBackLeft,
                Speaker::TopBackRight
            ]
        );
    }

    /// Unassigned channels are explicit: the right count, named, and not any
    /// placed speaker.
    #[test]
    fn unassigned_channels_are_counted_named_and_unplaced() {
        let d = ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap();
        assert_eq!(d.width().count(), 6);
        assert!(d.is_unassigned() && d.is_fully_named());
        assert_eq!(d.index_of(Speaker::FrontCenter), None);
        assert!(!Bed::Surround51.topology().is_unassigned());
        assert!(
            !ChannelTopology::unassigned(ChannelLayout::EMPTY)
                .unwrap()
                .is_unassigned()
        );
        assert!(!topology(&[Speaker::Unassigned, Speaker::Unknown(1)]).is_unassigned());
        assert!(!topology(&[Speaker::Unassigned, Speaker::Unknown(1)]).is_fully_named());
    }

    /// An unknown position keeps its slot instead of collapsing the list.
    #[test]
    fn an_unknown_position_holds_its_channel_slot() {
        let t = topology(&[
            Speaker::FrontLeft,
            Speaker::Unknown(4242),
            Speaker::FrontRight,
        ]);
        assert_eq!(t.width(), ChannelLayout::from(3u16));
        assert_eq!(
            t.index_of(Speaker::FrontRight),
            Some(2),
            "the channel after an unknown one must keep its own index"
        );
        assert!(!t.is_fully_named());
    }

    /// An ambisonic set has no speaker lookups, no LFE, and nothing unknown.
    #[test]
    fn an_ambisonic_set_answers_no_speaker_question() {
        let t = ChannelTopology::from(AmbisonicFormat::new(
            AmbisonicOrder::of(1),
            AmbisonicOrdering::Acn,
            AmbisonicNormalization::N3d,
        ));
        assert_eq!(t.index_of(Speaker::FrontLeft), None);
        assert_eq!(t.lfe_index(), None);
        assert!(t.is_fully_named() && !t.is_unassigned());
        assert_eq!(
            t.ambisonic().unwrap().normalization(),
            AmbisonicNormalization::N3d
        );
        assert_eq!(topology(&[Speaker::FrontLeft]).ambisonic(), None);
    }

    /// A speaker this bus does not carry is absent, not index 0.
    #[test]
    fn a_missing_speaker_has_no_index() {
        let t = Bed::Stereo.topology();
        assert_eq!(t.index_of(Speaker::LowFrequency), None);
        assert_eq!(t.index_of(Speaker::FrontLeft), Some(0));
    }

    /// A topology survives a serde round trip, including an unknown position
    /// and an ambisonic set, and a document naming a speaker twice is refused.
    ///
    /// It rides the plugin-load IPC reply, so this is a wire path.
    #[cfg(feature = "serde")]
    #[test]
    fn a_topology_survives_a_serde_round_trip_and_refuses_a_duplicate() {
        let original = topology(&[
            Speaker::FrontLeft,
            Speaker::Unknown(4242),
            Speaker::LowFrequency,
        ]);
        tutti_fixtures::check_round_trip(
            &original,
            r#"{"Speakers":["FrontLeft",{"Unknown":4242},"LowFrequency"]}"#,
        );
        tutti_fixtures::check_round_trip(
            &ChannelTopology::from(AmbisonicFormat::ambix(AmbisonicOrder::of(1))),
            r#"{"Ambisonic":{"order":1,"ordering":"Acn","normalization":"Sn3d"}}"#,
        );
        assert!(
            serde_json::from_str::<ChannelTopology>(r#"{"Speakers":["FrontLeft","FrontLeft"]}"#)
                .is_err()
        );
    }

    /// The wire path agrees with `new`: sixteen speakers decode and seventeen are
    /// refused, and the wire form is the list.
    #[cfg(feature = "serde")]
    #[test]
    fn the_wire_path_refuses_a_seventeenth_speaker() {
        let sixteen = format!("[{}]", vec!["\"Unassigned\""; 16].join(","));
        let seventeen = format!("[{}]", vec!["\"Unassigned\""; 17].join(","));
        let decoded: Speakers = serde_json::from_str(&sixteen).unwrap();
        assert_eq!(decoded.len(), 16);
        assert_eq!(serde_json::to_string(&decoded).unwrap(), sixteen);
        let refused = serde_json::from_str::<Speakers>(&seventeen).unwrap_err();
        assert!(refused.to_string().contains("17 channels"), "{refused}");
    }

    /// Only the LFE answers `is_lfe`.
    #[test]
    fn only_the_lfe_is_the_lfe() {
        assert!(Speaker::LowFrequency.is_lfe());
        for other in [
            Speaker::FrontCenter,
            Speaker::TopCenter,
            Speaker::BackCenter,
            Speaker::Unknown(3),
        ] {
            assert!(!other.is_lfe(), "{other:?} must not report as the LFE");
        }
    }

    /// A side and a back speaker exchange with each other, both ways; nothing
    /// else has a counterpart.
    #[test]
    fn side_and_back_are_each_others_counterparts() {
        use Speaker::*;
        for (a, b) in [(SideLeft, BackLeft), (SideRight, BackRight)] {
            assert_eq!(a.counterpart(), Some(b));
            assert_eq!(b.counterpart(), Some(a));
        }
        for sp in [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            TopFrontLeft,
            Unassigned,
        ] {
            assert_eq!(sp.counterpart(), None);
        }
    }
}
