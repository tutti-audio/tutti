//! [`MidiKeys`]: the key helpers over an event inlet's writer.
//!
//! Copying events into the graph is `tutti-graph`'s event inlet (an
//! [`EventInletNode`](tutti_graph::EventInletNode) and its
//! [`EventInletWriter`]); the graph builds no message. What a keyboard or a
//! preview needs on top of it is a Note On and a Note Off built as MIDI 2.0
//! packets on the Group the keys were made for, and that is
//! all this is: the writer is borrowed through `Deref`, so `push`, `push_all`,
//! `clock` and the counts are the inlet's own.

use std::ops::{Deref, DerefMut};

use tutti_graph::{EventCapacity, EventInletWriter, Stamp};
use tutti_midi_protocol::Addressed;
use tutti_rt::PushError;
use tutti_ump::{
    Address, KeyBitset, Message, MidiChannel, MidiGroup, NoteKey, NoteNumber, NoteVelocity,
    Sysex7Packets, SysexPart, Voice2,
};
use tutti_units::Losses;

/// The ring a live input's inlet is opened with, in events (it is also the
/// waiting list's size and the port's declared capacity).
pub const LIVE_INLET_CAPACITY: EventCapacity = EventCapacity::of(256);

/// An [`EventInletWriter`] with the key helpers. One producer, as the writer
/// is: share it behind a lock on the control side if it must be `Clone`.
///
/// **Keys are a performance, not part of a bounce.** What they send plays in
/// the live graph and reaches the device output, but an event inlet's fork is
/// silent, so a bounce renders the instrument with none of it. Notes an export
/// must hold are a clip on the timeline (`MidiClipNode`, through the
/// session's `play_clip`).
///
/// **Dropping the keys releases what they hold**: a Note Off goes for each note
/// [`note_on`](Self::note_on) started and [`note_off`](Self::note_off) has not
/// ended, so dropped keys never leave a note stuck. A Note Off the ring has no room
/// for is counted as the writer counts a refused event. A note started through the
/// writer's own `push` is not tracked and is not released.
#[derive(Debug)]
pub struct MidiKeys {
    writer: EventInletWriter,
    group: MidiGroup,
    /// The notes the helpers started and have not ended.
    held: Box<KeyBitset>,
}

impl MidiKeys {
    /// The helpers over `writer` (from `Graph::open_event_inlet` or
    /// `EventInletNode::pair`, opened with [`LIVE_INLET_CAPACITY`]), sending on
    /// `group`: the one Group of sixteen channels an instrument plays
    /// (`MidiGroup::FIRST` for the first).
    #[must_use]
    pub fn new(writer: EventInletWriter, group: MidiGroup) -> Self {
        Self {
            writer,
            group,
            held: Box::new(KeyBitset::new()),
        }
    }

    /// What the inlet has lost, in events (the writer's
    /// [`counts`](EventInletWriter::counts), read).
    #[must_use]
    pub fn losses(&self) -> Losses {
        self.writer.counts().losses()
    }

    /// The Group every key helper sends on.
    #[must_use]
    pub fn group(&self) -> MidiGroup {
        self.group
    }

    /// A MIDI 2.0 Note On at `velocity` on the keys' Group, for `at`.
    ///
    /// The velocity is the 16-bit field of M2-104 §7.4.2, `0` to `0xFFFF`, and
    /// not a MIDI 1.0 velocity: `NoteVelocity::new(100)` is nearly silent. A
    /// velocity of 0 is a Note On.
    ///
    /// # Errors
    ///
    /// As [`EventInletWriter::push`].
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_graph::{EventCapacity, EventInletNode, Stamp};
    /// use tutti_midi_runtime::MidiKeys;
    /// use tutti_ump::{MidiChannel, MidiGroup, NoteNumber, NoteVelocity};
    ///
    /// let (_node, writer) = EventInletNode::pair(EventCapacity::of(16));
    /// let mut keys = MidiKeys::new(writer, MidiGroup::FIRST);
    /// // Three quarters of the 16-bit range.
    /// keys.note_on(
    ///     Stamp::NextBlock,
    ///     MidiChannel::FIRST,
    ///     NoteNumber::of(60),
    ///     NoteVelocity::new(0xC000),
    /// )
    /// .unwrap();
    /// ```
    pub fn note_on(
        &mut self,
        at: impl Into<Stamp>,
        channel: MidiChannel,
        note: NoteNumber,
        velocity: NoteVelocity,
    ) -> Result<(), PushError> {
        self.voice(at, channel, Voice2::note_on(note, velocity))?;
        self.held.insert(self.key(channel, note));
        Ok(())
    }

