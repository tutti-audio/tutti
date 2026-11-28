//! [`Bed`]: the closed set of speaker layouts, positioned by ITU-R BS.2051.

use crate::{ChannelLayout, ChannelTopology, MAX_CHANNELS, Speaker};
use tutti_units::{Azimuth, Direction, Elevation};

/// A speaker layout of a known shape, in tutti's channel order.
///
/// # Positions
///
/// A [`Speaker`] names a role, and BS.2051 names a loudspeaker by its nominal
/// angle, so one `Speaker` is two loudspeakers: **`SideLeft` is M+110 in the
/// 5.x beds and M+090 in the 7.x beds, where `BackLeft` is M+135.** The
/// position of a speaker is therefore read per bed: [`direction`](Self::direction)
/// and [`label`](Self::label) answer from the one table in this module, each row
/// naming its BS.2051 label (`M` the middle layer at 0 degrees of elevation, `U`
/// the upper at +30, `B` the bottom at -30; the number is the azimuth, positive to
/// the left). The nominal figures are BS.2051-3's Table 1; the ranges of its
/// Tables 3 to 12 are room tolerances, not figures.
///
/// | Bed | BS.2051 |
/// |---|---|
/// | `Stereo` | system A (0+2+0) |
/// | `Surround50`, `Surround51` | system B (0+5+0) |
/// | `Surround512` | system C (2+5+0) |
/// | `Surround514` | system D (4+5+0) |
/// | `Surround70`, `Surround71` | system I (0+7+0) |
/// | `Surround714` | system J (4+7+0) |
/// | `Mono`, `Quad`, `Surround712` | none: tutti's convention |
///
/// The 5.0 and 7.0 beds are 5.1 and 7.1 without the LFE. The convention for the
/// three that BS.2051 has no system for: mono at 0 degrees, quad at 45 and 135
/// degrees either side, and 7.1.2 as system J without its top back pair (U±135),
/// keeping the top front pair (U±045).
///
/// ```
/// use tutti_buffers::{Bed, Speaker};
///
/// assert_eq!(Bed::Surround51.label(Speaker::SideLeft), Some("M+110"));
/// assert_eq!(Bed::Surround71.label(Speaker::SideLeft), Some("M+090"));
/// assert_eq!(Bed::Surround71.label(Speaker::BackLeft), Some("M+135"));
/// assert_eq!(Bed::Surround71.direction(Speaker::LowFrequency), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Bed {
    /// One channel at the front centre.
    Mono,
    /// `FL FR`.
    Stereo,
    /// `FL FR BL BR`.
    Quad,
    /// 5.0: `FL FR FC SL SR`.
    Surround50,
    /// 5.1: `FL FR FC LFE SL SR`.
    Surround51,
    /// 7.0: `FL FR FC SL SR BL BR`.
    Surround70,
    /// 7.1: `FL FR FC LFE SL SR BL BR`.
    Surround71,
    /// 5.1.2: 5.1 then `TFL TFR`.
    Surround512,
    /// 5.1.4: 5.1 then `TFL TFR TBL TBR`.
    Surround514,
    /// 7.1.2: 7.1 then `TFL TFR`.
    Surround712,
    /// 7.1.4: 7.1 then `TFL TFR TBL TBR`.
    Surround714,
}

/// One row of the position table: a speaker of a bed and where BS.2051 puts it.
#[derive(Debug, Clone, Copy)]
struct Row {
    speaker: Speaker,
    /// The BS.2051 loudspeaker label, `M+110`; the LFE's is `LFE1`.
    label: &'static str,
    /// The nominal direction, `None` for the LFE.
    direction: Option<Direction>,
}

/// A row at the azimuth and elevation, in degrees, BS.2051 Table 1 gives it.
const fn at(speaker: Speaker, label: &'static str, azimuth: f64, elevation: f64) -> Row {
    Row {
        speaker,
        label,
        direction: Some(Direction::new(
            Azimuth::new(azimuth),
            Elevation::new(elevation),
        )),
    }
}

use Speaker::{
    BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, LowFrequency, SideLeft, SideRight,
    TopBackLeft, TopBackRight, TopFrontLeft, TopFrontRight,
};

