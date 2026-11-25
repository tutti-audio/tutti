//! The MIDI 2.0 to MIDI 1.0 translation of M2-104 Appendix D.2: the
//! [`Midi1Translator`] an output edge runs.

use crate::midi1;
use tutti_ump::{CCNumber, ExtremeValues, Message, Midi1Bytes, U7, U14, Ump, Voice1, Voice2};

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
///   0 to 31), and downscales **with rounding** (section 4.4, `ExtremeValues::registered`).
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
/// [`new`](Self::new) is the Default Translation. [`extremes`](Self::extremes)
/// keeps near-extreme values off the minimum and maximum (D.1.5). The per-note
/// messages of a device with the MPE Profile enabled are `tutti_voicing::MpeSender`'s,
/// ahead of this (D.4.1).
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
#[derive(Clone, Copy, Debug)]
pub struct Midi1Translator {
    extremes: ExtremeValues,
}

impl Default for Midi1Translator {
    fn default() -> Self {
        Self::new()
    }
}

impl Midi1Translator {
    /// The most MIDI 1.0 messages one [`feed`](Self::feed) sends: a controller's
    /// sequence (D.2.3).
    pub const MAX_MESSAGES: usize = 4;

    /// The most bytes one [`feed`](Self::feed) sends: four 3-byte messages.
    pub const MAX_BYTES: usize = 12;

    /// A translator that translates by the Default Translation.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            extremes: ExtremeValues::Scaled,
        }
    }

    /// The translator with `extremes` as how the smallest and largest values
    /// are translated: [`ExtremeValues::Avoided`] is an Alternate Translation
    /// Mode (M2-104 D.1.5, D.4), and a product tells its user it is active.
    #[must_use]
    pub const fn extremes(mut self, extremes: ExtremeValues) -> Self {
        self.extremes = extremes;
        self
    }

    /// Calls `out` with each MIDI 1.0 message `message` stands for, in the
    /// order to send them: four for a controller, three for a Program Change
    /// with a bank, one for most others, none for one with no MIDI 1.0 form
    /// (D.2.8) or that is not a channel or system message. A MIDI 1.0 message
    /// is itself. [`MAX_MESSAGES`](Self::MAX_MESSAGES) and
    /// [`MAX_BYTES`](Self::MAX_BYTES) bound one call.
    pub fn feed(&self, packet: impl Into<Ump>, mut out: impl FnMut(Midi1Bytes)) {
        // A packet that does not decode has no MIDI 1.0 form.
        let Ok(message) = packet.into().decode() else {
            return;
        };
        let Message::Midi2(address, voice) = message else {
            midi1(message).for_each(&mut out);
            return;
        };
        let extremes = self.extremes;
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
                let data = extremes.registered(index, value);
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
                    extremes.assignable(value),
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
