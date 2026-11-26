//! [`SyncMessages`]: what a [`SyncOut`](crate::SyncOut) sends, as one word.

use tutti_midi_protocol::{MtcRate, Recipient};
use tutti_rt::AtomicWord;
use tutti_ump::U7;

/// What a [`SyncOut`](crate::SyncOut) sends of the transport. One value in
/// one word, so the audio thread reads all of it from one load, never a mix of
/// two stores.
///
/// ```
/// use tutti_midi_protocol::{MtcRate, Recipient};
/// use tutti_midi_runtime::SyncMessages;
///
/// let messages = SyncMessages {
///     clock: true,
///     mtc: true,
///     mmc: Some(Recipient::AllCall),
///     flex: false,
///     metronome: false,
///     rate: MtcRate::Fps30,
/// };
/// assert_ne!(messages, SyncMessages::OFF);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SyncMessages {
    /// The Beat Clock: Timing Clock (24 a quarter note), Song Position and
    /// Start, Continue and Stop.
    pub clock: bool,
    /// MIDI Time Code: the quarter-frames, and a Full Message on every locate.
    /// Sent with or without the Beat Clock.
    pub mtc: bool,
    /// MIDI Machine Control: Play, Stop and Locate to this recipient, each on
    /// the frame the transport changes. `None` sends none.
    pub mmc: Option<Recipient>,
    /// The Flex Data tempo and meter: a Set Tempo (M2-104 §7.5.3) when the
    /// tempo in force changes, a Set Time Signature (§7.5.4) when the meter
    /// does, each on the Group at the times the clock's docs give. Sent with or
    /// without the Beat Clock. The meter is the one the host lends
    /// each block (`Cx::meter`).
    pub flex: bool,
    /// The Flex Data metronome: a Set Metronome (§7.5.5) on every bar boundary
    /// where the meter in force differs from the one last stated. It states the
    /// meter's click, one on each beat with the bar's first beat accented, and
    /// is sent whatever the click node's state: on, off or in another mode.
    /// It is stated again on a start, and after a locate at the next bar
    /// boundary where the meter differs from the last one stated. A meter with
    /// no Set Metronome form (a 64th-note beat) sends none, and the receiver
    /// keeps the click it was last told. Sent with or without
    /// [`flex`](Self::flex), on the same Group. The meter is the one the host lends
    /// each block (`Cx::meter`).
    pub metronome: bool,
    /// The SMPTE frame rate every time code is stated at: the quarter-frames,
    /// the Full Messages and an MMC Locate, so they cannot disagree.
    pub rate: MtcRate,
}

impl SyncMessages {
    /// Nothing sent, at 25 frames a second.
    pub const OFF: Self = Self {
        clock: false,
        mtc: false,
        mmc: None,
        flex: false,
        metronome: false,
        rate: MtcRate::Fps25,
    };

    /// Whether the Flex lane (tempo, meter, metronome) has anything on.
    pub(crate) const fn any_flex(self) -> bool {
        self.flex || self.metronome
    }

    const CLOCK: u64 = 1;
    const MTC: u64 = 1 << 1;
    const MMC: u64 = 1 << 2;
    const FLEX: u64 = 1 << 3;
    const METRONOME: u64 = 1 << 4;
    const RATE_SHIFT: u32 = 8;
    const DEVICE_SHIFT: u32 = 16;
}

impl Default for SyncMessages {
    fn default() -> Self {
        Self::OFF
    }
}

/// The word: bit 0 the Beat Clock, bit 1 MTC, bit 2 MMC present, bit 3 Flex
/// Data, bit 4 the Flex metronome, bits 8 to 15 the rate's
/// two-bit code ([`MtcRate::code`]), bits 16 to 22 the MMC recipient's device byte (`0x7F` the all-call; zero while MMC is absent).
///
/// Decoding is total: every word reads as some value, the rate from the low
/// two bits of its byte ([`MtcRate::masked`]) and the device from the low seven
/// of its field, so a word no [`to_word`](AtomicWord::to_word) wrote still
/// reads, and no arm exists for a word that cannot happen.
impl AtomicWord for SyncMessages {
    fn to_word(self) -> u64 {
        let mut word = u64::from(self.rate.code()) << Self::RATE_SHIFT;
        if self.clock {
            word |= Self::CLOCK;
        }
        if self.mtc {
            word |= Self::MTC;
        }
        if self.flex {
            word |= Self::FLEX;
        }
        if self.metronome {
            word |= Self::METRONOME;
        }
        if let Some(to) = self.mmc {
            word |= Self::MMC | u64::from(to.device_byte()) << Self::DEVICE_SHIFT;
        }
        word
    }

    fn from_word(word: u64) -> Option<Self> {
        Some(Self {
            clock: word & Self::CLOCK != 0,
            mtc: word & Self::MTC != 0,
            mmc: (word & Self::MMC != 0)
                .then(|| Recipient::of_byte(U7::masked((word >> Self::DEVICE_SHIFT) as u8))),
            flex: word & Self::FLEX != 0,
            metronome: word & Self::METRONOME != 0,
            rate: MtcRate::masked((word >> Self::RATE_SHIFT) as u8),
        })
    }
}

#[cfg(test)]
mod tests;
