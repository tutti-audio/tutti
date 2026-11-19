//! `VstSpeakerArrangement`, the argument of `effSetSpeakerArrangement` (42) and
//! `effGetSpeakerArrangement` (69), from `aeffectx.h`.
//!
//! The ABI struct is an `i32` arrangement type, an `i32` channel count and then one
//! 112-byte `VstSpeakerProperties` per channel (`azimuth`, `elevation`, `radius` and a
//! reserved `f32`, a 64-byte name, the `i32` speaker type and 28 reserved bytes). It is
//! held here as `u32` words, so the wire form is built and read without a cast.

use std::os::raw::c_void;

use crate::api::SpeakerArrangementType;

/// The most channels an arrangement read from a plugin may name; the buffer a host
/// offers a plugin to write one into holds this many.
pub const MAX_ARRANGEMENT_CHANNELS: usize = 64;

/// Words of the header: the arrangement type and the channel count.
const HEADER_WORDS: usize = 2;
/// Words of one `VstSpeakerProperties` (112 bytes).
const SPEAKER_WORDS: usize = 28;
/// Word offset of the speaker type inside a `VstSpeakerProperties`.
const SPEAKER_TYPE_WORD: usize = 20;
/// `kSpeakerUndefined`.
const UNDEFINED: i32 = 0x7fff_ffff;
/// `kSpeakerU32`, the first of the user-defined speakers `kSpeakerU32..=kSpeakerU1`.
const FIRST_USER_DEFINED: i32 = -32;

/// A speaker position, `kSpeaker*` in `aeffectx.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Speaker {
    /// `kSpeakerM`, the mono channel.
    Mono,
    /// `kSpeakerL`.
    Left,
    /// `kSpeakerR`.
    Right,
    /// `kSpeakerC`.
    Center,
    /// `kSpeakerLfe`.
    Lfe,
    /// `kSpeakerLs`.
    LeftSurround,
    /// `kSpeakerRs`.
    RightSurround,
    /// `kSpeakerLc`.
    LeftOfCenter,
    /// `kSpeakerRc`.
    RightOfCenter,
    /// `kSpeakerS`, also spelled `kSpeakerCs`.
    Surround,
    /// `kSpeakerSl`.
    SideLeft,
    /// `kSpeakerSr`.
    SideRight,
    /// `kSpeakerTm`.
    TopMiddle,
    /// `kSpeakerTfl`.
    TopFrontLeft,
    /// `kSpeakerTfc`.
    TopFrontCenter,
    /// `kSpeakerTfr`.
    TopFrontRight,
    /// `kSpeakerTrl`.
    TopRearLeft,
    /// `kSpeakerTrc`.
    TopRearCenter,
    /// `kSpeakerTrr`.
    TopRearRight,
    /// `kSpeakerLfe2`.
    Lfe2,
    /// A channel placed at no speaker: `kSpeakerUndefined` or one of the
    /// `kSpeakerU1..=kSpeakerU32` user-defined speakers.
    Undefined,
}

impl Speaker {
    /// The `kSpeaker*` constant. [`Undefined`](Self::Undefined) is `kSpeakerUndefined`.
    #[must_use]
    pub const fn raw(self) -> i32 {
        match self {
            Self::Mono => 0,
            Self::Left => 1,
            Self::Right => 2,
            Self::Center => 3,
            Self::Lfe => 4,
            Self::LeftSurround => 5,
            Self::RightSurround => 6,
            Self::LeftOfCenter => 7,
            Self::RightOfCenter => 8,
            Self::Surround => 9,
            Self::SideLeft => 10,
            Self::SideRight => 11,
            Self::TopMiddle => 12,
            Self::TopFrontLeft => 13,
            Self::TopFrontCenter => 14,
            Self::TopFrontRight => 15,
            Self::TopRearLeft => 16,
            Self::TopRearCenter => 17,
            Self::TopRearRight => 18,
            Self::Lfe2 => 19,
            Self::Undefined => UNDEFINED,
        }
    }

