//! MIDI Time Code: assembling quarter-frame messages into SMPTE positions.
//!
//! MTC transmits a wall-clock position one nibble at a time, eight messages per
//! two frames, so a decoder is a small state machine rather than a parse
//! function — [`MtcDecoder`] holds the partial frame between bytes.
//!
//! This is the *wall-clock* half of MIDI sync; the musical half (beat position
//! and inferred tempo) is [`MidiClockDecoder`](crate::MidiClockDecoder). A device sends one or the
//! other, and which you decode is decided by the source, not by preference.

use tutti_midi_protocol::{MtcTimeCode, SmpteTimecode};
use tutti_ump::{Message, System, Ump};

/// Frames the eight pieces of a cycle take to send, and so how old the
/// assembled position is when it completes running forward (MTC, Quarter Frame
/// Message Implementation).
const CYCLE_FRAMES: u64 = 2;

/// The direction the source's time code runs, read from the order of its
/// Quarter Frame pieces (MTC, Quarter Frame Message Implementation: pieces
/// `0` to `7` while running forward, `7` to `0` in reverse).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MtcDirection {
    /// Pieces arrive in ascending order: time runs forward.
    Forward,
    /// Pieces arrive in descending order: time runs backward.
    Reverse,
}

/// One cycle of eight Quarter Frames, assembled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MtcCycle {
    /// The frame the eight pieces name: the frame that starts at the arrival
    /// of piece 0, whichever way the source runs (MTC: piece `0` "must be sent
    /// on a frame boundary", and in reverse it still falls on the boundary of
    /// the frame it represents).
    pub frame: SmpteTimecode,
    /// The way the pieces arrived.
    pub direction: MtcDirection,
}

/// Assembles 8 MTC quarter-frame messages into full SMPTE timecode.
///
/// MTC sends timecode as 8 sequential quarter-frame messages (0xF1 data),
/// each carrying a nibble of the full HH:MM:SS:FF timecode.
#[derive(Debug, Clone)]
pub struct MtcDecoder {
    nibbles: [u8; 8],
    count: u8,
    /// The direction of the cycle being collected; `None` between cycles.
    partial: Option<MtcDirection>,
    /// The piece accepted last; `None` before the first, so a decoder may join mid-cycle.
    last_piece: Option<u8>,
    /// The way the last two pieces ran; `None` after a gap or before the second.
    direction: Option<MtcDirection>,
    cycle: Option<MtcCycle>,
    /// Whether `cycle` completed since [`take_cycle`](Self::take_cycle).
    fresh: bool,
}

