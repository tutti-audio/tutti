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
//! send, in order, and says when one is owed by its [`PerNotePolicy`]. It is pure: it holds what it has
//! seen per Group, Channel and Note Number, allocates nothing after
//! construction, and sends nothing.

use tutti_ump::{Address, MidiChannel, MidiGroup, NoteNumber, PerNoteManagement, Voice2};

/// Every channel of every Group.
const SLOTS: usize = MidiGroup::COUNT as usize * MidiChannel::COUNT as usize;

/// When a sender sends Per-Note Management, and which (M2-104 Appendix C).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PerNotePolicy {
    /// Appendix C.2: only where a key's controllers are still attached. A key
    /// is attached once it is struck, and stays so until a management message
    /// detaches it. Management is owed when a new note begins on an attached
    /// key whose controllers were touched since its strike: at a Note On, or at
    /// the first Per-Note Controller after the Note Off, which sets up the
    /// next note. A key nothing was sent to owes none.
    WhenAttached(PerNoteManagement),
    /// Appendix C.3, note number rotation: before every note, the first one
    /// included, whatever was sent to the key. Management is owed at the start
    /// of each note's setup: its first Per-Note Controller while the key does
    /// not sound, or its Note On when no setup controller came first.
    EveryNote(PerNoteManagement),
}

impl PerNotePolicy {
    /// The management the policy sends.
    #[must_use]
    pub const fn management(self) -> PerNoteManagement {
        match self {
            Self::WhenAttached(management) | Self::EveryNote(management) => management,
        }
    }
}

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
    /// Managed since the last strike: the next note's setup has begun.
    managed: u128,
}

impl Keys {
    /// The controllers of the number are detached: nothing is attached.
    fn detach(&mut self, bit: u128) {
        self.struck &= !bit;
        self.touched &= !bit;
    }
}

/// Decides, message by message, when a Per-Note Management message is owed,
/// by its [`PerNotePolicy`].
///
/// A controller sent while a note sounds belongs to it and owes nothing. A
/// management message the sender sends itself counts as the management owed.
#[derive(Clone, Debug)]
pub struct PerNoteManager {
    policy: PerNotePolicy,
    channels: Box<[Keys; SLOTS]>,
}

impl PerNoteManager {
    /// A manager that follows `policy`.
    #[must_use]
    pub fn new(policy: PerNotePolicy) -> Self {
        Self {
            policy,
            channels: Box::new([Keys::default(); SLOTS]),
        }
    }

    /// The policy it follows.
    #[must_use]
    pub const fn policy(&self) -> PerNotePolicy {
        self.policy
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
        let management = self.policy.management();
        let every = matches!(self.policy, PerNotePolicy::EveryNote(_));
        match *message {
            Voice2::NoteOn { note, .. } => {
                let bit = bit(note);
                let owed = if every {
                    keys.managed & bit == 0
                } else {
                    keys.struck & keys.touched & bit != 0
                };
                keys.detach(bit);
                keys.struck |= bit;
                keys.sounding |= bit;
                keys.managed &= !bit;
                owed.then(|| management.voice(note))
            }
            Voice2::NoteOff { note, .. } => {
                keys.sounding &= !bit(note);
                None
            }
            Voice2::PerNoteManagement { note, .. } => {
                let bit = bit(note);
                keys.detach(bit);
                keys.managed |= bit;
                None
            }
            Voice2::RegisteredPerNote { note, .. }
            | Voice2::AssignablePerNote { note, .. }
            | Voice2::PolyPressure { note, .. }
            | Voice2::PerNotePitchBend { note, .. } => {
                let bit = bit(note);
                if keys.sounding & bit != 0 {
                    keys.touched |= bit;
                    return None;
                }
                if every {
                    let owed = keys.managed & bit == 0;
                    keys.managed |= bit;
                    return owed.then(|| management.voice(note));
                }
                if keys.struck & bit == 0 {
                    return None;
                }
                let owed = keys.touched & bit != 0;
                keys.detach(bit);
                owed.then(|| management.voice(note))
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
