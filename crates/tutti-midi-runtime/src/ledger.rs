//! [`NoteLedger`]: the notes a node has started and not ended, and the only
//! way such a node puts a MIDI event on its port.
//!
//! A node that starts notes owes each one a note-off. The debt cannot be a
//! `Drop`: a port can refuse the off, and a refusal is not an error the node
//! may lose. So the debt is a row in a fixed table, and the node reaches its
//! port only through a [`Sending`], which [`NoteLedger::block`] returns after
//! it has paid what is owed.
//!
//! The ledger also tracks the hold pedals (CC 64, 66, 67, 69) and the pitch
//! bend of each Group and Channel, in the protocol they were sent in: they
//! are the other state a stopped source leaves in its receiver (MIDI 1.0
//! Detailed Spec p. 31, Stop). [`Sending::release_all`] ends the notes, then
//! returns what is off its normal position to it.
//!
//! A key is a note's Group, Channel and Note Number (M2-104 section 7.4,
//! Appendix C): the wire has no other note identity, so there is one held
//! note per key. A note-on of a key that sounds ends the sounding note first,
//! on the same frame, and takes the key. The optional owner tag `T` lets a
//! source that pairs its own on and off messages (the clip) have an off end
//! the key only when the note that owns it sent the off.
//!
//! A held key is in one of two states, `Sounding` or `Owing` (the port
//! refused the off, so it goes out at the first frame of a later block).
//! Independently, a key counts the note-ons the port refused: each such
//! note's off is skipped and counted rather than written, so a note lost
//! whole is two losses, its on and its off. An off the port refused is a loss
//! that is given back when the off goes out. The count per key saturates at
//! 255.
//!
//! The ledger holds which notes a node has started and not ended on its own
//! port, and what the port owes.
//!
//! The table holds at most its capacity in keys, held or refused. A note-on
//! that finds it full is skipped and counted ([`EventWriter::count_skipped`])
//! like one the port refused: the note does not start, so no off is owed. A
//! note already started always has its row, so a full table never strands one.

use tutti_graph::{Event, EventWriter, Offset};
use tutti_ump::{
    HeldControllers, Message, NoteKey, NoteKeyEvent, NoteVelocity, Protocol, Ump, Voice2,
};

/// The off a ledger owes `key`: a MIDI 2.0 Note Off at velocity 0 on its Group,
/// Channel and Note Number.
pub(crate) fn owed_off(key: NoteKey, at: Offset) -> Event {
    let off = Voice2::note_off(key.note(), NoteVelocity::MIN);
    Event::midi(at, Message::Midi2(key.address(), off))
}

/// A note message, read once. A MIDI 1.0 Note On at velocity 0 ends a note; a
/// MIDI 2.0 one starts it (M2-104 section 7.4.2).
pub(crate) enum Class {
    On(NoteKey),
    Off(NoteKey),
    Other,
}

impl Class {
    pub(crate) fn of(ump: Ump) -> Self {
        let (Some(address), Some(event)) = (ump.address(), ump.note_key_event()) else {
            return Self::Other;
        };
        // A note is one key whichever protocol carried it: a MIDI 1.0 On is
        // ended by a MIDI 2.0 Off of the same Group, Channel and Note Number.
        let key = |key: NoteKey| NoteKey::new(Protocol::Midi2, address, key.note());
        match event {
            NoteKeyEvent::On(k) => key(k).map_or(Self::Other, Self::On),
            NoteKeyEvent::Off(k) => key(k).map_or(Self::Other, Self::Off),
            NoteKeyEvent::Sustain { .. } => Self::Other,
        }
    }
}

/// What the port has been told about a held key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    /// The port took the note-on; no off has gone out.
    Sounding,
    /// The port refused the off; it goes out at a later block's first frame.
    Owing,
}

#[derive(Clone, Copy, Debug)]
struct Entry<T> {
    key: NoteKey,
    held: Option<Held>,
    /// How many note-ons of this key the port refused and whose offs have not
    /// come by: each such off is skipped and counted. Saturates, so more than
    /// 255 refused ons on one key with no off between them undercount.
    refused: u8,
    /// The note that owns the held key; `None` for an untagged source.
    owner: Option<T>,
}

impl<T> Entry<T> {
    fn owing(&self) -> bool {
        self.held == Some(Held::Owing)
    }
}

/// The notes a node has started and not ended. Fixed capacity: nothing here
/// allocates after [`new`](Self::new).
pub(crate) struct NoteLedger<T> {
    /// Sorted by key; never longer than `capacity`, so it never reallocates.
    entries: Vec<Entry<T>>,
    capacity: usize,
    /// How many entries are `Owing`, so a block with no debt pays in one test.
    owing: usize,
    /// The hold pedals and the pitch bend the port has been told are off
    /// their normal position.
    controllers: Box<HeldControllers>,
}

impl<T> std::fmt::Debug for NoteLedger<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoteLedger")
            .field("keys", &self.entries.len())
            .field("owing", &self.owing)
            .finish_non_exhaustive()
    }
}

