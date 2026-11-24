//! [`PlayReading`]: the graph's play clock at one frame.
//!
//! A value type, so a crate that names no graph can carry one, for example to
//! date a rate edit's anchor from an executor's last reading.

use super::{Frame, Samples};

/// Bits of an epoch: a [`PlayReading`] packs into one `u64` as 24 bits of
/// epoch and 40 of play (`2^40` frames is 265 days at 48 kHz).
const EPOCH_BITS: u32 = 24;
const EPOCH_MASK: u32 = (1 << EPOCH_BITS) - 1;
const PLAY_MASK: u64 = (1 << (64 - EPOCH_BITS)) - 1;

/// The play clock at one frame: which play start it counts from (`epoch`)
/// and frames since then (`play`).
///
/// `play` is a [`Frame`], a count, not `Seconds`: a play position is exact in
/// frames at any rate, where a time needs the rate to name one. Epochs are compared for equality only (a
/// rate anchor dated in one epoch is ignored in another); they
/// wrap after [`EPOCHS`](Self::EPOCHS) play starts in one process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PlayReading {
    /// Which play start `play` counts from, below [`EPOCHS`](Self::EPOCHS).
    pub epoch: u32,
    /// Frames since that play start (or since the executor was built, before
    /// its first start).
    pub play: Frame,
}

impl PlayReading {
    /// How many distinct epochs there are: `2^24`.
    pub const EPOCHS: u32 = 1 << EPOCH_BITS;

    /// The reading `frames` later, in the same epoch.
    #[must_use]
    pub fn after(self, frames: Samples) -> Self {
        Self {
            epoch: self.epoch,
            play: self.play + frames,
        }
    }

    /// One word: 24 bits of epoch high, 40 of play low (play wraps past
    /// `2^40` frames, 265 days at 48 kHz).
    #[must_use]
    pub const fn pack(self) -> u64 {
        ((self.epoch as u64 & EPOCH_MASK as u64) << (64 - EPOCH_BITS))
            | (self.play.get() & PLAY_MASK)
    }

    /// The reading [`pack`](Self::pack) made `w` from.
    pub const fn unpack(w: u64) -> Self {
        Self {
            epoch: (w >> (64 - EPOCH_BITS)) as u32,
            play: Frame::new(w & PLAY_MASK),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAY_SPAN: u64 = 1 << 40;

    /// The word is 24 bits of epoch high and 40 of play low.
    #[test]
    fn the_epoch_packs_into_the_high_24_bits_and_play_into_the_low_40() {
        let epoch_only = PlayReading {
            epoch: 1,
            play: Frame::ZERO,
        };
        assert_eq!(epoch_only.pack(), PLAY_SPAN);
        let play_only = PlayReading {
            epoch: 0,
            play: Frame::new(1),
        };
        assert_eq!(play_only.pack(), 1);
        let top = PlayReading {
            epoch: PlayReading::EPOCHS - 1,
            play: Frame::new(PLAY_SPAN - 1),
        };
        assert_eq!(top.pack(), u64::MAX);
    }

    /// A reading whose epoch is below `EPOCHS` and whose play is below `2^40`
    /// comes back from its word unchanged, at every corner.
    #[test]
    fn a_reading_in_range_round_trips_through_its_word() {
        for epoch in [0, 1, PlayReading::EPOCHS / 2, PlayReading::EPOCHS - 1] {
            for play in [0, 1, PLAY_SPAN / 2, PLAY_SPAN - 1] {
                let r = PlayReading {
                    epoch,
                    play: Frame::new(play),
                };
                assert_eq!(PlayReading::unpack(r.pack()), r, "{r:?}");
            }
        }
    }

    /// Play wraps past `2^40` frames and the epoch past `EPOCHS`, each within
    /// its own field: neither carries into the other.
    #[test]
    fn play_and_epoch_wrap_within_their_own_fields() {
        let past_play = PlayReading {
            epoch: 5,
            play: Frame::new(PLAY_SPAN + 3),
        };
        assert_eq!(
            PlayReading::unpack(past_play.pack()),
            PlayReading {
                epoch: 5,
                play: Frame::new(3),
            }
        );
        let past_epoch = PlayReading {
            epoch: PlayReading::EPOCHS + 2,
            play: Frame::new(7),
        };
        assert_eq!(
            PlayReading::unpack(past_epoch.pack()),
            PlayReading {
                epoch: 2,
                play: Frame::new(7),
            }
        );
    }
}
