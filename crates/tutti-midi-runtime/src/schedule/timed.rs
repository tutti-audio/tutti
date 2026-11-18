//! [`TimedMidiEvent`]: a MIDI event at a beat.

use tutti_ump::Ump;
use tutti_units::Beat;

/// One [`Ump`] tagged with the finite beat at which it fires.
///
/// What a [`MidiClipNode`](super::MidiClipNode) plays and clip-file import
/// yields, so a parsed file drops straight into a clip. The beat is checked
/// when the value is built ([`new`](Self::new)), so a clip never meets a beat
/// that has no frame.
///
/// [`Beat`] is `tutti-units`' unit over an `f64`, which holds NaN and the
/// infinities; this type is where an event's beat is refused for them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedMidiEvent {
    event: Ump,
    beat: Beat,
}

impl TimedMidiEvent {
    /// A timed event at `beat`, or `None` when `beat` is not finite (NaN or
    /// infinite: it names no frame). Field-order-independent, so callers never
    /// have to remember whether `beat` or `event` comes first.
    #[inline]
    #[must_use]
    pub fn new(beat: Beat, event: impl Into<Ump>) -> Option<Self> {
        let event = event.into();
        beat.get().is_finite().then_some(Self { event, beat })
    }

    /// This event moved `by` later (earlier for a negative duration), or
    /// `None` when the moved beat is not finite.
    #[inline]
    #[must_use]
    pub fn shifted(self, by: tutti_units::BeatDuration) -> Option<Self> {
        Self::new(self.beat + by, self.event)
    }

    /// The event itself. It carries no offset in storage: [`beat`](Self::beat)
    /// is the authority, and a reader stamps the offset from it at poll time.
    #[inline]
    #[must_use]
    pub const fn event(&self) -> Ump {
        self.event
    }

    /// Beat position when this event should trigger: always finite.
    #[inline]
    #[must_use]
    pub const fn beat(&self) -> Beat {
        self.beat
    }
}
