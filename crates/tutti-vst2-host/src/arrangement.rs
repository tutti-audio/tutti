//! Speaker arrangement negotiation: [`Vst2Loaded::negotiate_arrangement`].
//!
//! `effSetSpeakerArrangement` proposes an input and an output arrangement to a suspended
//! plugin, and `effGetSpeakerArrangement` reads what it holds. The plugin may keep its own
//! layout, so the host reads back after every proposal and reports whether the plugin
//! kept the proposal. The two functions at the bottom of this file are the only mapping
//! between [`ChannelTopology`] and the ABI's [`SpeakerArrangement`].

use thiserror::Error;
use tutti_buffers::{ChannelTopology, Speaker, Speakers};
use tutti_plugin_types::MainThreadMarker;
use tutti_vst2_ffi::{Speaker as Vst2Speaker, SpeakerArrangement};

use crate::loaded::Vst2Loaded;

/// A proposal that was not sent.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArrangementError {
    /// The topology is ambisonic: VST 2 arrangements place speakers.
    #[error("an ambisonic set is no VST 2 speaker arrangement")]
    Ambisonic,
    /// The layout has a speaker VST 2 has no name for, so it cannot be proposed.
    #[error("{0:?} is a speaker VST 2 has no name for")]
    UnmappedSpeaker(Speaker),
    /// The plugin is resumed; arrangements are set while it is suspended.
    #[error("a speaker arrangement is set while the plugin is suspended")]
    Resumed,
}

/// How a plugin answered a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Negotiation {
    /// The plugin took both arrangements and reads back the same speakers.
    Accepted,
    /// The plugin returned 1 but gave no readable arrangement back (it answered 0 to
    /// `effGetSpeakerArrangement`, or wrote one the format does not have). Whether it kept
    /// the proposal is unobserved; the buses of
    /// [`Control::wiring`](tutti_plugin_types::Control::wiring) report the proposal.
    Unconfirmed,
    /// The plugin returned 0, or took the call and reads back something else. What it
    /// holds is on the buses of [`Control::wiring`](tutti_plugin_types::Control::wiring).
    Refused,
}

/// The arrangements a plugin held after a proposal, each `None` when the plugin gave no
/// arrangement that this vocabulary can name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Kept {
    pub(crate) input: Option<ChannelTopology>,
    pub(crate) output: Option<ChannelTopology>,
}

impl Vst2Loaded {
    /// Proposes `input` and `output` to the plugin with `effSetSpeakerArrangement`, then
    /// reads back what it holds with `effGetSpeakerArrangement`.
    ///
    /// The call goes before [`activate`](tutti_plugin_types::Loaded::activate), while the
    /// plugin is suspended. A plugin that refuses keeps its own layout: the buses of
    /// [`wiring`](tutti_plugin_types::Control::wiring) carry the arrangement it kept, with
    /// no placement for a bus whose arrangement cannot be named or does not match the
    /// plugin's pin count.
    ///
    /// # Errors
    ///
    /// [`ArrangementError::UnmappedSpeaker`] or
    /// [`ArrangementError::Ambisonic`] when a layout cannot be written in the ABI, and
    /// [`ArrangementError::Resumed`] when the plugin is resumed. Nothing is sent in any
    /// of them.
    pub fn negotiate_arrangement(
        &mut self,
        main: MainThreadMarker,
        input: &ChannelTopology,
        output: &ChannelTopology,
    ) -> Result<Negotiation, ArrangementError> {
        let wanted_input = arrangement_of(input)?;
        let wanted_output = arrangement_of(output)?;
        if self.instance.is_resumed() {
            return Err(ArrangementError::Resumed);
        }

        let returned_one =
            self.instance
                .set_speaker_arrangement(main, &wanted_input, &wanted_output);
        let held = self.instance.speaker_arrangement(main).ok();
        let (verdict, kept) = match (returned_one, held) {
            (false, Some((i, o))) => (
                Negotiation::Refused,
                Kept {
                    input: topology_of(&i),
                    output: topology_of(&o),
                },
            ),
            (false, None) => (Negotiation::Refused, Kept::default()),
            // The speakers are compared, not the type: a plugin that reads back the right
            // speakers under another `kSpeakerArr*` keeps the layout.
            (true, Some((i, o))) => (
                if same_speakers(&wanted_input, &i) && same_speakers(&wanted_output, &o) {
                    Negotiation::Accepted
                } else {
                    Negotiation::Refused
                },
                Kept {
                    input: topology_of(&i),
                    output: topology_of(&o),
                },
            ),
            // Nothing to observe: the buses report the proposal the plugin agreed to.
            (true, None) => (
                Negotiation::Unconfirmed,
                Kept {
                    input: Some(*input),
                    output: Some(*output),
                },
            ),
        };
        self.kept = Some(kept);
        Ok(verdict)
    }
}

