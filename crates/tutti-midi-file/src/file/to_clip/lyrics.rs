//! SMF lyric text as M2-104 Lyric Data messages.
//!
//! An SMF Lyric meta event (`FF 05`) carries karaoke conventions that
//! M2-104 section 7.5.10 states in its own terms: one syllable a message,
//! the end of a word, line or paragraph as a space, a carriage return or a
//! carriage return and line feed at the end of the syllable, and no hyphen
//! but a genuine one. The SMF conventions are not in a MIDI specification
//! under the project's spec set (the Lyric Meta Event Definition,
//! RP-026, is not there), so the reading here is the common karaoke one:
//!
//! - `/` at the start of an event is a line break and `\` a paragraph break,
//!   before the text that follows it; M2-104 puts the end of a line on the
//!   syllable before, so the previous syllable takes the end.
//! - A space at either end of the text is a word break, a line feed or a
//!   carriage return a line break and a carriage return with a line feed a
//!   paragraph break, whether at the ends or between syllables.
//! - Hyphens at the end of a syllable mark that the word goes on, which the
//!   absence of a word end already says (section 7.5.10: a syllable with no
//!   space, carriage return or line feed at its end is continued by the next
//!   one), so they are not carried: hyphens stay for hyphenated words. A
//!   genuine hyphen at the end of a syllable (`sixty-` then `four`) cannot
//!   be told from the mark and is lost with it.
//! - An event of several words is several messages at its tick, one a word.

use tutti_ump::{
    FlexAddress, LyricKind, MidiGroup, Syllable, SyllableBody, SyllableEnd, SyllableError,
    flex_lyric_packets,
};

use super::{DropReason, event};
use crate::FileEvent;

/// One syllable and how it ends.
#[derive(Debug)]
struct Piece {
    body: String,
    end: SyllableEnd,
}

/// The syllables of the lyric events of a file, in order, each event's own
/// apart, so that a break at the start of an event reaches the syllable
/// before it.
#[derive(Debug, Default)]
pub(super) struct Lyrics {
    /// The syllables of each lyric event, by the event's index.
    events: Vec<(usize, Result<Vec<Piece>, DropReason>)>,
}

impl Lyrics {
    /// Takes the lyric `text` of the event at `index`.
    pub(super) fn push(&mut self, index: usize, text: &str) {
        let (before, syllables) = split(text);
        let raised = before.is_some_and(|end| self.raise_last(end));
        let placed = if syllables.is_empty() && !raised {
            Err(DropReason::LyricWithoutSyllable)
        } else {
            Ok(syllables)
        };
        self.events.push((index, placed));
    }

    /// Takes the event at `index`, which has no lyric text to read.
    pub(super) fn refuse(&mut self, index: usize, reason: DropReason) {
        self.events.push((index, Err(reason)));
    }

    /// Makes the last syllable so far end at least as `end`: whether there
    /// was one.
    fn raise_last(&mut self, end: SyllableEnd) -> bool {
        let last = self
            .events
            .iter_mut()
            .rev()
            .find_map(|(_, s)| s.as_mut().ok().and_then(|s| s.last_mut()));
        last.map(|last| last.end = last.end.max(end)).is_some()
    }

    /// The clip events of each lyric event, with the event's index.
    pub(super) fn into_events(
        self,
    ) -> impl Iterator<Item = (usize, Result<Vec<FileEvent>, DropReason>)> {
        self.events.into_iter().map(|(index, syllables)| {
            let events = syllables.and_then(|syllables| {
                let mut events = Vec::new();
                for Piece { body, end } in syllables {
                    let body = SyllableBody::try_from(&*body).map_err(|e| match e {
                        SyllableError::Nul => DropReason::TextHasNul,
                        SyllableError::TooLong => DropReason::TextTooLong,
                        // The split leaves no separator in a body.
                        other => unreachable!("{other:?} in a syllable the split made"),
                    })?;
                    let packets = flex_lyric_packets(
                        MidiGroup::FIRST,
                        FlexAddress::Group,
                        LyricKind::Lyrics,
                        &Syllable { body, end },
                    );
                    events.extend(packets.map(event));
                }
                Ok(events)
            });
            (index, events)
        })
    }
}

/// The end the text asks of the syllable before it with its leading breaks
/// and spaces, and its own syllables.
fn split(text: &str) -> (Option<SyllableEnd>, Vec<Piece>) {
    let mut chars = text.chars().peekable();
    let mut before = None;
    while let Some(end) = chars.peek().and_then(|&c| match c {
        '/' | '\r' | '\n' => Some(SyllableEnd::Line),
        '\\' => Some(SyllableEnd::Paragraph),
        ' ' => Some(SyllableEnd::Word),
        _ => None,
    }) {
        let c = chars.next();
        // A carriage return and a line feed are a paragraph.
        let end = if c == Some('\r') && chars.peek() == Some(&'\n') {
            chars.next();
            SyllableEnd::Paragraph
        } else {
            end
        };
        before = before.max(Some(end));
    }
    let mut syllables = Vec::new();
    let (mut body, mut end) = (String::new(), SyllableEnd::None);
    while let Some(c) = chars.next() {
        let separator = match c {
            ' ' => Some(SyllableEnd::Word),
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                Some(SyllableEnd::Paragraph)
            }
            '\r' | '\n' => Some(SyllableEnd::Line),
            _ => None,
        };
        if let Some(separator) = separator {
            end = end.max(separator);
        } else {
            if end != SyllableEnd::None {
                push_syllable(&mut syllables, std::mem::take(&mut body), end);
                end = SyllableEnd::None;
            }
            body.push(c);
        }
    }
    push_syllable(&mut syllables, body, end);
    (before, syllables)
}

/// Adds `body` as a syllable, without the hyphens that marked its word going
/// on; a body of only hyphens is no syllable.
fn push_syllable(syllables: &mut Vec<Piece>, body: String, end: SyllableEnd) {
    let body = body.trim_end_matches('-');
    if !body.is_empty() {
        syllables.push(Piece {
            body: body.to_owned(),
            end,
        });
    }
}
