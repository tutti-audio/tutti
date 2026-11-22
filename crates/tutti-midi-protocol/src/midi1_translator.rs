//! The stateful MIDI 2.0 to MIDI 1.0 translation of M2-104 Appendix D.2: the
//! [`Midi1Translator`] an output edge runs.

use crate::scale;
use crate::{PerNoteTranslation, TranslationMode, midi1};
use members::Members;
use tutti_ump::{
    Address, BendValue, CCNumber, ControlValue, ExtremeValues, Message, Midi1Bytes, U7, U14, Ump,
    Voice1, Voice2,
};

mod members;

/// MIDI 2.0 in, MIDI 1.0 out: the translation an output edge runs, per group
/// and channel (M2-104 Appendix D.2). The inverse of [`Midi2Translator`](crate::Midi2Translator).
///
/// - **A Registered or Assignable Controller** is the four Control Changes of
///   D.2.3: each message generates a sequence of four MIDI 1.0 Protocol
///   messages, CC 101 and 100 (RPN) or CC 99 and 98 (NRPN) selecting the
///   parameter, then CC 6 and CC 38, the top 14 bits of the 32-bit value. All
///   four are sent for every message, even for the parameter the previous one
///   selected.
/// - **The data downscales** by the rule the [`Midi2Translator`](crate::Midi2Translator) upscaled with. A
///   Registered Controller of index 0 to 31 (any bank) is a fixed-point number
///   or a pair of fields, upscaled by zero-extension (M2-115 section 4.1:
///   Zero-Extension Scaling for every Registered Controller whose index is
///   0 to 31), and downscales **with rounding** (section 4.4, `rounded_u14`).
///   Every other controller was scaled
///   Min-Center-Max and downscales by the plain right shift of section 3.4
///   (`ControlValue::to_u7`), which returns the value it was upscaled from.
/// - **Null Function is not sent after a run.** D.2.3 gives four messages, not
///   five, and D.3.3 says the Null Function is not translated; the select
///   stays until the next message selects again.
/// - **A Program Change with a bank** is CC 0, CC 32, then the Program Change
///   (D.2.4): three messages every time the Bank Valid bit is 1, and the
///   Program Change alone when it is 0.
/// - **Every other message** is [`midi1()`](fn@midi1)'s: a Note On whose velocity translates
///   to 0 is 1 (D.2.1), pressure and Control Change shift, and the messages
///   with no MIDI 1.0 form (D.2.8: relative and per-note controllers) send
///   nothing.
///
/// [`new`](Self::new) is the Default Translation. [`with_mode`](Self::with_mode)
/// runs an Alternate Translation Mode (D.4): near-extreme values kept off the
/// minimum and maximum (D.1.5, [`ExtremeValues`]) and the per-note messages sent
/// as MPE on a zone's member channels (D.2.8, [`PerNoteTranslation`]). In the MPE
/// mode a Note On takes the member idle longest, or shares the member that plays
/// the fewest notes and was given a note longest ago when all play (M2-120
/// section 3.6.1); a per-note message for a note that does not sound sends
/// nothing, and every message that is not per-note goes on the zone's Manager
/// Channel. A MIDI 1.0 message passes unchanged, except in the MPE mode, where it
/// takes the rules of its MIDI 2.0 equivalent.
///
/// It holds no state of the receiver: a message's MIDI 1.0 sequence is the
/// same whatever went before. It allocates nothing after construction and holds
/// no clock.
///
/// ```
/// use tutti_midi_protocol::Midi1Translator;
/// use tutti_ump::{Address, ControlValue, Message, MidiChannel, MidiGroup, U7, Voice2};
///
/// let address = Address { group: MidiGroup::FIRST, channel: MidiChannel::FIRST };
/// // Pitch bend sensitivity (RPN 0/0), 12 semitones, zero-extended.
/// let bend_range = Message::Midi2(address, Voice2::Registered {
///     bank: U7::of(0),
///     index: U7::of(0),
///     value: ControlValue::new(12 << 25),
/// });
/// let mut translator = Midi1Translator::new();
/// let mut bytes = Vec::new();
/// translator.feed(bend_range, |b| bytes.push(b.as_slice().to_vec()));
/// // CC 101, CC 100, CC 6 (12), CC 38 (0).
/// assert_eq!(bytes, [[0xB0, 101, 0], [0xB0, 100, 0], [0xB0, 6, 12], [0xB0, 38, 0]]);
/// ```
#[derive(Clone, Debug)]
pub struct Midi1Translator {
    mode: TranslationMode,
    /// The note allocation of the MPE mode; none in any other.
    members: Option<Members>,
}

