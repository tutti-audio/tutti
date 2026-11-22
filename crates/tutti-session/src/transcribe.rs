//! From a measurement to the timeline: a transcribed note or chord becomes
//! the events a clip or a harmony lane plays.
//!
//! A result's positions are frames ([`Samples`]); the timeline's are beats.
//! [`clip_from_notes`] and [`harmony_from_chords`] are the named conversions,
//! and each takes the project's tempo as a [`TempoView`] (the map read at the
//! session's rate). They compose `tutti-measure` with `tutti-midi-runtime`,
//! neither of which depends on the other, so they live here.
//!
//! A [`NoteMapping`] states the two choices the MIDI spec does not make: what a
//! note's `cents` become, and what its level becomes. Neither choice has a default.

use tutti_graph::Harmony;
use tutti_measure::{ChordQuality, ChordSpan, NoteEvent, VelocityRange};
use tutti_midi_protocol::PitchBendSensitivity;
use tutti_midi_runtime::{
    ClipEvents, HarmonyChanges, HarmonyChangesError, TimedHarmony, TimedMidiEvent,
};
use tutti_tempo::TempoView;
use tutti_ump::{
    Address, BendValue, ControlValue, Message, NoteNumber, NoteVelocity, Rpn, U7, Voice2,
};
use tutti_units::{Beat, Bipolar, Frame, Samples};

/// How a note's measured level becomes the Note On's velocity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LevelVelocity {
    /// By a linear-in-dB map from level to a MIDI 1.0 velocity (`1..=127`),
    /// widened to the MIDI 2.0 field by M2-115 section 3, so the floor is
    /// velocity 1 and the ceiling `0xFFFF`, and 64 is `0x8000`.
    ByLevel(VelocityRange),
    /// Every note at this velocity, whatever its level.
    Fixed(NoteVelocity),
}

/// What a note's `cents` (its distance from the nearest note, `-50..=50`)
/// become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteCents {
    /// Dropped: every note is on its nearest equal-tempered pitch.
    Drop,
    /// A Per-Note Pitch Bend before each Note On (M2-104 section 7.4.13), over
    /// the receiver's per-note sensitivity (RPN 0/7), which the clip also sets
    /// once, at its first note. The bend is the cents over the range, so it is
    /// exact for a receiver at that range.
    PerNotePitchBend(PitchBendSensitivity),
}

/// The choices [`clip_from_notes`] makes for the caller: a level's velocity, a
/// note's cents, and where the notes are sent.
///
/// Neither choice has a default: [`new`](Self::new) takes both. The address is
/// the first Group's first Channel unless [`on`](Self::on) names another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteMapping {
    velocity: LevelVelocity,
    cents: NoteCents,
    address: Address,
}

impl NoteMapping {
    /// Notes at `velocity` and `cents`, on Group 1 Channel 1.
    #[must_use]
    pub const fn new(velocity: LevelVelocity, cents: NoteCents) -> Self {
        Self {
            velocity,
            cents,
            address: Address::first(tutti_ump::MidiChannel::FIRST),
        }
    }

    /// Sends the notes on `address`.
    #[must_use]
    pub const fn on(mut self, address: Address) -> Self {
        self.address = address;
        self
    }

    fn note_velocity(&self, level: tutti_units::Db) -> NoteVelocity {
        match self.velocity {
            LevelVelocity::Fixed(velocity) => velocity,
            LevelVelocity::ByLevel(range) => NoteVelocity::from(U7::masked(range.midi(level))),
        }
    }
}

/// The clip a transcription plays: each note a Note On at its `start` and a
/// Note Off at its `end`, at the beats `tempo` gives those frames.
///
/// With [`NoteCents::PerNotePitchBend`] each Note On is preceded by a Per-Note
/// Pitch Bend of `cents / range` of the way from the centre to the end of its
/// side of the range, saturating at the range when `|cents|` is past it, and the
/// clip's first note is preceded by the sensitivity (RPN 0/7) the bend is
/// scaled by. The Note Off carries no release velocity. A note with no MIDI
/// number (outside `0..=127`) sends nothing. No notes give an empty clip.
#[must_use]
pub fn clip_from_notes(
    notes: &[NoteEvent],
    tempo: &mut TempoView<'_>,
    mapping: NoteMapping,
) -> ClipEvents {
    note_events(notes, tempo, mapping).into_iter().collect()
}

