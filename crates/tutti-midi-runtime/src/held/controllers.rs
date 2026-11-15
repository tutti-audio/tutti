//! The controllers a source leaves off their normal position: the hold pedals
//! and the pitch bend, per (protocol, Group, Channel). A clip's ledger writes
//! the returns to its port ([`lift_all`](Controllers::lift_all)); a
//! [`HeldState`](super::HeldState) hands them to a caller
//! ([`lift_while`](Controllers::lift_while)).
//!
//! MIDI 1.0 Detailed Specification 4.2.1, p. 31 (Stop): besides note events, controllers
//! that are off their initial position (the pitch wheels and the sustain
//! pedal among them) go back to their normal positions. A pedal left down also defeats the note-offs
//! the ledger has just sent: a Hold or Sustain pedal that is on holds a note
//! through a Note Off or All Notes Off until it is lifted (Appendix A-5). So the pedals are lifted
//! after the notes are ended, and only then does the receiver let go of them.
//!
//! The table is fixed: one `u16` per lane, 512 lanes (MIDI 1.0 and MIDI 2.0
//! channel voice, 16 Groups, 16 Channels), allocated in [`Controllers::new`].
//! Per-note pitch bend (MIDI 2.0) is not tracked: M2-104 resets a per-note
//! controller when its note ends, and the ledger ends every note first.

use tutti_graph::{Event, EventWriter, Offset};
use tutti_ump::{
    BendValue, CCNumber, ControlValue, Lane, Message, MidiGroup, Protocol, U7, U14, Ump, Voice1,
    Voice2,
};

/// The hold controllers: sustain, sostenuto, soft pedal, hold 2.
const PEDALS: [u8; 4] = [64, 66, 67, 69];
/// The bit of the pitch bend in a lane's state; the pedals are bits 0 to 3.
const BEND: u8 = 4;

/// Which controllers are off their normal position, low byte, and which of
/// those a refused reset still owes, high byte (the same bits).
pub(crate) struct Controllers {
    lanes: Box<[u16; Lane::COUNT]>,
    /// Some lane owes a reset: a block with no debt pays in one test.
    owing: bool,
}

impl Controllers {
    /// Control thread: allocates.
    pub(crate) fn new() -> Self {
        Self {
            lanes: Box::new([0; Lane::COUNT]),
            owing: false,
        }
    }

    pub(crate) fn forget(&mut self) {
        self.lanes.fill(0);
        self.owing = false;
    }

    /// Folds in a message the port took. A controller back at its normal
    /// position is forgotten, and a new value supersedes a reset still owed.
    pub(crate) fn observe(&mut self, ump: Ump) {
        let (protocol, address, bit, off_normal) = match ump.decode() {
            Ok(Message::Midi1(a, Voice1::ControlChange { controller, value })) => {
                let Some(bit) = pedal_bit(controller) else {
                    return;
                };
                (Protocol::Midi1, a, bit, value.get() >= 64)
            }
            Ok(Message::Midi1(a, Voice1::PitchBend(b))) => {
                (Protocol::Midi1, a, BEND, b != U14::CENTER)
            }
            Ok(Message::Midi2(a, Voice2::ControlChange { controller, value })) => {
                let Some(bit) = pedal_bit(controller) else {
                    return;
                };
                (Protocol::Midi2, a, bit, value >= ControlValue::CENTER)
            }
            Ok(Message::Midi2(a, Voice2::PitchBend(v))) => {
                (Protocol::Midi2, a, BEND, v != BendValue::CENTER)
            }
            _ => return,
        };
        let Some(lane) = Lane::new(protocol, address) else {
            return;
        };
        let lane = &mut self.lanes[lane.index()];
        *lane &= !(0x101 << bit);
        if off_normal {
            *lane |= 1 << bit;
        }
    }

