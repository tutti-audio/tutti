//! The Per-Note Management a sender owes before a key is struck again
//! (M2-104 §7.4.5, Appendix C.2 and C.3).
//!
//! Per-Note Controllers are shared by every note of one Note Number. A note
//! that is struck while the controllers of an earlier note on that number are
//! still attached would take their values, and the earlier note would answer
//! the new note's controllers. A Per-Note Management message sent first detaches
//! the earlier notes and resets the controllers to their defaults, so the new
//! note starts clean.
//!
//! [`PerNoteManager`] reads the channel voice messages a sender is about to
//! send, in order, and says when one is owed. It is pure: it holds what it has
//! seen per Group, Channel and Note Number, allocates nothing after
//! construction, and sends nothing.

use tutti_ump::{Address, MidiChannel, MidiGroup, NoteNumber, PerNoteManagement, Voice2};

/// Every channel of every Group.
const SLOTS: usize = MidiGroup::COUNT as usize * MidiChannel::COUNT as usize;

/// The keys of one channel, a bit per Note Number.
#[derive(Clone, Copy, Debug, Default)]
struct Keys {
    /// Struck, and not yet detached: a note on the number may still answer the
    /// controllers.
    struck: u128,
    /// A Note On with no Note Off since.
    sounding: u128,
    /// A Per-Note Controller was sent since the strike.
    touched: u128,
}

impl Keys {
    /// The controllers of the number are detached: nothing is attached.
    fn detach(&mut self, bit: u128) {
        self.struck &= !bit;
        self.touched &= !bit;
    }
}

/// Decides, message by message, when a Per-Note Management message is owed.
///
/// A key is *attached* once it is struck and until a management message
/// detaches it. A message is owed when something begins a new note on an
/// attached key whose controllers were touched since its strike:
///
/// - a Note On, whether or not the earlier note has ended; or
/// - the first Per-Note Controller after the Note Off, which sets up the next
///   note (the order of M2-104 Appendix C.2, Figure 91: management, then the
///   controllers, then the Note On).
///
/// A controller sent while the note sounds belongs to it. The management
/// sent is the choice the sender made at construction. A management message
/// the sender sends itself detaches the key, and nothing more is owed for it.
#[derive(Clone, Debug)]
pub struct PerNoteManager {
    choice: PerNoteManagement,
    channels: Box<[Keys; SLOTS]>,
}

impl PerNoteManager {
    /// A manager that sends `choice` whenever one is owed: Detach for the
    /// sender that lets the new note start from the values it sets up
    /// (Appendix C.2), Detach and Reset for one that rotates note numbers
    /// (Appendix C.3).
    #[must_use]
    pub fn new(choice: PerNoteManagement) -> Self {
        Self {
            choice,
            channels: Box::new([Keys::default(); SLOTS]),
        }
    }

    /// The management it sends.
    #[must_use]
    pub const fn choice(&self) -> PerNoteManagement {
        self.choice
    }

    /// Forgets every key: nothing is attached.
    pub fn clear(&mut self) {
        self.channels.fill(Keys::default());
    }

    /// Takes the next message the sender is about to send on `address`, and
    /// returns the Per-Note Management message to send before it, if one is
    /// owed. A message that is not a Per-Note message owes none.
    pub fn before(&mut self, address: Address, message: &Voice2) -> Option<Voice2> {
        let keys = &mut self.channels[address.index()];
        match *message {
            Voice2::NoteOn { note, .. } => {
                let bit = bit(note);
                let owed = (keys.struck & keys.touched & bit != 0).then_some(self.choice);
                keys.detach(bit);
                keys.struck |= bit;
                keys.sounding |= bit;
                owed.map(|m| m.voice(note))
            }
            Voice2::NoteOff { note, .. } => {
                keys.sounding &= !bit(note);
                None
            }
            Voice2::PerNoteManagement { note, .. } => {
                keys.detach(bit(note));
                None
            }
            Voice2::RegisteredPerNote { note, .. }
            | Voice2::AssignablePerNote { note, .. }
            | Voice2::PolyPressure { note, .. }
            | Voice2::PerNotePitchBend { note, .. } => {
                let bit = bit(note);
                if keys.struck & bit == 0 {
                    return None;
                }
                if keys.sounding & bit != 0 {
                    keys.touched |= bit;
                    return None;
                }
                let owed = (keys.touched & bit != 0).then_some(self.choice);
                keys.detach(bit);
                owed.map(|m| m.voice(note))
            }
            _ => None,
        }
    }
}

const fn bit(note: NoteNumber) -> u128 {
    1 << note.get()
}

#[cfg(test)]
mod tests;