    /// The speaker a `kSpeaker*` constant names, or `None` for a number that is none.
    #[must_use]
    pub const fn from_raw(raw: i32) -> Option<Self> {
        Some(match raw {
            0 => Self::Mono,
            1 => Self::Left,
            2 => Self::Right,
            3 => Self::Center,
            4 => Self::Lfe,
            5 => Self::LeftSurround,
            6 => Self::RightSurround,
            7 => Self::LeftOfCenter,
            8 => Self::RightOfCenter,
            9 => Self::Surround,
            10 => Self::SideLeft,
            11 => Self::SideRight,
            12 => Self::TopMiddle,
            13 => Self::TopFrontLeft,
            14 => Self::TopFrontCenter,
            15 => Self::TopFrontRight,
            16 => Self::TopRearLeft,
            17 => Self::TopRearCenter,
            18 => Self::TopRearRight,
            19 => Self::Lfe2,
            UNDEFINED | FIRST_USER_DEFINED..=-1 => Self::Undefined,
            _ => return None,
        })
    }
}

use Speaker::{
    Center as C, Left as L, LeftOfCenter as Lc, LeftSurround as Ls, Lfe, Lfe2, Mono as M,
    Right as R, RightOfCenter as Rc, RightSurround as Rs, SideLeft as Sl, SideRight as Sr,
    Surround as S, TopFrontCenter as Tfc, TopFrontLeft as Tfl, TopFrontRight as Tfr,
    TopRearLeft as Trl, TopRearRight as Trr,
};

/// Each named arrangement (`kSpeakerArr*`) and its speakers in channel order, from
/// `aeffectx.h`.
const NAMED: &[(SpeakerArrangementType, &[Speaker])] = &[
    (SpeakerArrangementType::Mono, &[M]),
    (SpeakerArrangementType::Stereo, &[L, R]),
    (SpeakerArrangementType::StereoSurround, &[Ls, Rs]),
    (SpeakerArrangementType::StereoCenter, &[Lc, Rc]),
    (SpeakerArrangementType::StereoSide, &[Sl, Sr]),
    (SpeakerArrangementType::StereoCLfe, &[C, Lfe]),
    (SpeakerArrangementType::Cinema30, &[L, R, C]),
    (SpeakerArrangementType::Music30, &[L, R, S]),
    (SpeakerArrangementType::Cinema31, &[L, R, C, Lfe]),
    (SpeakerArrangementType::Music31, &[L, R, Lfe, S]),
    (SpeakerArrangementType::Cinema40, &[L, R, C, S]),
    (SpeakerArrangementType::Music40, &[L, R, Ls, Rs]),
    (SpeakerArrangementType::Cinema41, &[L, R, C, Lfe, S]),
    (SpeakerArrangementType::Music41, &[L, R, Lfe, Ls, Rs]),
    (SpeakerArrangementType::Surround50, &[L, R, C, Ls, Rs]),
    (SpeakerArrangementType::Surround51, &[L, R, C, Lfe, Ls, Rs]),
    (SpeakerArrangementType::Cinema60, &[L, R, C, Ls, Rs, S]),
    (SpeakerArrangementType::Music60, &[L, R, Ls, Rs, Sl, Sr]),
    (SpeakerArrangementType::Cinema61, &[L, R, C, Lfe, Ls, Rs, S]),
    (
        SpeakerArrangementType::Music61,
        &[L, R, Lfe, Ls, Rs, Sl, Sr],
    ),
    (SpeakerArrangementType::Cinema70, &[L, R, C, Ls, Rs, Lc, Rc]),
    (SpeakerArrangementType::Music70, &[L, R, C, Ls, Rs, Sl, Sr]),
    (
        SpeakerArrangementType::Cinema71,
        &[L, R, C, Lfe, Ls, Rs, Lc, Rc],
    ),
    (
        SpeakerArrangementType::Music71,
        &[L, R, C, Lfe, Ls, Rs, Sl, Sr],
    ),
    (
        SpeakerArrangementType::Cinema80,
        &[L, R, C, Ls, Rs, Lc, Rc, S],
    ),
    (
        SpeakerArrangementType::Music80,
        &[L, R, C, Ls, Rs, S, Sl, Sr],
    ),
    (
        SpeakerArrangementType::Cinema81,
        &[L, R, C, Lfe, Ls, Rs, Lc, Rc, S],
    ),
    (
        SpeakerArrangementType::Music81,
        &[L, R, C, Lfe, Ls, Rs, S, Sl, Sr],
    ),
    (
        SpeakerArrangementType::Surround102,
        &[L, R, C, Lfe, Ls, Rs, Tfl, Tfc, Tfr, Trl, Trr, Lfe2],
    ),
];

