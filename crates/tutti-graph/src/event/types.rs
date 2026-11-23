//! [`Event`], what it carries ([`EventKind`], [`Harmony`]) and `UmpAt`.

use tutti_ump::Ump;

use crate::node::time::Offset;

/// What an [`Event`] says.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    /// A MIDI message, as UMP words.
    Midi(Ump),
    /// A chord or scale taking effect.
    Harmony(Harmony),
    /// A request to an **event source**: re-send where it stands, from this
    /// event's frame, as a resumed segment. What a sink that
    /// starts reading a source late (a control lane whose program newly
    /// reads a held note envelope) needs, since a source sends only when it
    /// moves. Delivered by [`Editor::schedule`](crate::Editor::schedule),
    /// which waits for the commit sent before it, so the re-send reaches the
    /// edges that commit placed. A node that sends nothing a sink could
    /// miss ignores it; so does every param port.
    Resync,
}

/// Whether a [`Harmony`] is a chord or a scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HarmonyKind {
    /// The chord sounding from here: a root, a bass and the degrees.
    Chord,
    /// The scale (key) in force from here: a tonic and the degrees.
    Scale,
}

/// A harmonic context change — a chord or a scale taking effect, as a
/// sequencer's chord and scale lanes say — for a node that follows them (a
/// hosted VST3 plugin's chord and scale events).
///
/// Not MIDI: MIDI 2.0's Flex Data names a chord by type and a key by its
/// signature, which cannot carry an arbitrary degree set (a scale is any of
/// the 4096). Pitches are MIDI note numbers (`0..=127`); `degrees` is a 12-bit
/// mask relative to the root, bit `n` set when semitone `n` belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Harmony {
    kind: HarmonyKind,
    root: u8,
    bass: u8,
    degrees: u16,
}

impl Harmony {
    /// A chord on `root`, `bass` in the bass (equal to `root` in root
    /// position), sounding `degrees`. Pitches past 127 and mask bits past the
    /// twelfth are dropped.
    pub const fn chord(root: u8, bass: u8, degrees: u16) -> Self {
        Self {
            kind: HarmonyKind::Chord,
            root: root & 0x7f,
            bass: bass & 0x7f,
            degrees: degrees & 0xfff,
        }
    }

    /// A scale on the tonic `root` with `degrees`. As [`chord`](Self::chord);
    /// its bass is its root.
    pub const fn scale(root: u8, degrees: u16) -> Self {
        Self {
            kind: HarmonyKind::Scale,
            root: root & 0x7f,
            bass: root & 0x7f,
            degrees: degrees & 0xfff,
        }
    }

    /// Chord or scale.
    pub const fn kind(&self) -> HarmonyKind {
        self.kind
    }

    /// The root (a chord's) or tonic (a scale's), as a MIDI note number.
    pub const fn root(&self) -> u8 {
        self.root
    }

    /// The bass note, as a MIDI note number: a scale's is its root.
    pub const fn bass(&self) -> u8 {
        self.bass
    }

    /// The degrees, a 12-bit mask relative to the root.
    pub const fn degrees(&self) -> u16 {
        self.degrees
    }
}

/// A MIDI packet at its position in the current block: what a MIDI edge
/// (`MidiOut`, a plugin's input) carries.
///
/// An alias, not a type: both halves are already checked ([`Offset`] against
/// a block, [`Ump`] as the packet), so the pair adds no rule of its own, and a
/// site destructures it (`for &(at, ump) in events`). Inside the graph the same
/// pair is an [`Event`] with [`EventKind::Midi`]. A position not yet checked
/// against a block (a plugin's report, a ring crossing threads) is a raw `u32`,
/// not an `Offset`, and so not an `UmpAt`.
pub type UmpAt = (Offset, Ump);

/// One event on an event port: an offset into the current block, and what
/// happens there.
///
/// `offset` is an [`Offset`] — a position inside the block the event is
/// delivered in, never an absolute [`Frame`](tutti_units::Frame) (see
/// [`Offset`]). Slices of events handed to a node are
/// sorted by it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Event {
    /// Where in the block.
    pub offset: Offset,
    /// The payload.
    pub kind: EventKind,
}

// An event is what every event buffer, merge and delay FIFO holds by value,
// preallocated on the control side and copied on the audio thread: it must
// stay `Copy` and small, whatever payload is added (a curve segment packs
// its address and shape tag for this).
// Four UMP words cover every MIDI 2.0 message, 128-bit ones included, so a
// MIDI payload never needs to grow it. Asserted here rather than discovered
// as a slower merge.
const _: () = {
    const fn copy<T: Copy>() {}
    copy::<Event>();
    assert!(std::mem::size_of::<Event>() <= 32);
};

impl Event {
    /// A MIDI event at `offset`.
    pub fn midi(offset: Offset, ump: impl Into<Ump>) -> Self {
        Self {
            offset,
            kind: EventKind::Midi(ump.into()),
        }
    }

    /// Whether this is a MIDI note-off: a MIDI 1.0 channel-voice note-off, a
    /// note-on at velocity 0 (the MIDI 1.0 spelling of the same thing), or a
    /// MIDI 2.0 channel-voice note-off. The one event the graph refuses to
    /// lose — see the delay FIFO's overflow policy.
    #[must_use]
    pub fn is_note_off(&self) -> bool {
        matches!(self.kind, EventKind::Midi(u) if u.is_note_off())
    }

    /// A [`Resync`](EventKind::Resync) request at `offset`.
    pub const fn resync(offset: Offset) -> Self {
        Self {
            offset,
            kind: EventKind::Resync,
        }
    }

    /// A chord or scale taking effect at `offset`.
    pub const fn harmony(offset: Offset, harmony: Harmony) -> Self {
        Self {
            offset,
            kind: EventKind::Harmony(harmony),
        }
    }
}