const C: Row = at(FrontCenter, "M+000", 0.0, 0.0);
const LFE: Row = Row {
    speaker: LowFrequency,
    label: "LFE1",
    direction: None,
};
const L30: Row = at(FrontLeft, "M+030", 30.0, 0.0);
const R30: Row = at(FrontRight, "M-030", -30.0, 0.0);
const L45: Row = at(FrontLeft, "M+045", 45.0, 0.0);
const R45: Row = at(FrontRight, "M-045", -45.0, 0.0);
const SL110: Row = at(SideLeft, "M+110", 110.0, 0.0);
const SR110: Row = at(SideRight, "M-110", -110.0, 0.0);
const SL90: Row = at(SideLeft, "M+090", 90.0, 0.0);
const SR90: Row = at(SideRight, "M-090", -90.0, 0.0);
const BL135: Row = at(BackLeft, "M+135", 135.0, 0.0);
const BR135: Row = at(BackRight, "M-135", -135.0, 0.0);
const TFL30: Row = at(TopFrontLeft, "U+030", 30.0, 30.0);
const TFR30: Row = at(TopFrontRight, "U-030", -30.0, 30.0);
const TFL45: Row = at(TopFrontLeft, "U+045", 45.0, 30.0);
const TFR45: Row = at(TopFrontRight, "U-045", -45.0, 30.0);
const TBL110: Row = at(TopBackLeft, "U+110", 110.0, 30.0);
const TBR110: Row = at(TopBackRight, "U-110", -110.0, 30.0);
const TBL135: Row = at(TopBackLeft, "U+135", 135.0, 30.0);
const TBR135: Row = at(TopBackRight, "U-135", -135.0, 30.0);

/// Quad's rows take the 45 and 135 degree convention, as no BS.2051 system
/// states a quad.
const QUAD: [Row; 4] = [L45, R45, BL135, BR135];
const MONO: [Row; 1] = [C];
const STEREO: [Row; 2] = [L30, R30];
const S50: [Row; 5] = [L30, R30, C, SL110, SR110];
const S51: [Row; 6] = [L30, R30, C, LFE, SL110, SR110];
const S70: [Row; 7] = [L30, R30, C, SL90, SR90, BL135, BR135];
const S71: [Row; 8] = [L30, R30, C, LFE, SL90, SR90, BL135, BR135];
const S512: [Row; 8] = [L30, R30, C, LFE, SL110, SR110, TFL30, TFR30];
const S514: [Row; 10] = [L30, R30, C, LFE, SL110, SR110, TFL30, TFR30, TBL110, TBR110];
const S712: [Row; 10] = [L30, R30, C, LFE, SL90, SR90, BL135, BR135, TFL45, TFR45];
const S714: [Row; 12] = [
    L30, R30, C, LFE, SL90, SR90, BL135, BR135, TFL45, TFR45, TBL135, TBR135,
];

impl Bed {
    /// Every bed, narrowest first.
    pub const ALL: [Bed; 11] = [
        Self::Mono,
        Self::Stereo,
        Self::Quad,
        Self::Surround50,
        Self::Surround51,
        Self::Surround70,
        Self::Surround71,
        Self::Surround512,
        Self::Surround514,
        Self::Surround712,
        Self::Surround714,
    ];