impl MtcDecoder {
    /// Builds a decoder holding no partial frame and no timecode.
    ///
    /// [`timecode`](Self::timecode) stays `None` until eight in-order
    /// quarter-frames have arrived.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nibbles: [0; 8],
            count: 0,
            partial: None,
            last_piece: None,
            direction: None,
            cycle: None,
            fresh: false,
        }
    }

    /// Feeds one inbound [`Ump`], as [`MidiClockDecoder::feed`](crate::MidiClockDecoder::feed)
    /// does: a MTC Quarter Frame (0xF1) is taken, any other packet is ignored.
    /// Returns `true` if the packet was a Quarter Frame. It takes no time:
    /// the position is carried in the pieces, not in when they arrived.
    ///
    /// A quarter frame's upper nibble (bits 4-6) identifies the piece (0-7)
    /// and its lower nibble (bits 0-3) carries 4 bits of timecode data.
    ///
    /// Pieces must arrive in order, ascending from piece 0 or descending from
    /// piece 7 ([`direction`](Self::direction)). A gap discards the partial
    /// frame and waits for the next piece that starts a cycle rather than
    /// publishing a timecode assembled from two different frames, so a dropped
    /// byte costs one frame of position, not a wrong one.
    /// [`timecode`](Self::timecode) keeps its previous value across a discarded
    /// frame.
    ///
    /// Allocation-free, so it is safe to call from the audio thread.
    pub fn feed(&mut self, ump: &Ump) -> bool {
        let Ok(Message::System(_, System::TimeCode(code))) = (*ump).decode() else {
            return false;
        };
        self.piece(code.get());
        true
    }

    fn piece(&mut self, quarter_frame: u8) {
        let piece = (quarter_frame >> 4) & 0x07;
        let nibble = quarter_frame & 0x0F;

        // The step from the piece before: one up is forward, one down is
        // reverse (modulo 8: 7 to 0 starts the next forward cycle).
        let step = self.last_piece.and_then(|last| {
            if piece == (last + 1) % 8 {
                Some(MtcDirection::Forward)
            } else if piece == (last + 7) % 8 {
                Some(MtcDirection::Reverse)
            } else {
                None
            }
        });
        self.last_piece = Some(piece);
        self.direction = step;

        // A forward cycle starts at piece 0 and a reverse one at piece 7. With
        // no step to say which (a decoder joining, or after a gap) either may
        // start one: the next piece settles it, and a piece that disagrees
        // discards it. Any other piece must continue the cycle in the direction
        // it began in, or the partial frame is discarded and the decoder waits
        // for the next start.
        let begins = match (step, piece) {
            (Some(MtcDirection::Reverse) | None, 7) => Some(MtcDirection::Reverse),
            (Some(MtcDirection::Forward) | None, 0) => Some(MtcDirection::Forward),
            _ => None,
        };
        if let Some(direction) = begins {
            self.count = 0;
            self.partial = Some(direction);
        } else if step.is_none() || self.partial != step {
            self.count = 0;
            self.partial = None;
            return;
        }

        self.nibbles[piece as usize] = nibble;
        self.count += 1;

        if self.count >= 8 {
            if let Some(direction) = self.partial {
                self.assemble(direction);
            }
            self.count = 0;
            self.partial = None;
        }
    }

    fn assemble(&mut self, direction: MtcDirection) {
        // A position the wire framed but no clock reads (hour 28, frame 31) is
        // a corrupt frame: it is dropped, and `timecode` keeps its last value.
        if let Some(position) = MtcTimeCode::from_nibbles(self.nibbles) {
            self.cycle = Some(MtcCycle {
                frame: position.timecode(),
                direction,
            });
            self.fresh = true;
        }
    }

    /// The way the last two pieces ran, or `None` before a second piece and
    /// after a gap (a piece that is neither one up nor one down from the one
    /// before).
    ///
    /// Known from two pieces, a quarter frame in, not at the end of a cycle:
    /// a source that turns round is seen at once.
    #[must_use]
    pub fn direction(&self) -> Option<MtcDirection> {
        self.direction
    }

    /// The piece (0 to 7) fed last, or `None` before the first.
    #[must_use]
    pub fn last_piece(&self) -> Option<u8> {
        self.last_piece
    }

    /// The cycle the last [`feed`](Self::feed) completed, once: `None` when it
    /// completed none, or when it was taken already.
    pub fn take_cycle(&mut self) -> Option<MtcCycle> {
        std::mem::take(&mut self.fresh)
            .then_some(self.cycle)
            .flatten()
    }

    /// The most recently completed timecode, or `None` before the first full
    /// frame has been assembled.
    ///
    /// It is the time of the last quarter-frame that completed it. Running
    /// forward, the eight pieces name the frame piece 0 fell on, and by the time
    /// the last quarter frame message (F1 7X) arrives the information is 2
    /// frames old, so a receiver keeps an internal offset of +2 frames (MTC,
    /// Quarter Frame Message Implementation); the decoder applies it (hours wrap
    /// at 24). Running in reverse the last piece is piece 0, which falls on the
    /// boundary of the frame it represents, and no offset is applied. The
    /// frame itself, whichever way, is [`MtcCycle::frame`]. MTC transmits one
    /// full position per *two* frames, so this is current only at the instant
    /// the cycle completes.
    pub fn timecode(&self) -> Option<SmpteTimecode> {
        self.cycle.map(|cycle| match cycle.direction {
            MtcDirection::Forward => SmpteTimecode::from_frame_count(
                cycle.frame.frame_count() + CYCLE_FRAMES,
                cycle.frame.frame_rate(),
            ),
            MtcDirection::Reverse => cycle.frame,
        })
    }

    /// Drops the partial frame and the last completed timecode.
    ///
    /// Call this on a locate or a transport stop: without it,
    /// [`timecode`](Self::timecode) keeps reporting the old position until a
    /// whole new frame arrives.
    pub fn reset(&mut self) {
        self.nibbles = [0; 8];
        self.count = 0;
        self.partial = None;
        self.last_piece = None;
        self.direction = None;
        self.cycle = None;
        self.fresh = false;
    }
}

impl Default for MtcDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