/// A speaker arrangement: the type and the speakers in channel order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerArrangement {
    kind: SpeakerArrangementType,
    speakers: Vec<Speaker>,
}

impl SpeakerArrangement {
    /// The arrangement of `speakers`: the named `kSpeakerArr*` they spell, `Empty` for none,
    /// and `Custom` (user defined) for any other order.
    #[must_use]
    pub fn new(speakers: Vec<Speaker>) -> Self {
        let kind = if speakers.is_empty() {
            SpeakerArrangementType::Empty
        } else {
            NAMED
                .iter()
                .find(|(_, named)| *named == speakers.as_slice())
                .map_or(SpeakerArrangementType::Custom, |(kind, _)| *kind)
        };
        Self { kind, speakers }
    }

    /// The arrangement type, `kSpeakerArr*`.
    #[must_use]
    pub fn kind(&self) -> SpeakerArrangementType {
        self.kind
    }

    /// The speakers in channel order.
    #[must_use]
    pub fn speakers(&self) -> &[Speaker] {
        &self.speakers
    }

    /// The `VstSpeakerArrangement` as words: exactly the header and one
    /// `VstSpeakerProperties` per channel, the unused fields zero.
    #[must_use]
    pub fn encode(&self) -> Vec<u32> {
        let mut words = vec![0u32; HEADER_WORDS + SPEAKER_WORDS * self.speakers.len()];
        words[0] = self.kind as i32 as u32;
        words[1] = self.speakers.len() as u32;
        for (i, speaker) in self.speakers.iter().enumerate() {
            let base = HEADER_WORDS + SPEAKER_WORDS * i;
            words[base + SPEAKER_TYPE_WORD] = speaker.raw() as u32;
        }
        words
    }

    /// An arrangement read from words in the ABI layout, or `None` when the type or a
    /// speaker is no `kSpeakerArr*` or `kSpeaker*`, the channel count is negative, past
    /// [`MAX_ARRANGEMENT_CHANNELS`] or not the count the named type has, or `words` is too
    /// short for it. The speakers of a named type are its `kSpeakerArr*` row; the array is
    /// read only for a user-defined or empty one.
    #[must_use]
    pub fn decode(words: &[u32]) -> Option<Self> {
        let kind = SpeakerArrangementType::from_raw(*words.first()? as i32)?;
        let channels = usize::try_from(*words.get(1)? as i32).ok()?;
        if channels > MAX_ARRANGEMENT_CHANNELS
            || words.len() < HEADER_WORDS + SPEAKER_WORDS * channels
        {
            return None;
        }
        let speakers = match NAMED.iter().find(|(named, _)| *named == kind) {
            // A named arrangement is its table row: the speaker array of the struct is not
            // consulted, so a plugin that sets the type and the count and leaves the array
            // zeroed still reads as that arrangement.
            Some((_, row)) if row.len() == channels => row.to_vec(),
            Some(_) => return None,
            None => (0..channels)
                .map(|i| {
                    let base = HEADER_WORDS + SPEAKER_WORDS * i;
                    Speaker::from_raw(words[base + SPEAKER_TYPE_WORD] as i32)
                })
                .collect::<Option<Vec<_>>>()?,
        };
        Some(Self { kind, speakers })
    }

    /// An arrangement read through the pointer a host passed a plugin.
    ///
    /// # Safety
    ///
    /// `ptr` is null or points to memory readable for a `VstSpeakerArrangement` header and,
    /// when the header's channel count is at most [`MAX_ARRANGEMENT_CHANNELS`], for that
    /// many `VstSpeakerProperties`. A pointer not aligned to 4 bytes is refused, not read.
    #[must_use]
    pub unsafe fn from_ptr(ptr: *const c_void) -> Option<Self> {
        let words = ptr.cast::<u32>();
        if words.is_null() || !words.is_aligned() {
            return None;
        }
        // SAFETY: the caller guarantees the header is readable; `words` is aligned.
        let channels = usize::try_from(unsafe { words.add(1).read() } as i32).ok()?;
        if channels > MAX_ARRANGEMENT_CHANNELS {
            return None;
        }
        // SAFETY: the caller guarantees `channels` speakers follow the header, and `words` is
        // aligned.
        let all =
            unsafe { std::slice::from_raw_parts(words, HEADER_WORDS + SPEAKER_WORDS * channels) };
        Self::decode(all)
    }
}