    fn rows(self) -> &'static [Row] {
        match self {
            Self::Mono => &MONO,
            Self::Stereo => &STEREO,
            Self::Quad => &QUAD,
            Self::Surround50 => &S50,
            Self::Surround51 => &S51,
            Self::Surround70 => &S70,
            Self::Surround71 => &S71,
            Self::Surround512 => &S512,
            Self::Surround514 => &S514,
            Self::Surround712 => &S712,
            Self::Surround714 => &S714,
        }
    }

    fn row(self, speaker: Speaker) -> Option<&'static Row> {
        self.rows().iter().find(|r| r.speaker == speaker)
    }

    /// The speakers of the bed in channel order, as a topology.
    ///
    /// # Panics
    ///
    /// Never: the widest bed is twelve channels, inside [`MAX_CHANNELS`].
    #[must_use]
    pub fn topology(self) -> ChannelTopology {
        let speakers = self.rows().iter().map(|r| r.speaker);
        // The table names no speaker twice (pinned by a test).
        ChannelTopology::Speakers(
            crate::Speakers::from_format(speakers).expect("a bed fits a topology"),
        )
    }

    /// The channel count.
    #[must_use]
    pub fn width(self) -> ChannelLayout {
        ChannelLayout::clamped(self.rows().len())
    }

    /// Where the bed puts `speaker`, or `None` for the LFE (it is not a placed
    /// speaker) and for a speaker the bed does not have.
    ///
    /// Exact to the bed's own spelling: [`Quad`](Self::Quad) has `BackLeft` and
    /// no `SideLeft`, so a caller holding a side/back counterpart of the
    /// bed's list matches it first with [`from_topology`](Self::from_topology).
    #[must_use]
    pub fn direction(self, speaker: Speaker) -> Option<Direction> {
        self.row(speaker)?.direction
    }

    /// The BS.2051 label of `speaker` in this bed (`M+110`, `U-045`; `LFE1` for
    /// the LFE), or `None` for a speaker the bed does not have.
    ///
    /// [`Mono`](Self::Mono), [`Quad`](Self::Quad) and
    /// [`Surround712`](Self::Surround712) are not BS.2051 systems: their labels
    /// name the nominal angle tutti's convention gives the speaker.
    #[must_use]
    pub fn label(self, speaker: Speaker) -> Option<&'static str> {
        self.row(speaker).map(|r| r.label)
    }

    /// The bed a topology is, or `None`.
    ///
    /// Matches the speaker **set**, whatever the order: each channel of the
    /// topology must be one row of the bed, one to one. A channel that is the
    /// [counterpart](Speaker::counterpart) of a row (a side for a back, a back
    /// for a side) stands for it only when it is not itself a row of the bed, so
    /// VST3's and AU's `FL FR SL SR` and WAVE's `FL FR BL BR` are both
    /// [`Quad`](Self::Quad), WAVE's `FL FR FC LFE BL BR` is 5.1, and WAVE's
    /// 7.1 (`FL FR FC LFE BL BR SL SR`, the other order) is
    /// [`Surround71`](Self::Surround71). In the 7.x beds both pairs are rows, so
    /// a back is never taken for a side. An ambisonic set is no bed.
    ///
    /// The answer says nothing of the channel order: the bed's own order is
    /// [`topology`](Self::topology), and what is derived from a bed
    /// ([`direction`](Self::direction), `SpeakerLayout`'s gains from a bed)
    /// follows it. A consumer holding another order maps each channel through
    /// [`index_of`](ChannelTopology::index_of) on the bed's topology.
    ///
    /// ```
    /// use tutti_buffers::{Bed, ChannelTopology, Speaker::*};
    ///
    /// let vst3_quad = ChannelTopology::new([FrontLeft, FrontRight, SideLeft, SideRight]).unwrap();
    /// let wav_quad = ChannelTopology::new([FrontLeft, FrontRight, BackLeft, BackRight]).unwrap();
    /// assert_eq!(Bed::from_topology(&vst3_quad), Some(Bed::Quad));
    /// assert_eq!(Bed::from_topology(&wav_quad), Some(Bed::Quad));
    /// ```
    #[must_use]
    pub fn from_topology(topology: &ChannelTopology) -> Option<Bed> {
        let list = topology.positions()?;
        Self::ALL.into_iter().find(|bed| {
            let rows = bed.rows();
            let mut taken = [false; MAX_CHANNELS];
            rows.len() == list.len()
                && list.iter().all(|&speaker| {
                    let at = |s: Speaker| rows.iter().position(|r| r.speaker == s);
                    let row = at(speaker).or_else(|| speaker.counterpart().and_then(at));
                    row.is_some_and(|i| !core::mem::replace(&mut taken[i], true))
                })
        })
    }
}

impl ChannelTopology {
    /// What a bare width is taken to mean at an edge that cannot know (a
    /// device, a node's outputs, a file whose container states a count and no
    /// placement): 1 mono, 2 stereo, 4 quad, 6 5.1, 8 7.1, 12 7.1.4, and
    /// [`unassigned`](Self::unassigned) channels at any other width.
    ///
    /// The one count-to-placement map: an edge that uses it reports
    /// [`Placement::Default`], and an edge that knows better states its own
    /// topology instead. The quad is a guess a four-channel device or main output
    /// invites over B-format; an unlisted width names no speaker, so a fold
    /// places each channel by index.
    ///
    /// For a node or a design built for a width and nothing more (a filter of
    /// `n` channels): what its bus states is the engine's default layout, said
    /// once here.
    ///
    /// `None` past [`MAX_CHANNELS`], where a signal has no topology.
    #[must_use]
    pub fn default_for(width: ChannelLayout) -> Option<Self> {
        let bed = match width.count() {
            1 => Bed::Mono,
            2 => Bed::Stereo,
            4 => Bed::Quad,
            6 => Bed::Surround51,
            8 => Bed::Surround71,
            12 => Bed::Surround714,
            _ => return Self::unassigned(width),
        };
        Some(bed.topology())
    }

