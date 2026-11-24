// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_ump::{Bank, CCNumber, ControlValue, U7, U14, Voice2};
use tutti_units::Amplitude;

/// The amount of the default modulators CC 91 to reverb send and CC 93 to
/// chorus send (SF2 2.04 sections 8.4.8 and 8.4.9): 200 tenths of a percent,
/// which is 20% of full level at the controller's top.
const SEND_AMOUNT: f64 = 0.2;

/// A 14-bit controller with its most significant 7 bits replaced (CC 0 to 31).
/// The least significant 7 bits go to zero (M1 p.12).
fn with_msb(msb: U7) -> U14 {
    U14::masked(u16::from(msb.get()) << 7)
}

/// A 14-bit controller with its least significant 7 bits replaced (CC 32 to 63).
fn with_lsb(current: U14, lsb: U7) -> U14 {
    U14::masked((current.get() & 0x3F80) | u16::from(lsb.get()))
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Channel {
    /// Whether this is the percussion channel: a Program Change on it selects
    /// from the percussion bank, whatever Bank Select holds.
    percussion: bool,

    /// The bank and the program the channel plays.
    bank: tutti_sf2::Bank,
    program: U7,
    /// The Bank Select (CC 0 and 32) received and not yet applied (M1 p.13). The
    /// next Program Change applies it.
    pending_bank: U14,

    reverb_send: u8,
    chorus_send: u8,
}

impl Channel {
    pub(crate) fn new(percussion: bool) -> Self {
        let mut channel = Self {
            percussion,
            bank: tutti_sf2::Bank::Melodic(U7::of(0)),
            program: U7::of(0),
            pending_bank: U14::of(0),
            reverb_send: 0,
            chorus_send: 0,
        };

        channel.reset();

        channel
    }

    pub(crate) fn reset(&mut self) {
        self.bank = self.bank_of(U7::of(0));
        self.pending_bank = U14::of(0);
        self.program = U7::of(0);

        self.reverb_send = 40;
        self.chorus_send = 0;
    }

    /// Bank Select, coarse (CC 0): held for the next Program Change, and the
    /// pending LSB goes to zero (M1 p.12).
    pub(crate) fn select_bank_coarse(&mut self, value: U7) {
        self.pending_bank = with_msb(value);
    }

    /// Bank Select, fine (CC 32): held for the next Program Change.
    pub(crate) fn select_bank_fine(&mut self, value: U7) {
        self.pending_bank = with_lsb(self.pending_bank, value);
    }

    /// The bank a Bank Select of MSB `msb` names on this channel (a
    /// SoundFont's bank is one number, which MIDI sends as the MSB): the
    /// percussion bank on the percussion channel, whatever the MSB.
    fn bank_of(&self, msb: U7) -> tutti_sf2::Bank {
        if self.percussion {
            tutti_sf2::Bank::Percussion
        } else {
            tutti_sf2::Bank::Melodic(msb)
        }
    }

    /// The MSB a Bank Select sends for `bank` on this channel, `None` for a
    /// bank MIDI cannot name here: the percussion bank on a melodic channel,
    /// a melodic bank on the percussion channel.
    fn msb_of(&self, bank: tutti_sf2::Bank) -> Option<U7> {
        match (bank, self.percussion) {
            (tutti_sf2::Bank::Melodic(msb), false) => Some(msb),
            (tutti_sf2::Bank::Percussion, true) => Some(U7::of(0)),
            _ => None,
        }
    }

    /// A Program Change: the held bank takes effect (M1 p.13) and the program
    /// is selected.
    pub(crate) fn program_change(&mut self, program: U7) {
        let msb = U7::masked((self.pending_bank.get() >> 7) as u8);
        self.bank = self.bank_of(msb);
        self.program = program;
    }

    /// Selects a preset's bank and program at once, as they are: the channel
    /// plays that preset, whichever channel it is. A melodic bank is also held
    /// as Bank Select, as the message that names it would leave it.
    pub(crate) fn select(&mut self, bank: tutti_sf2::Bank, program: U7) {
        if let tutti_sf2::Bank::Melodic(msb) = bank {
            self.pending_bank = with_msb(msb);
        }
        self.bank = bank;
        self.program = program;
    }

    /// The most messages [`chase`](Self::chase) writes: a Program Change with
    /// its bank, the Bank Select held for the next one (two controllers) and
    /// the two sends.
    pub(crate) const CHASE_MESSAGES: usize = 5;

    /// Writes what this channel holds that a fresh one does not, as the
    /// messages that set it (the unit's chase, `Node::chase`): the program
    /// and the bank it plays, as one Program Change with the bank riding
    /// along; the Bank Select received and not yet applied, which waits for
    /// the next Program Change (M1 p.13); and the two effect sends.
    pub(crate) fn chase(&self, emit: &mut impl FnMut(Voice2)) {
        let fresh = Self::new(self.percussion);
        let cc = |controller: CCNumber, value: u8| Voice2::ControlChange {
            controller,
            value: ControlValue::from(U7::masked(value)),
        };
        // What the receiver holds as Bank Select once the messages so far
        // are read: nothing, then the bank's MSB if a Program Change goes.
        let mut held = fresh.pending_bank;
        if (self.bank, self.program) != (fresh.bank, fresh.program) {
            // A bank a Program Change cannot name on this channel is not
            // chased: the receiver stays on its own.
            if let Some(msb) = self.msb_of(self.bank) {
                emit(Voice2::ProgramChange {
                    program: self.program,
                    bank: Some(Bank {
                        msb,
                        lsb: U7::of(0),
                    }),
                });
                held = with_msb(msb);
            }
        }
        if self.pending_bank != held {
            emit(cc(
                CCNumber::BANK_SELECT,
                (self.pending_bank.get() >> 7) as u8,
            ));
            let lsb = (self.pending_bank.get() & 0x7F) as u8;
            if lsb != 0
                && let Some(controller) = CCNumber::BANK_SELECT.lsb()
            {
                emit(cc(controller, lsb));
            }
        }
        if self.reverb_send != fresh.reverb_send {
            emit(cc(CCNumber::REVERB_SEND, self.reverb_send));
        }
        if self.chorus_send != fresh.chorus_send {
            emit(cc(CCNumber::CHORUS_SEND, self.chorus_send));
        }
    }

    pub(crate) fn set_reverb_send(&mut self, value: i32) {
        self.reverb_send = value as u8;
    }

    pub(crate) fn set_chorus_send(&mut self, value: i32) {
        self.chorus_send = value as u8;
    }

    /// The bank the channel plays.
    pub(crate) fn bank(&self) -> tutti_sf2::Bank {
        self.bank
    }

    /// The program the channel plays.
    pub(crate) fn program(&self) -> U7 {
        self.program
    }

    /// What CC 91 adds to a voice's reverb send, as a fraction of full level:
    /// the default modulator of SF2 2.04 section 8.4.8 (a linear positive
    /// unipolar source, so the controller maps to `value / 128`, times an
    /// amount of 200 tenths of a percent).
    pub(crate) fn reverb_send(&self) -> Amplitude {
        Amplitude::new(SEND_AMOUNT * (f64::from(self.reverb_send) / 128.0))
    }

    /// What CC 93 adds to a voice's chorus send: the default modulator of
    /// section 8.4.9, as [`reverb_send`](Self::reverb_send).
    pub(crate) fn chorus_send(&self) -> Amplitude {
        Amplitude::new(SEND_AMOUNT * (f64::from(self.chorus_send) / 128.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_ump::ControlValue;

    /// **A chase carries the selection a Program Change can name on the
    /// channel, and no other.** A melodic channel on bank 2, program 40 and
    /// the percussion channel on kit 8 each chase one Program Change that puts
    /// a fresh channel of the same kind on the same bank and program; the
    /// percussion bank on a melodic channel and a melodic bank on the
    /// percussion channel chase none.
    #[test]
    fn a_chase_carries_what_a_program_change_can_name_on_the_channel() {
        use tutti_sf2::Bank as Sf2;
        let cases = [
            (false, Sf2::Melodic(U7::of(2)), 40, true),
            (true, Sf2::Percussion, 8, true),
            (false, Sf2::Percussion, 8, false),
            (true, Sf2::Melodic(U7::of(2)), 40, false),
        ];
        for (percussion, bank, program, carried) in cases {
            let mut channel = Channel::new(percussion);
            channel.select(bank, U7::of(program));
            let mut fresh = Channel::new(percussion);
            let mut changes = 0;
            channel.chase(&mut |m| match m {
                Voice2::ProgramChange { program, bank } => {
                    if let Some(bank) = bank {
                        fresh.select_bank_coarse(bank.msb);
                    }
                    fresh.program_change(program);
                    changes += 1;
                }
                Voice2::ControlChange { .. } => {}
                other => panic!("a chase sends no {other:?}"),
            });
            let what = format!("percussion channel {percussion}, {bank:?}");
            assert_eq!(changes, usize::from(carried), "{what}");
            if carried {
                assert_eq!(
                    (fresh.bank(), fresh.program()),
                    (bank, U7::of(program)),
                    "{what}"
                );
            }
        }
    }

    /// A chased effect send is the 7-bit value upscaled Min-Center-Max (M2-115 section 3):
    /// the centre 64 is exactly `0x8000_0000`, 127 is full scale, `0xFFFF_FFFF`, and a value
    /// below the centre shifts (1 is `1 << 25`). A plain shift would put 127 at
    /// `0xFE00_0000`. (0 is the fresh channel's value and is not chased.)
    #[test]
    fn a_chased_send_is_widened_to_the_corners_of_32_bits() {
        for (send, want) in [(64, 0x8000_0000_u32), (127, 0xFFFF_FFFF), (1, 1 << 25)] {
            let mut channel = Channel::new(false);
            channel.set_reverb_send(send);
            let mut seen = Vec::new();
            channel.chase(&mut |m| seen.push(m));
            let Some(Voice2::ControlChange { controller, value }) = seen.pop() else {
                panic!("a send is chased as a control change");
            };
            assert_eq!(controller, CCNumber::REVERB_SEND);
            assert_eq!(value, ControlValue::new(want), "send {send}");
        }
    }
}