impl<T: Copy + PartialEq> NoteLedger<T> {
    /// A ledger for up to `capacity` keys at once. Control thread: allocates.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
            owing: 0,
            controllers: Box::default(),
        }
    }

    /// Bytes of heap the table holds.
    #[cfg(test)]
    pub(crate) fn heap_bytes(&self) -> usize {
        self.capacity * size_of::<Entry<T>>()
    }

    /// Forgets every note: the node was reset, and whatever it fed is reset
    /// with it.
    pub fn forget(&mut self) {
        self.entries.clear();
        self.owing = 0;
        self.controllers.clear();
    }

    /// The handle a block sends through. Everything owed goes out first, at
    /// `first`, the block's first frame: a node cannot write a note before it
    /// has paid what it owes, because this is how it gets the handle.
    pub fn block<'l, 'w>(
        &'l mut self,
        out: &'l mut EventWriter<'w>,
        first: Offset,
    ) -> Sending<'l, 'w, T> {
        let mut sending = Sending { ledger: self, out };
        sending.pay(first);
        sending
    }

    /// The index of `key`'s entry, made if absent; `None` when the table is
    /// full.
    fn entry(&mut self, key: NoteKey) -> Option<usize> {
        match self.entries.binary_search_by_key(&key, |e| e.key) {
            Ok(i) => Some(i),
            Err(_) if self.entries.len() >= self.capacity => None,
            Err(at) => {
                self.entries.insert(
                    at,
                    Entry {
                        key,
                        held: None,
                        refused: 0,
                        owner: None,
                    },
                );
                Some(at)
            }
        }
    }

    fn refuse(&mut self, i: usize) {
        let e = &mut self.entries[i];
        e.refused = e.refused.saturating_add(1);
    }

    fn set_held(&mut self, i: usize, held: Option<Held>) {
        let e = &mut self.entries[i];
        let (was, now) = (e.owing(), held == Some(Held::Owing));
        e.held = held;
        self.owing = self.owing + usize::from(now) - usize::from(was);
    }

    fn recount(&mut self) {
        self.owing = self.entries.iter().filter(|e| e.owing()).count();
    }
}

/// One block's sending through a [`NoteLedger`]. It holds the port's writer,
/// so nothing else can write to the port while it lives.
pub(crate) struct Sending<'l, 'w, T> {
    ledger: &'l mut NoteLedger<T>,
    out: &'l mut EventWriter<'w>,
}

impl<T> std::fmt::Debug for Sending<'_, '_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sending")
            .field("ledger", &self.ledger)
            .finish_non_exhaustive()
    }
}

impl<T: Copy + PartialEq> Sending<'_, '_, T> {
    /// Writes the offs the port refused earlier, in key order, stopping at the
    /// first it refuses again (the port is full).
    ///
    /// An owed off is a loss the writer already counted: each attempt gives
    /// its count back, so a delayed off nets zero.
    fn pay(&mut self, at: Offset) {
        if self.ledger.owing == 0 {
            // The pedals only after every owed note-off (A-5).
            self.pay_controllers(at);
            return;
        }
        let mut full = false;
        self.ledger.entries.retain_mut(|e| {
            if full || !e.owing() {
                return true;
            }
            let taken = self.out.push(owed_off(e.key, at)).is_ok();
            self.out.recovered();
            full = !taken;
            if taken {
                e.held = None;
                e.owner = None;
            }
            !taken || e.refused > 0
        });
        self.ledger.recount();
        if self.ledger.owing == 0 {
            self.pay_controllers(at);
        }
    }

    /// Writes the controller releases the port refused earlier, stopping at the
    /// first it refuses again. Each attempt gives its count back, so a delayed
    /// release nets zero.
    fn pay_controllers(&mut self, at: Offset) {
        let out = &mut *self.out;
        self.ledger.controllers.pay(|release| {
            let taken = out.push(Event::midi(at, release)).is_ok();
            out.recovered();
            taken
        });
    }

    /// Writes `ump` at `at`. A note message is kept track of: a note-on the
    /// port took is sounding, one it refused marks its key so that note's off
    /// is skipped, an off it refused is owed. `owner` says which note a
    /// message belongs to; `None` is an untagged source's message.
    ///
    /// A note-on of a key that sounds ends the sounding note first, on the
    /// same frame, and takes the key. A tagged off ends the key only when its
    /// owner owns it, and is swallowed otherwise.
    pub fn send(&mut self, at: Offset, ump: impl Into<Ump>, owner: Option<T>) {
        let ump = ump.into();
        match Class::of(ump) {
            Class::Other => {
                if self.out.push(Event::midi(at, ump)).is_ok() {
                    self.ledger.controllers.observe(ump);
                }
            }
            Class::On(key) => self.on(at, ump, key, owner),
            Class::Off(key) => self.off(at, ump, key, owner),
        }
    }