fn note_events(
    notes: &[NoteEvent],
    tempo: &mut TempoView<'_>,
    mapping: NoteMapping,
) -> Vec<TimedMidiEvent> {
    let mut events = Vec::with_capacity(notes.len() * 3 + 1);
    let mut first = None;
    for note in notes {
        let Some(number) = NoteNumber::try_from(note.note).ok() else {
            continue;
        };
        let (start, end) = (
            beat_at(tempo, note.span.start()),
            beat_at(tempo, note.span.end()),
        );
        let mut voice = |beat: Beat, voice: Voice2| {
            events.extend(TimedMidiEvent::new(
                beat,
                Message::Midi2(mapping.address, voice),
            ));
        };
        if let NoteCents::PerNotePitchBend(range) = mapping.cents {
            if first.is_none_or(|f: Beat| start < f) {
                first = Some(start);
            }
            voice(
                start,
                Voice2::per_note_pitch_bend(number, bend(note, range)),
            );
        }
        voice(
            start,
            Voice2::note_on(number, mapping.note_velocity(note.velocity)),
        );
        voice(end, Voice2::note_off(number, NoteVelocity::MIN));
    }
    if let (Some(at), NoteCents::PerNotePitchBend(range)) = (first, mapping.cents) {
        let set = Voice2::registered(
            Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
            ControlValue::new(range.to_rpn_bits()),
        );
        // First in the list, so at the first note's beat it sorts before the bend it scales.
        events.splice(
            0..0,
            TimedMidiEvent::new(at, Message::Midi2(mapping.address, set)),
        );
    }
    events
}

/// The bend of `note`'s cents over `range`, centred at none and saturating at
/// the range's ends. A range of nothing bends nothing.
fn bend(note: &NoteEvent, range: PitchBendSensitivity) -> BendValue {
    let span = range.semitones().to_cents().get();
    if span <= 0.0 {
        return BendValue::CENTER;
    }
    BendValue::from_bipolar(Bipolar::clamped(note.cents.get() / span))
}

fn beat_at(tempo: &mut TempoView<'_>, at: Samples) -> Beat {
    tempo.beat_at_frame(Frame::ZERO + at)
}

/// The harmony lane a chord transcription plays: one change per span, at the
/// beat `tempo` gives its `start`.
///
/// A chord is a triad on its root in the octave from middle C (C is note 60),
/// its bass the root. A span whose quality has no triad here (the quality enum
/// is open) is left out. No spans give no changes.
///
/// # Errors
///
/// [`HarmonyChangesError::SameBeat`] for two chords that start at one beat,
/// and [`HarmonyChangesError::NonFiniteBeat`] for a start the tempo gives no
/// finite beat.
pub fn harmony_from_chords(
    spans: &[ChordSpan],
    tempo: &mut TempoView<'_>,
) -> Result<HarmonyChanges, HarmonyChangesError> {
    HarmonyChanges::new(
        spans
            .iter()
            .filter_map(|chord| {
                let degrees = degrees(chord.quality)?;
                let root = MIDDLE_C + chord.root.semitones_above_c();
                Some(TimedHarmony::new(
                    beat_at(tempo, chord.span.start()),
                    Harmony::chord(root, root, degrees),
                ))
            })
            .collect::<Vec<_>>(),
    )
}

/// The MIDI note number of middle C, the octave a chord's root is placed in.
const MIDDLE_C: u8 = 60;

/// The semitones a triad holds above its root, as the harmony's 12-bit mask.
fn degrees(quality: ChordQuality) -> Option<u16> {
    match quality {
        ChordQuality::Major => Some(0b1001_0001),
        ChordQuality::Minor => Some(0b1000_1001),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