/// Why a plugin's answer to `effGetSpeakerArrangement` holds no arrangement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrangementRead {
    /// The plugin returned 0: it has no arrangement to report.
    Unanswered,
    /// The plugin returned 1 and wrote a type, a speaker or a channel count that is no
    /// part of the format.
    Malformed,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each `kSpeaker*` number is the SDK's, written as a literal: a transposed arm of
    /// `raw` and `from_raw` that agree with each other still fails here.
    #[test]
    fn speaker_numbers_are_the_sdks() {
        let sdk = [
            (Speaker::Mono, 0),
            (Speaker::Left, 1),
            (Speaker::Right, 2),
            (Speaker::Center, 3),
            (Speaker::Lfe, 4),
            (Speaker::LeftSurround, 5),
            (Speaker::RightSurround, 6),
            (Speaker::LeftOfCenter, 7),
            (Speaker::RightOfCenter, 8),
            (Speaker::Surround, 9),
            (Speaker::SideLeft, 10),
            (Speaker::SideRight, 11),
            (Speaker::TopMiddle, 12),
            (Speaker::TopFrontLeft, 13),
            (Speaker::TopFrontCenter, 14),
            (Speaker::TopFrontRight, 15),
            (Speaker::TopRearLeft, 16),
            (Speaker::TopRearCenter, 17),
            (Speaker::TopRearRight, 18),
            (Speaker::Lfe2, 19),
            (Speaker::Undefined, 0x7fff_ffff),
        ];
        for (speaker, raw) in sdk {
            assert_eq!(speaker.raw(), raw, "{speaker:?}");
            assert_eq!(Speaker::from_raw(raw), Some(speaker), "{raw}");
        }
        // The user-defined speakers kSpeakerU32..=kSpeakerU1 are all undefined channels.
        for raw in -32..=-1 {
            assert_eq!(Speaker::from_raw(raw), Some(Speaker::Undefined), "{raw}");
        }
        assert_eq!(Speaker::from_raw(20), None);
        assert_eq!(Speaker::from_raw(-33), None);
    }

    /// The channel counts the SDK gives each arrangement name (`kSpeakerArr30Cine` is
    /// three channels, `kSpeakerArr102` twelve), and no two names spell the same speakers,
    /// so the type chosen for a speaker list is never ambiguous.
    #[test]
    fn each_named_arrangement_has_its_channel_count_and_a_unique_spelling() {
        let counts = [
            1, 2, 2, 2, 2, 2, 3, 3, 4, 4, 4, 4, 5, 5, 5, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 8, 9, 9, 12,
        ];
        assert_eq!(NAMED.len(), counts.len());
        for ((kind, speakers), count) in NAMED.iter().zip(counts) {
            assert_eq!(speakers.len(), count, "{kind:?}");
        }
        for (i, (_, a)) in NAMED.iter().enumerate() {
            for (_, b) in &NAMED[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Types are numbered in table order from `kSpeakerArrMono` = 0.
        for (i, (kind, _)) in NAMED.iter().enumerate() {
            assert_eq!(*kind as i32, i as i32);
        }
    }

    /// The words are the ABI layout: type, count, then 28 words a speaker with the
    /// speaker type at word 20 (offset 80: 4 floats, a 64-byte name).
    #[test]
    fn the_encoding_is_the_abi_layout() {
        let words = SpeakerArrangement::new(vec![Speaker::Left, Speaker::Right]).encode();
        assert_eq!(words.len(), 2 + 2 * 28);
        assert_eq!(words[0], 1, "kSpeakerArrStereo");
        assert_eq!(words[1], 2);
        assert_eq!(words[2 + 20], 1, "kSpeakerL");
        assert_eq!(words[2 + 28 + 20], 2, "kSpeakerR");
        assert_eq!(std::mem::size_of::<u32>() * 28, 112);
    }

    /// An order no `kSpeakerArr*` names is user defined, and an empty one is empty.
    #[test]
    fn an_unnamed_order_is_user_defined_and_none_is_empty() {
        let swapped = SpeakerArrangement::new(vec![Speaker::Right, Speaker::Left]);
        assert_eq!(swapped.kind(), SpeakerArrangementType::Custom);
        assert_eq!(
            SpeakerArrangement::new(Vec::new()).kind(),
            SpeakerArrangementType::Empty
        );
        assert_eq!(
            SpeakerArrangement::new(vec![Speaker::Mono]).kind(),
            SpeakerArrangementType::Mono
        );
    }

    /// Decoding refuses words that are not an arrangement: a speaker past `Lfe2`, a count
    /// past the buffer, and a body shorter than the count.
    #[test]
    fn decoding_refuses_what_is_not_an_arrangement() {
        let ok = SpeakerArrangement::new(vec![Speaker::Left, Speaker::Right]);
        assert_eq!(SpeakerArrangement::decode(&ok.encode()), Some(ok.clone()));

        let custom = SpeakerArrangement::new(vec![Speaker::Right, Speaker::Left]);
        let mut bad_speaker = custom.encode();
        bad_speaker[2 + 20] = 20;
        assert_eq!(SpeakerArrangement::decode(&bad_speaker), None);

        let mut bad_count = ok.encode();
        bad_count[1] = 65;
        assert_eq!(SpeakerArrangement::decode(&bad_count), None);

        let mut short = ok.encode();
        short.truncate(30);
        assert_eq!(SpeakerArrangement::decode(&short), None);

        let mut bad_kind = ok.encode();
        bad_kind[0] = 99;
        assert_eq!(SpeakerArrangement::decode(&bad_kind), None);
    }

    /// A named type is its table row whatever the speaker array holds: a plugin that
    /// writes the type and the count and leaves the array zeroed reads as `L R`, not as two
    /// `kSpeakerM`; a count other than the type's is no arrangement.
    #[test]
    fn a_named_type_reads_as_its_table_row() {
        let mut words = SpeakerArrangement::new(vec![Speaker::Left, Speaker::Right]).encode();
        words[2 + 20] = 0;
        words[2 + 28 + 20] = 0;
        assert_eq!(
            SpeakerArrangement::decode(&words).map(|a| a.speakers().to_vec()),
            Some(vec![Speaker::Left, Speaker::Right])
        );
        let mut wrong_count = SpeakerArrangement::new(vec![Speaker::Mono]).encode();
        wrong_count[0] = 1; // kSpeakerArrStereo
        assert_eq!(SpeakerArrangement::decode(&wrong_count), None);
    }

    /// A pointer is refused when it is not 4-aligned, null, or heads a count the format does
    /// not allow (past the cap, or negative), before anything past the header is read.
    #[test]
    fn from_ptr_refuses_a_misaligned_pointer_and_a_bad_count() {
        let ok = SpeakerArrangement::new(vec![Speaker::Left, Speaker::Right]);
        let words = ok.encode();
        // SAFETY: `words` is a live, aligned encoding of two channels.
        let read = unsafe { SpeakerArrangement::from_ptr(words.as_ptr().cast()) };
        assert_eq!(read, Some(ok));

        let mut bytes = vec![0u8; 4 * words.len() + 1];
        for (i, word) in words.iter().enumerate() {
            bytes[1 + 4 * i..5 + 4 * i].copy_from_slice(&word.to_ne_bytes());
        }
        // Whatever the allocation's alignment, one of these two is not 4-aligned.
        let odd = [bytes.as_ptr(), bytes[1..].as_ptr()]
            .into_iter()
            .find(|p| !p.cast::<u32>().is_aligned())
            .expect("a byte offset of 1 breaks the alignment");
        // SAFETY: the buffer holds the whole encoding past `odd`; only the alignment is
        // under test, and a misaligned pointer is refused before any read.
        let misaligned = unsafe { SpeakerArrangement::from_ptr(odd.cast()) };
        assert_eq!(misaligned, None);
        // SAFETY: null is refused.
        let null = unsafe { SpeakerArrangement::from_ptr(std::ptr::null()) };
        assert_eq!(null, None);

        for count in [65u32, u32::MAX] {
            let header = [1u32, count];
            // SAFETY: the header is readable, and a count the format forbids is refused
            // before the speakers are.
            let read = unsafe { SpeakerArrangement::from_ptr(header.as_ptr().cast()) };
            assert_eq!(read, None, "{count}");
        }
    }
}
