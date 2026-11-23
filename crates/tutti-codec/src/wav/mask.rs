//! The speaker mask a multichannel WAV file declares.
//!
//! A WAVE file stores channels in ascending speaker-bit order, so a mask can
//! describe a [`ChannelTopology`] only when its speakers already are in that
//! order. [`channel_mask`] returns the mask of such a topology, and `0` (the
//! "unassigned" mask: the channels are anonymous rather than mislabelled) when
//! a speaker has no WAVE bit or the order differs. Samples are never
//! reordered, so file channel `n` is always the engine's channel `n`.
//!
//! A spec built from a width alone (`Spec::of_width`) takes the default
//! topology ([`ChannelTopology::default_for`]): 1 channel is `0x4`
//! (`KSAUDIO_SPEAKER_MONO`), 2 is `0x3`, 4 is `0x33` (`KSAUDIO_SPEAKER_QUAD`),
//! 6 is `0x60F` (`KSAUDIO_SPEAKER_5POINT1_SURROUND`), and every other width is
//! `0`, including 8 and 12 (tutti's 7.1 is `... SL SR BL BR`, WAVE's would be
//! `... BL BR SL SR`).

use crate::wave_mask::bit_of;
use tutti_buffers::ChannelTopology;

/// The speaker mask a WAV file declares for `topology`, or `0` (unassigned)
/// when a speaker has no WAVE bit or the speakers are not in ascending bit
/// order, which is the only order a WAVE file can state.
pub(crate) fn channel_mask(topology: &ChannelTopology) -> u32 {
    let mut mask = 0u32;
    // An ambisonic set names no speaker, so no WAVE bit.
    let Some(positions) = topology.positions() else {
        return 0;
    };
    for &speaker in positions {
        match bit_of(speaker) {
            Some(bit) if bit > mask => mask |= bit,
            _ => return 0,
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_buffers::Speaker;

    /// The masks are the `ksmedia.h` constants for the default-layout widths whose
    /// order WAVE can state (mono, stereo, quad, 5.1), and `0` for 7.1 and 7.1.4
    /// (tutti's 7.1 is SL SR BL BR, WAVE's would be BL BR SL SR), for unnamed
    /// widths, for a topology out of ascending order or with a speaker WAVE has
    /// no bit for, and for an ambisonic set.
    #[test]
    fn masks_match_ksmedia_and_are_zero_where_they_cannot_be_stated() {
        let m = |n: u16| {
            channel_mask(
                &ChannelTopology::default_for(tutti_buffers::ChannelLayout::from(n)).unwrap(),
            )
        };
        assert_eq!(m(1), 0x4);
        assert_eq!(m(2), 0x3);
        assert_eq!(m(4), 0x33);
        assert_eq!(m(6), 0x60F);
        assert_eq!(m(8), 0);
        assert_eq!(m(3), 0);
        assert_eq!(m(12), 0);
        use Speaker::*;
        let t = |list: &[Speaker]| ChannelTopology::new(list.iter().copied()).unwrap();
        assert_eq!(channel_mask(&t(&[FrontRight, FrontLeft])), 0);
        assert_eq!(channel_mask(&t(&[FrontLeft, TopSideLeft])), 0);
        assert_eq!(channel_mask(&t(&[FrontLeft, Unknown(3)])), 0);
        assert_eq!(
            channel_mask(
                &tutti_buffers::AmbisonicFormat::ambix(tutti_buffers::AmbisonicOrder::of(1)).into()
            ),
            0
        );
    }
}