    /// The message that returns control `bit` of `lane` to its normal position.
    fn reset(lane: Lane, bit: u8) -> Ump {
        let address = lane.address();
        let message = if lane.protocol() == Protocol::Midi2 {
            Message::Midi2(
                address,
                match PEDALS.get(usize::from(bit)) {
                    Some(&cc) => Voice2::ControlChange {
                        controller: CCNumber::new(cc).expect("a controller"),
                        value: ControlValue::MIN,
                    },
                    None => Voice2::PitchBend(BendValue::CENTER),
                },
            )
        } else {
            Message::Midi1(
                address,
                match PEDALS.get(usize::from(bit)) {
                    Some(&cc) => Voice1::ControlChange {
                        controller: CCNumber::new(cc).expect("a controller"),
                        value: U7::of(0),
                    },
                    None => Voice1::PitchBend(U14::CENTER),
                },
            )
        };
        Ump::from(message)
    }

    /// Returns to normal every controller that is off, at `at`, in lane and
    /// bit order, after the notes. A message the port refuses is owed and goes
    /// out first at a later block ([`pay`](Self::pay)); once the port refuses
    /// one the rest are not tried (it is full) but each is owed and counted.
    pub(crate) fn lift_all(&mut self, out: &mut EventWriter<'_>, at: Offset) {
        let mut full = false;
        for (lane, state) in Lane::all().zip(self.lanes.iter_mut()) {
            for bit in 0..=BEND {
                let (held, owed) = (*state & (1 << bit) != 0, *state & (0x100 << bit) != 0);
                if !held {
                    continue;
                }
                let attempted = !full;
                if attempted && out.push(Event::midi(at, Self::reset(lane, bit))).is_ok() {
                    if owed {
                        out.recovered();
                    }
                    *state &= !(0x101 << bit);
                    continue;
                }
                match (attempted, owed) {
                    // The refused push counted itself.
                    (true, true) => out.recovered(),
                    (false, false) => out.withhold(),
                    _ => {}
                }
                full = true;
                *state |= 0x100 << bit;
                self.owing = true;
            }
        }
    }

    /// Hands `accept` the message that returns each controller that is off
    /// (all of `group`'s, or every Group's for `None`) to its normal position,
    /// in lane and bit order, and forgets the ones it takes (`true`). Stops at
    /// the first `accept` refuses (`false`): that one and the rest stay held.
    /// Returns whether every controller in scope was taken. Allocates nothing.
    pub(crate) fn lift_while(
        &mut self,
        group: Option<MidiGroup>,
        mut accept: impl FnMut(Ump) -> bool,
    ) -> bool {
        for (lane, state) in Lane::all().zip(self.lanes.iter_mut()) {
            if group.is_some_and(|g| lane.address().group != g) {
                continue;
            }
            for bit in 0..=BEND {
                if *state & (1 << bit) == 0 {
                    continue;
                }
                if !accept(Self::reset(lane, bit)) {
                    return false;
                }
                *state &= !(0x101 << bit);
            }
        }
        true
    }

    /// Whether any controller is off its normal position.
    pub(crate) fn any(&self) -> bool {
        self.lanes.iter().any(|&l| l != 0)
    }

    /// Writes the resets the port refused earlier, stopping at the first it
    /// refuses again. Each attempt gives its count back, so a delayed reset
    /// nets zero.
    pub(crate) fn pay(&mut self, out: &mut EventWriter<'_>, at: Offset) {
        if !self.owing {
            return;
        }
        let mut full = false;
        let mut left = false;
        for (lane, state) in Lane::all().zip(self.lanes.iter_mut()) {
            for bit in 0..=BEND {
                if *state & (0x100 << bit) == 0 {
                    continue;
                }
                if !full {
                    let taken = out.push(Event::midi(at, Self::reset(lane, bit))).is_ok();
                    out.recovered();
                    full = !taken;
                    if taken {
                        *state &= !(0x101 << bit);
                        continue;
                    }
                }
                left = true;
            }
        }
        self.owing = left;
    }
}

fn pedal_bit(controller: CCNumber) -> Option<u8> {
    PEDALS
        .iter()
        .position(|&p| p == controller.get())
        .map(|i| i as u8)
}
