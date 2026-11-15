//! Song Position Pointer as a beat (M1 p.30; M2-104 section 7.6): the one
//! reading of its 14-bit count of MIDI beats, each a sixteenth note, that the
//! sender and the follower share.

use tutti_types::Beat;
use tutti_ump::U14;

/// MIDI beats in one quarter-note beat: a MIDI beat is a sixteenth note.
const MIDI_BEATS_PER_BEAT: f64 = 4.0;

/// The Song Position Pointer that names `beat`: sixteenths from the start,
/// rounded to the nearest. A position before the start (or NaN) is the start,
/// and one past the 14-bit field (4096 beats) sticks at the largest.
///
/// ```
/// use tutti_midi_protocol::song_position;
/// use tutti_types::Beat;
///
/// assert_eq!(song_position(Beat::new(2.0)).get(), 8);
/// assert_eq!(song_position(Beat::new(-1.0)).get(), 0);
/// ```
#[must_use]
pub fn song_position(beat: Beat) -> U14 {
    let midi_beats = (beat.get().max(0.0) * MIDI_BEATS_PER_BEAT).round();
    // `max(0.0)` turned NaN and negatives into 0, and `as` saturates.
    U14::new(midi_beats as u16).unwrap_or(U14::MAX)
}

/// The beat a Song Position Pointer names, the inverse of [`song_position`].
#[must_use]
pub fn song_position_beat(position: U14) -> Beat {
    Beat::new(f64::from(position.get()) / MIDI_BEATS_PER_BEAT)
}
