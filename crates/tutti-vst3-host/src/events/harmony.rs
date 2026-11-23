//! Chord, scale and note-expression text events, with their text as UTF-16.

use tutti_vst3_ffi::{MAX_EVENT_TEXT_LEN, Staging};

use super::{Chord, Event, ExpressionText, Payload, Scale};

/// The text of one harmony or text event as UTF-16 code units: the most an event
/// list's arena takes for one text ([`MAX_EVENT_TEXT_LEN`]), held inline, so a
/// block's texts are built without the heap. A longer name is cut at the room
/// and the buffer reports it ([`is_overflowed`](Self::is_overflowed)).
pub struct EventText {
    units: [u16; MAX_EVENT_TEXT_LEN],
    len: usize,
    overflowed: bool,
}

impl EventText {
    /// An empty text.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            units: [0; MAX_EVENT_TEXT_LEN],
            len: 0,
            overflowed: false,
        }
    }

    /// Appends `unit`, or latches [`is_overflowed`](Self::is_overflowed) and
    /// drops it when the room is full.
    pub fn push(&mut self, unit: u16) -> bool {
        let Some(slot) = self.units.get_mut(self.len) else {
            self.overflowed = true;
            return false;
        };
        *slot = unit;
        self.len += 1;
        true
    }

    /// Appends `units` until the room is full; the count written.
    pub fn extend(&mut self, units: impl IntoIterator<Item = u16>) -> usize {
        let mut written = 0;
        for unit in units {
            if !self.push(unit) {
                break;
            }
            written += 1;
        }
        written
    }

    /// Removes the last unit; the overflow flag stays as it is.
    pub fn pop(&mut self) -> Option<u16> {
        self.len = self.len.checked_sub(1)?;
        Some(self.units[self.len])
    }

    /// The units written.
    #[must_use]
    pub fn as_slice(&self) -> &[u16] {
        &self.units[..self.len]
    }

    /// Whether a unit was dropped for want of room.
    #[must_use]
    pub const fn is_overflowed(&self) -> bool {
        self.overflowed
    }
}

impl Default for EventText {
    fn default() -> Self {
        Self::new()
    }
}

impl core::ops::Deref for EventText {
    type Target = [u16];

    fn deref(&self) -> &[u16] {
        self.as_slice()
    }
}

impl core::fmt::Debug for EventText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

/// `src` as an [`EventText`], cut at the room, on a character boundary: a
/// surrogate pair the cut would split is dropped whole. Never allocates.
#[must_use]
pub fn event_text(src: &str) -> EventText {
    let mut text = EventText::new();
    text.extend(src.encode_utf16());
    if text.is_overflowed()
        && text
            .last()
            .is_some_and(|unit| (0xD800..0xDC00).contains(unit))
    {
        text.pop();
    }
    text
}

/// A chord context change for [`InputEvents`](super::InputEvents).
#[derive(Debug)]
pub struct Utf16Chord {
    /// Frame offset within the block the event is staged into.
    pub sample_offset: i32,
    /// Chord root as a MIDI note number, 0..=127.
    pub root: i16,
    /// Bass note as a MIDI note number, 0..=127.
    pub bass_note: i16,
    /// Bitmask of chord degrees present.
    pub mask: i16,
    /// Display name as UTF-16 code units, held until staging interns it.
    pub text: EventText,
}

impl Utf16Chord {
    /// The event, its name interned into the block's arena.
    pub fn to_event(&self, staging: &mut Staging<'_>) -> Event {
        Event::at(
            self.sample_offset,
            Payload::Chord(Chord {
                root: self.root,
                bass_note: self.bass_note,
                mask: self.mask,
                text: staging.intern(self.text.as_slice()),
            }),
        )
    }
}

/// A scale context change for [`InputEvents`](super::InputEvents).
#[derive(Debug)]
pub struct Utf16Scale {
    /// Frame offset within the block the event is staged into.
    pub sample_offset: i32,
    /// Scale root as a MIDI note number, 0..=127.
    pub root: i16,
    /// 12-bit mask of scale degrees, bit 0 = C.
    pub mask: i16,
    /// Display name as UTF-16 code units, held until staging interns it.
    pub text: EventText,
}

impl Utf16Scale {
    /// The event, its name interned into the block's arena.
    pub fn to_event(&self, staging: &mut Staging<'_>) -> Event {
        Event::at(
            self.sample_offset,
            Payload::Scale(Scale {
                root: self.root,
                mask: self.mask,
                text: staging.intern(self.text.as_slice()),
            }),
        )
    }
}

/// A per-note text annotation for [`InputEvents`](super::InputEvents).
#[derive(Debug)]
pub struct Utf16Text {
    /// Frame offset within the block the event is staged into.
    pub sample_offset: i32,
    /// Note id assigned by the originating note-on.
    pub note_id: i32,
    /// The note-expression type id the text annotates.
    pub type_id: u32,
    /// The annotation as UTF-16 code units, held until staging interns it.
    pub text: EventText,
}

impl Utf16Text {
    /// The event, its text interned into the block's arena.
    pub fn to_event(&self, staging: &mut Staging<'_>) -> Event {
        Event::at(
            self.sample_offset,
            Payload::ExpressionText(ExpressionText {
                type_id: self.type_id,
                note_id: self.note_id,
                text: staging.intern(self.text.as_slice()),
            }),
        )
    }
}