    /// A MIDI 2.0 Note Off at release velocity 0 on the keys' Group, for `at`. A full
    /// ring refuses it, and the note stays held: dropping the keys sends its Note Off.
    ///
    /// # Errors
    ///
    /// As [`EventInletWriter::push`].
    pub fn note_off(
        &mut self,
        at: impl Into<Stamp>,
        channel: MidiChannel,
        note: NoteNumber,
    ) -> Result<(), PushError> {
        self.voice(at, channel, Voice2::note_off(note, NoteVelocity::MIN))?;
        self.held.remove(self.key(channel, note));
        Ok(())
    }

    /// A Universal System Exclusive message on the keys' Group, for `at`: the
    /// SysEx7 packets of [`Addressed::payload`], every one stamped `at`.
    ///
    /// All or nothing: the ring is asked for room for every packet first
    /// ([`vacant`](EventInletWriter::vacant); the writer is the only producer,
    /// so the room holds), and a message that does not fit is refused whole,
    /// never cut. The packets are made one at a time from a payload on the
    /// stack, so nothing allocates. Like the other key helpers it is a
    /// control-side call, and what it sends is a performance, not part of a
    /// bounce.
    ///
    /// # Errors
    ///
    /// [`PushError::Full`] when the ring cannot hold every packet and
    /// [`PushError::Disconnected`] once the node is dropped. Nothing is written
    /// for either, and a refusal for `Full` is not counted as a loss.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_graph::{EventCapacity, EventInletNode, Stamp};
    /// use tutti_midi_protocol::{Addressed, GeneralMidiMode, Recipient, Universal};
    /// use tutti_midi_runtime::MidiKeys;
    /// use tutti_ump::MidiGroup;
    ///
    /// let (_node, writer) = EventInletNode::pair(EventCapacity::of(16));
    /// let mut keys = MidiKeys::new(writer, MidiGroup::FIRST);
    /// keys.send_universal(
    ///     Stamp::NextBlock,
    ///     Addressed {
    ///         to: Recipient::AllCall,
    ///         message: Universal::GeneralMidi(GeneralMidiMode::On),
    ///     },
    /// )
    /// .unwrap();
    /// ```
    pub fn send_universal(
        &mut self,
        at: impl Into<Stamp>,
        message: Addressed,
    ) -> Result<(), PushError> {
        let payload = message.payload();
        let packets = Sysex7Packets::part(self.group, &payload, SysexPart::Only);
        if packets.len() > self.writer.vacant() {
            return Err(PushError::Full);
        }
        let at = at.into();
        for packet in packets {
            self.writer.push(at, packet)?;
        }
        Ok(())
    }

    /// The held-note key of `note` on `channel` of the keys' Group.
    fn key(&self, channel: MidiChannel, note: NoteNumber) -> NoteKey {
        NoteKey::new(
            Address {
                group: self.group,
                channel,
            },
            note,
        )
    }

    fn voice(
        &mut self,
        at: impl Into<Stamp>,
        channel: MidiChannel,
        voice: Voice2,
    ) -> Result<(), PushError> {
        let address = Address {
            group: self.group,
            channel,
        };
        self.writer.push(at, Message::Midi2(address, voice))
    }
}

impl Drop for MidiKeys {
    /// Sends a Note Off for each note still held, for the next block. A refusal is
    /// the writer's: a full ring counts it, and a dropped node has nothing to end.
    fn drop(&mut self) {
        for key in self.held.iter() {
            let address = key.address();
            let off = Voice2::note_off(key.note(), NoteVelocity::MIN);
            let _ = self
                .writer
                .push(Stamp::NextBlock, Message::Midi2(address, off));
        }
    }
}

impl Deref for MidiKeys {
    type Target = EventInletWriter;
    fn deref(&self) -> &EventInletWriter {
        &self.writer
    }
}

impl DerefMut for MidiKeys {
    fn deref_mut(&mut self) -> &mut EventInletWriter {
        &mut self.writer
    }
}

#[cfg(test)]
mod tests;