    fn on(&mut self, at: Offset, ump: Ump, key: NoteKey, owner: Option<T>) {
        let Some(i) = self.ledger.entry(key) else {
            self.out.count_skipped();
            return;
        };
        if let Some(held) = self.ledger.entries[i].held {
            if self.out.push(owed_off(key, at)).is_err() {
                // The old note stays, its off owed, and the new one does not
                // start.
                self.owe(i);
                self.ledger.refuse(i);
                self.out.count_skipped();
                return;
            }
            if held == Held::Owing {
                self.out.recovered();
            }
            self.ledger.set_held(i, None);
        }
        if self.write(at, ump) {
            self.ledger.set_held(i, Some(Held::Sounding));
            self.ledger.entries[i].owner = owner;
        } else {
            self.ledger.refuse(i);
        }
    }

    fn off(&mut self, at: Offset, ump: Ump, key: NoteKey, owner: Option<T>) {
        let found = self
            .ledger
            .entries
            .binary_search_by_key(&key, |e| e.key)
            .ok();
        if owner.is_none() {
            self.untagged_off(at, ump, found);
            return;
        }
        let Some(i) = found else { return };
        let e = self.ledger.entries[i];
        if e.held.is_some() && e.owner == owner {
            let taken = self.write(at, ump);
            self.finish(i, taken);
        } else if e.refused > 0 {
            // The off of a note whose on the port refused.
            self.settle(i);
        }
    }

    fn write(&mut self, at: Offset, ump: Ump) -> bool {
        self.out.push(Event::midi(at, ump)).is_ok()
    }

    /// An untagged source has one note per key, so its off ends whatever its
    /// key holds and settles a refused on. A key whose on the port refused is
    /// swallowed and counted, as a tagged off is; any other off goes through.
    fn untagged_off(&mut self, at: Offset, ump: Ump, found: Option<usize>) {
        let Some(i) = found else {
            self.write(at, ump);
            return;
        };
        let e = self.ledger.entries[i];
        if e.held.is_some() {
            let taken = self.write(at, ump);
            self.ledger.entries[i].refused = 0;
            self.finish(i, taken);
        } else if e.refused > 0 {
            self.settle(i);
        } else {
            self.write(at, ump);
        }
    }

    /// The off of a note whose on the port refused: swallowed and counted.
    fn settle(&mut self, i: usize) {
        self.ledger.entries[i].refused -= 1;
        self.forget_if_idle(i);
        self.out.count_skipped();
    }

    /// The off of held entry `i` was written (`taken`) or refused.
    fn finish(&mut self, i: usize, taken: bool) {
        if !taken {
            self.owe(i);
            return;
        }
        if self.ledger.entries[i].owing() {
            self.out.recovered();
        }
        self.ledger.set_held(i, None);
        self.ledger.entries[i].owner = None;
        self.forget_if_idle(i);
    }

    /// Drops entry `i` when it holds nothing.
    fn forget_if_idle(&mut self, i: usize) {
        let e = &self.ledger.entries[i];
        if e.held.is_none() && e.refused == 0 {
            self.ledger.entries.remove(i);
        }
    }

    /// The port refused the off of entry `i`: it is owed. The writer counted
    /// the refusal; one already owed is the same loss, taken back.
    fn owe(&mut self, i: usize) {
        if self.ledger.entries[i].owing() {
            self.out.recovered();
        } else {
            self.ledger.set_held(i, Some(Held::Owing));
        }
    }

    /// Ends every note at `at` (playback stopped or jumped, or the clip was
    /// replaced), then returns every hold pedal (CC 64, 66, 67, 69) and pitch
    /// bend the port was told is off its normal position to it, in the
    /// protocol it was sent in; one already at rest sends nothing. MIDI 1.0
    /// Detailed Spec p. 31 (Stop) asks for the return; the pedals come after
    /// the offs because a Hold or Sustain pedal that is on holds a note through
    /// its Note Off and All Notes Off until it is released (A-5). An off the port refuses stays owed. Once the port refuses
    /// one the rest are not tried (it is full), but each is owed and counted:
    /// a note that gets no off is a lost event whether or not its push was
    /// attempted.
    pub fn release_all(&mut self, at: Offset) {
        let mut full = false;
        self.ledger.entries.retain_mut(|e| {
            // What was refused belongs to the pass that is ending.
            e.refused = 0;
            let Some(held) = e.held else { return false };
            if !full && self.out.push(owed_off(e.key, at)).is_ok() {
                if held == Held::Owing {
                    self.out.recovered();
                }
                return false;
            }
            match (full, held) {
                // The refused push counted itself.
                (false, Held::Owing) => self.out.recovered(),
                (true, Held::Sounding) => self.out.count_skipped(),
                _ => {}
            }
            full = true;
            e.held = Some(Held::Owing);
            true
        });
        self.ledger.recount();
        // Then the controllers: a pedal still down holds the notes just ended
        // (MIDI 1.0 Detailed Spec A-5), so it is released after them.
        let out = &mut *self.out;
        let mut full = false;
        self.ledger.controllers.release_all(|release, owed| {
            let attempted = !full;
            if attempted && out.push(Event::midi(at, release)).is_ok() {
                if owed {
                    out.recovered();
                }
                return true;
            }
            match (attempted, owed) {
                // The refused push counted itself.
                (true, true) => out.recovered(),
                (false, false) => out.count_skipped(),
                _ => {}
            }
            full = true;
            false
        });
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod graphs;