impl Default for Midi1Translator {
    fn default() -> Self {
        Self::new()
    }
}

impl Midi1Translator {
    /// The most MIDI 1.0 messages one [`feed`](Self::feed) sends. An MPE Note
    /// On sends one Note Off when it strikes its key again, then a centred Pitch
    /// Bend, Channel Pressure 0 and Control Change 74 at 0x40 for each its
    /// member's last note moved (only a member that plays no other note), then
    /// the Note On. The Default Translation sends at most four, a
    /// controller's sequence (D.2.3).
    pub const MAX_MESSAGES: usize = 5;

    /// The most bytes one [`feed`](Self::feed) sends: the Note On sequence of
    /// [`MAX_MESSAGES`](Self::MAX_MESSAGES), four 3-byte messages and the
    /// 2-byte Channel Pressure. The Default Translation sends at most 12.
    pub const MAX_BYTES: usize = 14;

    /// A translator that translates by the Default Translation.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mode: TranslationMode {
                extremes: ExtremeValues::Scaled,
                per_note: PerNoteTranslation::Dropped,
            },
            members: None,
        }
    }

    /// A translator that translates by `mode`: [`new`](Self::new) is the Default
    /// Translation, and any other mode is an Alternate Translation Mode
    /// (M2-104 D.4). [`reset`](Self::reset) in the MPE mode forgets the notes
    /// held without sending their Note Offs.
    #[must_use]
    pub fn with_mode(mode: TranslationMode) -> Self {
        let members = matches!(mode.per_note, PerNoteTranslation::Mpe(_)).then(Members::new);
        Self { mode, members }
    }

    /// The mode this translator translates by.
    #[must_use]
    pub const fn mode(&self) -> TranslationMode {
        self.mode
    }

    /// Forgets the notes the MPE mode holds, without a Note Off, as at
    /// [`new`](Self::new). The Default Translation holds nothing to forget.
    pub fn reset(&mut self) {
        if let Some(members) = self.members.as_mut() {
            members.clear();
        }
    }

    /// Calls `out` with each MIDI 1.0 message `message` stands for, in the
    /// order to send them: four for a controller, three for a Program Change
    /// with a bank, up to five for an MPE Note On, one for most others, none
    /// for one with no MIDI 1.0 form (D.2.8) or that is not a channel or system
    /// message. [`MAX_MESSAGES`](Self::MAX_MESSAGES) and
    /// [`MAX_BYTES`](Self::MAX_BYTES) bound one call.
    pub fn feed(&mut self, packet: impl Into<Ump>, mut out: impl FnMut(Midi1Bytes)) {
        // A packet that does not decode has no MIDI 1.0 form.
        let Ok(message) = packet.into().decode() else {
            return;
        };
        let (address, voice) = match message {
            Message::Midi2(address, voice) => (address, voice),
            // In the MPE mode a MIDI 1.0 voice message is the MIDI 2.0 one it
            // means (M2-104 D.3) and takes the same rules; otherwise it passes
            // unchanged.
            Message::Midi1(address, voice) if self.members.is_some() => {
                (address, Voice2::from(voice))
            }
            Message::Midi1(..) => {
                midi1(message).for_each(&mut out);
                return;
            }
            other => {
                midi1(other).for_each(&mut out);
                return;
            }
        };
        let extremes = self.mode.extremes;
        if let (PerNoteTranslation::Mpe(zone), Some(members)) =
            (self.mode.per_note, self.members.as_mut())
            && members.route(&zone, extremes, address, voice, &mut out)
        {
            return;
        }
        // The zone is the destination of every other message (M2-120 sections
        // 4.1 to 4.6: zone-wide messages, Program Change and Bank Select are
        // the Manager Channel's), so the source channel names no MIDI 1.0
        // channel.
        let address = match self.mode.per_note {
            PerNoteTranslation::Mpe(zone) => Address {
                channel: zone.master_channel(),
                ..address
            },
            PerNoteTranslation::Dropped => address,
        };
        let plain = |out: &mut dyn FnMut(Midi1Bytes)| {
            if let Ok(voice) = Voice1::from_voice2(voice, extremes) {
                out(voice.bytes(address.channel));
            }
        };
        let mut cc = |controller: CCNumber, value: U7| {
            out(Voice1::ControlChange { controller, value }.bytes(address.channel));
        };
        match voice {
            Voice2::Registered { bank, index, value } => {
                let data = if scale::zero_extended(index) {
                    rounded_u14(value)
                } else {
                    data_u14(extremes, value)
                };
                run(
                    (CCNumber::RPN_MSB, bank),
                    (CCNumber::RPN_LSB, index),
                    data,
                    &mut cc,
                );
            }
            Voice2::Assignable { bank, index, value } => {
                run(
                    (CCNumber::NRPN_MSB, bank),
                    (CCNumber::NRPN_LSB, index),
                    data_u14(extremes, value),
                    &mut cc,
                );
            }
            Voice2::ProgramChange {
                program,
                bank: Some(bank),
            } => {
                cc(CCNumber::BANK_SELECT, bank.msb);
                cc(CCNumber::BANK_SELECT_LSB, bank.lsb);
                out(Voice1::ProgramChange(program).bytes(address.channel));
            }
            // Omni Off, Omni On, Mono Mode On and Poly Mode On are sent on no
            // MPE Channel (M2-120 section 4.7).
            Voice2::ControlChange { controller, .. }
                if matches!(self.mode.per_note, PerNoteTranslation::Mpe(_))
                    && (124..=127).contains(&controller.get()) => {}
            _ => plain(&mut out),
        }
    }
}

