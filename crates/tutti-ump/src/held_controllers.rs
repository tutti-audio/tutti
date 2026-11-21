//! The controllers a source leaves off their normal position, per [`GroupChannelIndex`].

use crate::{
    BendValue, CCNumber, ControlValue, GroupChannelIndex, Message, MidiGroup, Protocol, U7, U14,
    Ump, Voice1, Voice2,
};

/// The hold controllers, in bit order: sustain, sostenuto, soft pedal, hold 2.
const PEDALS: [CCNumber; 4] = [
    CCNumber::of(64),
    CCNumber::of(66),
    CCNumber::of(67),
    CCNumber::of(69),
];
/// The bit of the pitch bend in a lane's state; the pedals are bits 0 to 3.
const BEND: u8 = 4;
/// The bits of a lane's state that are off their normal position.
const HELD: u16 = 0x1f;

/// Which controllers are off their normal position, per [`GroupChannelIndex`]: the four
/// hold pedals and the pitch bend. 1 KiB, one `u16` per lane, and no
/// allocation; a caller that wants it off the stack boxes it.
///
/// A message is folded in with [`observe`](Self::observe): a pedal at or past
/// the middle of its range (MIDI 1.0 Detailed Specification, Control Change
/// messages: 0 to 63 off, 64 to 127 on; MIDI 2.0 the upper half of the 32-bit
/// range) or a bend off centre is held, and back at its normal position it is
/// let go. The message that returns each one to normal is what
/// [`closes`](Self::closes), [`release_while`](Self::release_while) and the other
/// releases hand out. A per-note controller (MIDI 2.0) ends with its note
/// (M2-104 section 7.4), so none is tracked.
///
/// A release a caller was refused can be owed: the controller stays held, marked,
/// until [`pay`](Self::pay) takes it; a new value for it forgets the mark.
///
/// ```
/// use tutti_ump::{CCNumber, HeldControllers, Message, MidiChannel, U7, Ump, Voice1};
///
/// let ch = MidiChannel::of(2);
/// let mut held = HeldControllers::new();
/// held.observe(Ump::from(Message::midi1(
///     ch,
///     Voice1::control_change(CCNumber::SUSTAIN, U7::of(127)),
/// )));
/// assert_eq!(held.closes().count(), 1);
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct HeldControllers {
    /// The held bits in the low byte, and the ones whose release is owed in the
    /// high byte (the same bits).
    lanes: [u16; GroupChannelIndex::COUNT],
    /// How many controllers are held.
    len: u32,
    /// Some controller is owed: a block with no debt pays in one test.
    owing: bool,
}

impl Default for HeldControllers {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for HeldControllers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HeldControllers")
            .field("held", &self.len)
            .field("owing", &self.owing)
            .finish_non_exhaustive()
    }
}

