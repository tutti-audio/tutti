//! Song Position Pointer from a beat (M1 p.30; M2-104 section 7.6): its 14-bit
//! count of MIDI beats, each a sixteenth note.

use tutti_ump::U14;
use tutti_units::Beat;

/// MIDI beats in one quarter-note beat: a MIDI beat is a sixteenth note.
const MIDI_BEATS_PER_BEAT: f64 = 4.0;

/// The Song Position Pointer that names `beat`: sixteenths from the start,
/// rounded to the nearest. `None` for a beat the field cannot hold: before the
/// start, NaN or infinite, or past the 14-bit field (the largest position is
/// 16383 sixteenths, a beat short of 4096).
///
/// ```
/// use tutti_midi_protocol::song_position;
/// use tutti_units::Beat;
///
/// assert_eq!(song_position(Beat::new(2.0)).map(|p| p.get()), Some(8));
/// assert_eq!(song_position(Beat::new(-1.0)), None);
/// assert_eq!(song_position(Beat::new(4096.0)), None);
/// ```
#[must_use]
pub fn song_position(beat: Beat) -> Option<U14> {
    // A beat before the start is refused whole, not rounded up to 0.
    if beat.get() < 0.0 {
        return None;
    }
    let midi_beats = libm::round(beat.get() * MIDI_BEATS_PER_BEAT);
    // NaN and the infinities fail the range test; the cast is of a whole
    // number in the field.
    if !(0.0..=f64::from(U14::MAX.get())).contains(&midi_beats) {
        return None;
    }
    U14::new(midi_beats as u16)
}