    /// The [`Bed`] this is, up to a side/back counterpart, or `None`: see
    /// [`Bed::from_topology`].
    #[must_use]
    pub fn bed(&self) -> Option<Bed> {
        Bed::from_topology(self)
    }
}

impl From<Bed> for ChannelTopology {
    fn from(bed: Bed) -> Self {
        bed.topology()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BS.2051 loudspeaker label's nominal position, from Table 1: the layer
    /// letter gives the elevation (`M` 0, `U` +30, `B` -30) and the signed
    /// number the azimuth in degrees.
    fn table_1(label: &str) -> Direction {
        let layer = label.as_bytes()[0];
        let sign = match label.as_bytes()[1] {
            b'+' => 1.0,
            b'-' => -1.0,
            other => panic!("label {label} has sign {other}"),
        };
        let azimuth: f64 = label[2..].parse().expect("an azimuth in degrees");
        let elevation = match layer {
            b'M' => 0.0,
            b'U' => 30.0,
            b'B' => -30.0,
            other => panic!("label {label} has layer {other}"),
        };
        Direction::new(Azimuth::new(sign * azimuth), Elevation::new(elevation))
    }

    /// Every row of every bed is at the nominal figure of its own label.
    #[test]
    fn every_row_is_at_its_labels_table_1_figure() {
        for bed in Bed::ALL {
            for row in bed.rows() {
                match row.direction {
                    Some(figure) => assert_eq!(
                        figure,
                        table_1(row.label),
                        "{bed:?} {:?} labelled {}",
                        row.speaker,
                        row.label
                    ),
                    None => assert_eq!(row.label, "LFE1", "{bed:?} {:?}", row.speaker),
                }
            }
        }
    }

    /// The rows read as BS.2051's systems, written out per bed from Tables 3, 4,
    /// 5, 6, 12 and the tutti convention: the speaker and its label, in channel
    /// order.
    #[test]
    fn each_bed_is_its_systems_loudspeaker_list() {
        use Speaker::*;
        let spec: [(Bed, &[(Speaker, &str)]); 11] = [
            (Bed::Mono, &[(FrontCenter, "M+000")]),
            (Bed::Stereo, &[(FrontLeft, "M+030"), (FrontRight, "M-030")]),
            (
                Bed::Quad,
                &[
                    (FrontLeft, "M+045"),
                    (FrontRight, "M-045"),
                    (BackLeft, "M+135"),
                    (BackRight, "M-135"),
                ],
            ),
            (
                Bed::Surround50,
                &[
                    (FrontLeft, "M+030"),
                    (FrontRight, "M-030"),
                    (FrontCenter, "M+000"),
                    (SideLeft, "M+110"),
                    (SideRight, "M-110"),
                ],
            ),
            (
                Bed::Surround51,
                &[
                    (FrontLeft, "M+030"),
                    (FrontRight, "M-030"),
                    (FrontCenter, "M+000"),
                    (LowFrequency, "LFE1"),
                    (SideLeft, "M+110"),
                    (SideRight, "M-110"),
                ],
            ),
            (
                Bed::Surround70,
                &[
                    (FrontLeft, "M+030"),
                    (FrontRight, "M-030"),
                    (FrontCenter, "M+000"),
                    (SideLeft, "M+090"),
                    (SideRight, "M-090"),
                    (BackLeft, "M+135"),
                    (BackRight, "M-135"),
                ],
            ),
            (
                Bed::Surround71,
                &[
                    (FrontLeft, "M+030"),
                    (FrontRight, "M-030"),
                    (FrontCenter, "M+000"),
                    (LowFrequency, "LFE1"),
                    (SideLeft, "M+090"),
                    (SideRight, "M-090"),
                    (BackLeft, "M+135"),
                    (BackRight, "M-135"),
                ],
            ),
            (
                Bed::Surround512,
                &[
                    (FrontLeft, "M+030"),
                    (FrontRight, "M-030"),
                    (FrontCenter, "M+000"),
                    (LowFrequency, "LFE1"),
                    (SideLeft, "M+110"),
                    (SideRight, "M-110"),
                    (TopFrontLeft, "U+030"),
                    (TopFrontRight, "U-030"),
                ],
            ),
            (
                Bed::Surround514,
                &[
                    (FrontLeft, "M+030"),
                    (FrontRight, "M-030"),
                    (FrontCenter, "M+000"),
                    (LowFrequency, "LFE1"),
                    (SideLeft, "M+110"),
                    (SideRight, "M-110"),
                    (TopFrontLeft, "U+030"),
                    (TopFrontRight, "U-030"),
                    (TopBackLeft, "U+110"),
                    (TopBackRight, "U-110"),
                ],
            ),
            (
                Bed::Surround712,
                &[
                    (FrontLeft, "M+030"),
                    (FrontRight, "M-030"),
                    (FrontCenter, "M+000"),
                    (LowFrequency, "LFE1"),
                    (SideLeft, "M+090"),
                    (SideRight, "M-090"),
                    (BackLeft, "M+135"),
                    (BackRight, "M-135"),
                    (TopFrontLeft, "U+045"),
                    (TopFrontRight, "U-045"),
                ],
            ),
            (
                Bed::Surround714,
                &[
                    (FrontLeft, "M+030"),
                    (FrontRight, "M-030"),
                    (FrontCenter, "M+000"),
                    (LowFrequency, "LFE1"),
                    (SideLeft, "M+090"),
                    (SideRight, "M-090"),
                    (BackLeft, "M+135"),
                    (BackRight, "M-135"),
                    (TopFrontLeft, "U+045"),
                    (TopFrontRight, "U-045"),
                    (TopBackLeft, "U+135"),
                    (TopBackRight, "U-135"),
                ],
            ),
        ];
        assert_eq!(spec.map(|(bed, _)| bed), Bed::ALL);
        for (bed, list) in spec {
            let rows: Vec<(Speaker, &str)> =
                bed.rows().iter().map(|r| (r.speaker, r.label)).collect();
            assert_eq!(rows, list, "{bed:?}");
            let speakers: Vec<Speaker> = list.iter().map(|&(s, _)| s).collect();
            assert_eq!(bed.topology().positions().unwrap(), speakers, "{bed:?}");
            assert_eq!(usize::from(bed.width().count()), list.len());
        }
    }

    /// The same speaker sits at two angles in two beds, which is why positions
    /// are per bed.
    #[test]
    fn the_surround_pair_is_110_in_five_one_and_90_in_seven_one() {
        let az = |bed: Bed, speaker| bed.direction(speaker).unwrap().azimuth.get();
        assert_eq!(az(Bed::Surround51, Speaker::SideLeft), 110.0);
        assert_eq!(az(Bed::Surround71, Speaker::SideLeft), 90.0);
        assert_eq!(az(Bed::Surround71, Speaker::BackLeft), 135.0);
        assert_eq!(az(Bed::Surround71, Speaker::SideRight), -90.0);
        assert_eq!(az(Bed::Surround514, Speaker::TopBackLeft), 110.0);
        assert_eq!(az(Bed::Surround714, Speaker::TopBackLeft), 135.0);
        assert_eq!(az(Bed::Surround512, Speaker::TopFrontLeft), 30.0);
        assert_eq!(az(Bed::Surround712, Speaker::TopFrontLeft), 45.0);
        assert_eq!(az(Bed::Quad, Speaker::FrontLeft), 45.0);
        assert_eq!(az(Bed::Quad, Speaker::BackRight), -135.0);
        assert_eq!(az(Bed::Mono, Speaker::FrontCenter), 0.0);
    }

    /// The heights are at +30 and nothing is below the plane.
    #[test]
    fn the_upper_layer_is_at_thirty_degrees_of_elevation() {
        let top = Bed::Surround714.direction(Speaker::TopFrontLeft).unwrap();
        assert_eq!(top.elevation.get(), 30.0);
        let floor = Bed::Surround714.direction(Speaker::FrontLeft).unwrap();
        assert_eq!(floor.elevation.get(), 0.0);
    }

    /// The LFE has no direction, and neither has a speaker the bed lacks.
    #[test]
    fn the_lfe_and_an_absent_speaker_have_no_direction() {
        assert_eq!(Bed::Surround51.direction(Speaker::LowFrequency), None);
        assert_eq!(Bed::Surround51.label(Speaker::LowFrequency), Some("LFE1"));
        assert_eq!(Bed::Stereo.direction(Speaker::FrontCenter), None);
        assert_eq!(Bed::Stereo.label(Speaker::FrontCenter), None);
        assert_eq!(Bed::Quad.direction(Speaker::SideLeft), None);
        assert_eq!(Bed::Surround50.direction(Speaker::LowFrequency), None);
    }

    /// A bed's own topology is that bed, and a plugin's or file's other spelling
    /// of a quad or 5.1 is the same bed.
    #[test]
    fn a_topology_is_a_bed_up_to_a_side_back_counterpart() {
        use Speaker::*;
        for bed in Bed::ALL {
            assert_eq!(Bed::from_topology(&bed.topology()), Some(bed));
        }
        let t = |list: &[Speaker]| ChannelTopology::new(list.iter().copied()).unwrap();
        assert_eq!(
            Bed::from_topology(&t(&[FrontLeft, FrontRight, SideLeft, SideRight])),
            Some(Bed::Quad)
        );
        assert_eq!(
            Bed::from_topology(&t(&[FrontLeft, FrontRight, BackLeft, BackRight])),
            Some(Bed::Quad)
        );
        assert_eq!(
            Bed::from_topology(&t(&[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight
            ])),
            Some(Bed::Surround51)
        );
    }

    /// The match is of the speaker set: WAVE's 7.1 order (the backs before the
    /// sides) is the 7.1 bed, and so is a reordered stereo pair.
    #[test]
    fn a_bed_is_matched_whatever_the_order() {
        use Speaker::*;
        let wav_order = ChannelTopology::new([
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            SideLeft,
            SideRight,
        ])
        .unwrap();
        assert_eq!(Bed::from_topology(&wav_order), Some(Bed::Surround71));
        let swapped = ChannelTopology::new([FrontRight, FrontLeft]).unwrap();
        assert_eq!(Bed::from_topology(&swapped), Some(Bed::Stereo));
    }

    /// A quad or a 5.1 written with both spellings of its surround pair (a back
    /// left and a side right) is still that bed: each channel is one row's own
    /// spelling or its counterpart, once.
    #[test]
    fn mixed_spellings_of_a_pair_match_the_bed() {
        use Speaker::*;
        let t = |list: &[Speaker]| ChannelTopology::new(list.iter().copied()).unwrap();
        assert_eq!(
            Bed::from_topology(&t(&[FrontLeft, FrontRight, BackLeft, SideRight])),
            Some(Bed::Quad)
        );
        assert_eq!(
            Bed::from_topology(&t(&[FrontLeft, FrontRight, SideLeft, BackRight])),
            Some(Bed::Quad)
        );
        assert_eq!(
            Bed::from_topology(&t(&[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                SideRight
            ])),
            Some(Bed::Surround51)
        );
    }

    /// One channel never stands for two rows: a back is no side where the bed
    /// has both pairs, and a counterpart fills only a row the topology lacks.
    #[test]
    fn a_speaker_is_taken_once() {
        use Speaker::*;
        let two_backs = ChannelTopology::new([
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            Unassigned,
            Unassigned,
        ])
        .unwrap();
        assert_eq!(Bed::from_topology(&two_backs), None);
        // A side and a back for the quad's one back pair: the back row is taken twice.
        let twice = ChannelTopology::new([FrontLeft, FrontRight, BackLeft, SideLeft]).unwrap();
        assert_eq!(Bed::from_topology(&twice), None);
        assert_eq!(
            Bed::from_topology(&ChannelTopology::unassigned(ChannelLayout::STEREO).unwrap()),
            None
        );
        let ambisonic =
            ChannelTopology::from(crate::AmbisonicFormat::ambix(crate::AmbisonicOrder::of(1)));
        assert_eq!(Bed::from_topology(&ambisonic), None);
    }
}