/// Whether `held` places the speakers `wanted` does, in the same order: the type is not
/// compared, so the right speakers under another `kSpeakerArr*` are the same layout.
fn same_speakers(wanted: &SpeakerArrangement, held: &SpeakerArrangement) -> bool {
    wanted.speakers() == held.speakers()
}

/// The arrangement that places each channel of `topology` at the speaker VST 2 names it.
///
/// # Errors
///
/// [`ArrangementError::UnmappedSpeaker`] for a speaker with no VST 2 name
/// ([`Speaker::Unknown`] and the top-side pair), and [`ArrangementError::Ambisonic`] for an
/// ambisonic set. A topology is at most [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS) wide,
/// inside `tutti_vst2_ffi::MAX_ARRANGEMENT_CHANNELS`.
pub(crate) fn arrangement_of(
    topology: &ChannelTopology,
) -> Result<SpeakerArrangement, ArrangementError> {
    let positions = topology.positions().ok_or(ArrangementError::Ambisonic)?;
    // The engine's one-channel bus is the centre, and VST 2's is `kSpeakerM`.
    if positions == [Speaker::FrontCenter] {
        return Ok(SpeakerArrangement::new(vec![Vst2Speaker::Mono]));
    }
    positions
        .iter()
        .map(|&speaker| match speaker {
            Speaker::FrontLeft => Ok(Vst2Speaker::Left),
            Speaker::FrontRight => Ok(Vst2Speaker::Right),
            Speaker::FrontCenter => Ok(Vst2Speaker::Center),
            Speaker::LowFrequency => Ok(Vst2Speaker::Lfe),
            // Channel order, not position: the engine's 5.1 and 7.1 beds put the side pair
            // where VST 2's `kSpeakerArr51` and `Music71` put `Ls`/`Rs`, and the back pair
            // where `Music71` puts `Sl`/`Sr`, so the mapping aligns the engine's order with
            // the SDK's named arrangements.
            Speaker::SideLeft => Ok(Vst2Speaker::LeftSurround),
            Speaker::SideRight => Ok(Vst2Speaker::RightSurround),
            Speaker::BackLeft => Ok(Vst2Speaker::SideLeft),
            Speaker::BackRight => Ok(Vst2Speaker::SideRight),
            Speaker::FrontLeftOfCenter => Ok(Vst2Speaker::LeftOfCenter),
            Speaker::FrontRightOfCenter => Ok(Vst2Speaker::RightOfCenter),
            Speaker::BackCenter => Ok(Vst2Speaker::Surround),
            Speaker::TopCenter => Ok(Vst2Speaker::TopMiddle),
            Speaker::TopFrontLeft => Ok(Vst2Speaker::TopFrontLeft),
            Speaker::TopFrontCenter => Ok(Vst2Speaker::TopFrontCenter),
            Speaker::TopFrontRight => Ok(Vst2Speaker::TopFrontRight),
            Speaker::TopBackLeft => Ok(Vst2Speaker::TopRearLeft),
            Speaker::TopBackCenter => Ok(Vst2Speaker::TopRearCenter),
            Speaker::TopBackRight => Ok(Vst2Speaker::TopRearRight),
            Speaker::Unassigned => Ok(Vst2Speaker::Undefined),
            Speaker::TopSideLeft | Speaker::TopSideRight | Speaker::Unknown(_) => {
                Err(ArrangementError::UnmappedSpeaker(speaker))
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(SpeakerArrangement::new)
}

/// The topology an arrangement spells, or `None` when a speaker in it has no name in the
/// shared vocabulary (`kSpeakerLfe2`), so the bus cannot be routed by speaker, or when it
/// is wider than a topology holds.
pub(crate) fn topology_of(arrangement: &SpeakerArrangement) -> Option<ChannelTopology> {
    if arrangement.speakers() == [Vst2Speaker::Mono] {
        return Some(ChannelTopology::MONO);
    }
    arrangement
        .speakers()
        .iter()
        .map(|&speaker| match speaker {
            Vst2Speaker::Mono => Some(Speaker::FrontCenter),
            Vst2Speaker::Left => Some(Speaker::FrontLeft),
            Vst2Speaker::Right => Some(Speaker::FrontRight),
            Vst2Speaker::Center => Some(Speaker::FrontCenter),
            Vst2Speaker::Lfe => Some(Speaker::LowFrequency),
            Vst2Speaker::LeftSurround => Some(Speaker::SideLeft),
            Vst2Speaker::RightSurround => Some(Speaker::SideRight),
            Vst2Speaker::SideLeft => Some(Speaker::BackLeft),
            Vst2Speaker::SideRight => Some(Speaker::BackRight),
            Vst2Speaker::LeftOfCenter => Some(Speaker::FrontLeftOfCenter),
            Vst2Speaker::RightOfCenter => Some(Speaker::FrontRightOfCenter),
            Vst2Speaker::Surround => Some(Speaker::BackCenter),
            Vst2Speaker::TopMiddle => Some(Speaker::TopCenter),
            Vst2Speaker::TopFrontLeft => Some(Speaker::TopFrontLeft),
            Vst2Speaker::TopFrontCenter => Some(Speaker::TopFrontCenter),
            Vst2Speaker::TopFrontRight => Some(Speaker::TopFrontRight),
            Vst2Speaker::TopRearLeft => Some(Speaker::TopBackLeft),
            Vst2Speaker::TopRearCenter => Some(Speaker::TopBackCenter),
            Vst2Speaker::TopRearRight => Some(Speaker::TopBackRight),
            Vst2Speaker::Undefined => Some(Speaker::Unassigned),
            Vst2Speaker::Lfe2 => None,
        })
        .collect::<Option<Vec<_>>>()
        .and_then(|speakers| Speakers::from_format(speakers).map(ChannelTopology::Speakers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::{AmbisonicFormat, AmbisonicOrder, Bed};
    use tutti_vst2_ffi::SpeakerArrangementType as Kind;

    /// The engine's own beds map to the SDK's named arrangements: `kSpeakerArrMono`,
    /// `Stereo`, `51`, `Music71`, and a user-defined one for 7.1.4, which no `kSpeakerArr*`
    /// names.
    #[test]
    fn the_engines_orders_are_the_sdks_named_arrangements() {
        let kind = |bed: Bed| arrangement_of(&bed.topology()).expect("mapped").kind();
        assert_eq!(kind(Bed::Mono), Kind::Mono);
        assert_eq!(kind(Bed::Stereo), Kind::Stereo);
        assert_eq!(kind(Bed::Surround51), Kind::Surround51);
        assert_eq!(kind(Bed::Surround71), Kind::Music71);
        assert_eq!(kind(Bed::Surround714), Kind::Custom);
    }

    /// A layout with a speaker VST 2 has no name for is refused, naming the speaker, and
    /// no arrangement is built around it.
    #[test]
    fn a_speaker_without_a_vst2_name_is_refused() {
        for speaker in [
            Speaker::TopSideLeft,
            Speaker::TopSideRight,
            Speaker::Unknown(3),
        ] {
            let topology = ChannelTopology::new([Speaker::FrontLeft, speaker]).expect("no repeat");
            assert_eq!(
                arrangement_of(&topology).unwrap_err(),
                ArrangementError::UnmappedSpeaker(speaker)
            );
        }
    }

    /// The right speakers under another type are the same layout, and other speakers are
    /// not, whatever the type.
    #[test]
    fn a_layout_is_its_speakers_and_not_its_type() {
        let stereo = SpeakerArrangement::new(vec![Vst2Speaker::Left, Vst2Speaker::Right]);
        let mut words = stereo.encode();
        words[0] = -2i32 as u32; // kSpeakerArrUserDefined
        let retyped = SpeakerArrangement::decode(&words).expect("a user-defined pair");
        assert_eq!(retyped.kind(), Kind::Custom);
        assert!(same_speakers(&stereo, &retyped));

        let swapped = SpeakerArrangement::new(vec![Vst2Speaker::Right, Vst2Speaker::Left]);
        assert!(!same_speakers(&stereo, &swapped));
    }

    /// An ambisonic set is refused: its channels are no speakers.
    #[test]
    fn an_ambisonic_set_is_refused() {
        let first_order = ChannelTopology::Ambisonic(AmbisonicFormat::ambix(AmbisonicOrder::of(1)));
        assert_eq!(
            arrangement_of(&first_order).unwrap_err(),
            ArrangementError::Ambisonic
        );
    }

    /// Each way is the other's inverse over every layout both can name, and `kSpeakerLfe2`,
    /// which the shared vocabulary has no speaker for, reads back as no topology.
    #[test]
    fn the_two_mappings_are_inverse_and_lfe2_is_unnamed() {
        let orders = [
            vec![Speaker::FrontCenter],
            vec![Speaker::FrontLeft, Speaker::FrontRight],
            vec![Speaker::FrontRight, Speaker::FrontLeft],
            vec![
                Speaker::FrontLeft,
                Speaker::FrontRight,
                Speaker::FrontCenter,
                Speaker::LowFrequency,
                Speaker::SideLeft,
                Speaker::SideRight,
                Speaker::FrontLeftOfCenter,
                Speaker::FrontRightOfCenter,
                Speaker::BackCenter,
                Speaker::BackLeft,
                Speaker::BackRight,
                Speaker::TopCenter,
            ],
            vec![
                Speaker::TopFrontLeft,
                Speaker::TopFrontCenter,
                Speaker::TopFrontRight,
                Speaker::TopBackLeft,
                Speaker::TopBackCenter,
                Speaker::TopBackRight,
                Speaker::Unassigned,
            ],
        ];
        for order in orders {
            let topology = ChannelTopology::new(order).expect("no repeat");
            let arrangement = arrangement_of(&topology).expect("mapped");
            assert_eq!(topology_of(&arrangement), Some(topology));
        }
        let ten_two = SpeakerArrangement::new(vec![
            Vst2Speaker::Left,
            Vst2Speaker::Right,
            Vst2Speaker::Lfe2,
        ]);
        assert_eq!(topology_of(&ten_two), None);
    }

    /// 5.1 is `L R C Lfe Ls Rs` on the wire (`kSpeakerArr51`), and the engine's 5.1 puts
    /// its side pair where VST 2 puts `Ls`/`Rs`: pinned as the SDK's numbers, not through
    /// the mapping under test.
    #[test]
    fn five_one_is_the_sdks_speaker_numbers() {
        let words = arrangement_of(&Bed::Surround51.topology())
            .expect("mapped")
            .encode();
        let speakers: Vec<u32> = (0..6).map(|i| words[2 + 28 * i + 20]).collect();
        assert_eq!(speakers, [1, 2, 3, 4, 5, 6]);
        assert_eq!(words[0], 15, "kSpeakerArr51");
    }
}