/// One controller as D.2.3's four Control Changes: the two selects, then the
/// data's MSB and LSB.
fn run(
    (msb_select, msb): (CCNumber, U7),
    (lsb_select, lsb): (CCNumber, U7),
    data: U14,
    cc: &mut impl FnMut(CCNumber, U7),
) {
    cc(msb_select, msb);
    cc(lsb_select, lsb);
    cc(CCNumber::DATA_ENTRY, data.msb());
    cc(CCNumber::DATA_ENTRY_LSB, data.lsb());
}

/// The top 14 bits of a 32-bit controller word: the data of a Registered or
/// Assignable Controller that M2-115 scales Min-Center-Max, downscaled by a right
/// shift (as [`BendValue::to_u14`](tutti_ump::BendValue::to_u14) does a bend).
fn data_u14(extremes: ExtremeValues, value: ControlValue) -> U14 {
    extremes.fourteen(BendValue::new(value.get()).to_u14(), value.get())
}

/// A 32-bit value as 14 bits, rounded (M2-115 section 4.4.1,
/// `scaleDownRounding(value, 32, 14)`): fixed-point values are rounded when
/// downscaled, by adding half the scaled range, shifting, and clamping to the
/// 14-bit maximum.
fn rounded_u14(value: ControlValue) -> U14 {
    const SCALE_BITS: u32 = 32 - 14;
    let shifted = (u64::from(value.get()) + (1 << (SCALE_BITS - 1))) >> SCALE_BITS;
    // `min` keeps 14 bits.
    U14::masked(shifted.min(0x3FFF) as u16)
}