impl HeldControllers {
    /// Nothing held.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            lanes: [0; GroupChannelIndex::COUNT],
            len: 0,
            owing: false,
        }
    }

    /// How many controllers are held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether no controller is held.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether some release is owed ([`release_all`](Self::release_all) was refused).
    #[must_use]
    pub const fn owing(&self) -> bool {
        self.owing
    }

    /// Forgets everything held.
    pub fn clear(&mut self) {
        self.lanes.fill(0);
        self.len = 0;
        self.owing = false;
    }

    /// Adds every controller `other` holds, and what it owes.
    pub fn union_with(&mut self, other: &Self) {
        for (a, b) in self.lanes.iter_mut().zip(&other.lanes) {
            *a |= b;
        }
        self.len = self.lanes.iter().map(|l| (l & HELD).count_ones()).sum();
        self.owing |= other.owing;
    }

    /// Folds in a message the source sent: a pedal or bend off its normal
    /// position is held, back at it is forgotten, and a new value supersedes a
    /// release still owed. Anything else changes nothing.
    #[inline]
    pub fn observe(&mut self, ump: Ump) {
        // Channel voice, MIDI 1.0 or 2.0, status Control Change or Pitch Bend.
        let w = ump.0[0];
        if !matches!(w >> 28, 0x2 | 0x4) || !matches!((w >> 20) & 0xf, 0xb | 0xe) {
            return;
        }
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
        let Some(lane) = GroupChannelIndex::new(protocol, address) else {
            return;
        };
        let state = &mut self.lanes[lane.index()];
        let was = *state >> bit & 1;
        *state &= !(0x101 << bit);
        *state |= u16::from(off_normal) << bit;
        self.len = self.len + u32::from(off_normal) - u32::from(was);
    }

    /// The message that returns control `bit` of `lane` to its normal position.
    fn reset(lane: GroupChannelIndex, bit: u8) -> Ump {
        let address = lane.address();
        let pedal = PEDALS.get(usize::from(bit)).copied();
        // Each pedal returns to 0 (MIDI 1.0 Detailed Specification, Control
        // Change messages: CC 64 Damper Pedal (Sustain), CC 66 Sostenuto, CC 67
        // Soft Pedal, CC 69 Hold 2, each off at 0 to 63); the bend to its centre
        // (MIDI 1.0 Pitch Bend, 0x2000; MIDI 2.0, M2-104 section 7.4.7,
        // 0x80000000).
        let message = if lane.protocol() == Protocol::Midi2 {
            Message::Midi2(
                address,
                match pedal {
                    Some(controller) => Voice2::ControlChange {
                        controller,
                        value: ControlValue::MIN,
                    },
                    None => Voice2::PitchBend(BendValue::CENTER),
                },
            )
        } else {
            Message::Midi1(
                address,
                match pedal {
                    Some(controller) => Voice1::ControlChange {
                        controller,
                        value: U7::of(0),
                    },
                    None => Voice1::PitchBend(U14::CENTER),
                },
            )
        };
        Ump::from(message)
    }

    /// The held controllers' (lane, bit), in lane and bit order.
    fn held(&self) -> impl Iterator<Item = (GroupChannelIndex, u8)> + '_ {
        GroupChannelIndex::all()
            .zip(&self.lanes)
            .filter(|(_, state)| **state & HELD != 0)
            .flat_map(|(lane, &state)| {
                (0..=BEND)
                    .filter(move |bit| state >> bit & 1 != 0)
                    .map(move |bit| (lane, bit))
            })
    }

    /// The messages that return each held controller to its normal position,
    /// in lane and bit order.
    pub fn closes(&self) -> impl Iterator<Item = Ump> + '_ {
        self.held().map(|(lane, bit)| Self::reset(lane, bit))
    }

    /// The message that returns the first held controller to normal, forgotten
    /// as it is taken; `None` once nothing is held.
    pub fn pop(&mut self) -> Option<Ump> {
        let (lane, bit) = self.held().next()?;
        self.forget(lane, bit);
        Some(Self::reset(lane, bit))
    }

    fn forget(&mut self, lane: GroupChannelIndex, bit: u8) {
        let state = &mut self.lanes[lane.index()];
        self.len -= u32::from(*state >> bit & 1);
        *state &= !(0x101 << bit);
    }

    /// Hands `accept` the message that returns each controller that is held
    /// (all of `group`'s, or every Group's for `None`) to its normal position,
    /// in lane and bit order, and forgets the ones it takes (`true`). Stops at
    /// the first `accept` refuses (`false`): that one and the rest stay held.
    /// Returns whether every controller in scope was taken. Allocates nothing.
    pub fn release_while(
        &mut self,
        group: Option<MidiGroup>,
        mut accept: impl FnMut(Ump) -> bool,
    ) -> bool {
        if self.len == 0 {
            return true;
        }
        for lane in GroupChannelIndex::all() {
            if group.is_some_and(|g| lane.address().group != g) {
                continue;
            }
            for bit in 0..=BEND {
                if self.lanes[lane.index()] >> bit & 1 == 0 {
                    continue;
                }
                if !accept(Self::reset(lane, bit)) {
                    return false;
                }
                self.forget(lane, bit);
            }
        }
        true
    }

    /// Hands `send` the message that returns each held controller to its normal
    /// position, in lane and bit order, with whether its release was already owed:
    /// `true` forgets it, `false` leaves it held and owed to
    /// [`pay`](Self::pay). `send` is called for every held controller, so it
    /// can count the ones it did not try. Allocates nothing.
    pub fn release_all(&mut self, mut send: impl FnMut(Ump, bool) -> bool) {
        if self.len == 0 {
            return;
        }
        for lane in GroupChannelIndex::all() {
            for bit in 0..=BEND {
                let state = self.lanes[lane.index()];
                if state >> bit & 1 == 0 {
                    continue;
                }
                if send(Self::reset(lane, bit), state >> (8 + bit) & 1 != 0) {
                    self.forget(lane, bit);
                } else {
                    self.lanes[lane.index()] |= 0x100 << bit;
                    self.owing = true;
                }
            }
        }
    }

    /// Hands `send` the message of each owed release, in lane and bit order, and
    /// forgets the ones it takes (`true`). Stops at the first it refuses
    /// (`false`): that one and the rest stay owed. Allocates nothing.
    pub fn pay(&mut self, mut send: impl FnMut(Ump) -> bool) {
        if !self.owing {
            return;
        }
        let mut left = false;
        for lane in GroupChannelIndex::all() {
            for bit in 0..=BEND {
                if self.lanes[lane.index()] >> (8 + bit) & 1 == 0 {
                    continue;
                }
                if left || !send(Self::reset(lane, bit)) {
                    left = true;
                } else {
                    self.forget(lane, bit);
                }
            }
        }
        self.owing = left;
    }
}

fn pedal_bit(controller: CCNumber) -> Option<u8> {
    PEDALS
        .iter()
        .position(|&p| p == controller)
        .map(|i| i as u8)
}
