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
    FlexAddress, LyricKind, MidiGroup, Syllable, SyllableBody, SyllableEnd, Ump, flex_lyric_packets,
};

use super::{DropReason, LyricChange};

/// One syllable and how it ends.
#[derive(Debug)]
struct Piece {
    body: SyllableBody,
    end: SyllableEnd,
}

/// The syllables of the lyric events of a file, in order, each event's own
/// apart, so that a break at the start of an event reaches the syllable
/// before it.
#[derive(Debug, Default)]
pub(super) struct Lyrics {
    /// The syllables of each lyric event, by the event's index, and what
    /// reading them changed of its text.
    events: Vec<(usize, Result<Vec<Piece>, DropReason>, Vec<LyricChange>)>,
}

impl Lyrics {
    /// Takes the lyric `text` of the event at `index`.
    pub(super) fn push(&mut self, index: usize, text: &str) {
        let (before, syllables, changes) = split(text);
        let raised = before.is_some_and(|end| self.raise_last(end));
        let placed = syllables.and_then(|syllables| {
            if syllables.is_empty() && !raised {
                Err(DropReason::LyricWithoutSyllable)
            } else {
                Ok(syllables)
            }
        });
        self.events.push((index, placed, changes));
    }

    /// Takes the event at `index`, which has no lyric text to read.
    pub(super) fn refuse(&mut self, index: usize, reason: DropReason) {
        self.events.push((index, Err(reason), Vec::new()));
    }

    /// Makes the last syllable so far end at least as `end`: whether there
    /// was one.
    fn raise_last(&mut self, end: SyllableEnd) -> bool {
        let last = self
            .events
            .iter_mut()
            .rev()
            .find_map(|(_, s, _)| s.as_mut().ok().and_then(|s| s.last_mut()));
        last.map(|last| last.end = last.end.max(end)).is_some()
    }

    /// The clip events of each lyric event, with the event's index and what
    /// reading its text changed.
    pub(super) fn into_events(
        self,
    ) -> impl Iterator<Item = (usize, Result<Vec<Ump>, DropReason>, Vec<LyricChange>)> {
        self.events.into_iter().map(|(index, syllables, changes)| {
            let events = syllables.map(|syllables| {
                syllables
                    .into_iter()
                    .flat_map(|Piece { body, end }| {
                        flex_lyric_packets(
                            MidiGroup::FIRST,
                            FlexAddress::Group,
                            LyricKind::Lyrics,
                            &Syllable { body, end },
                        )
                        .collect::<Vec<_>>()
                    })
                    .collect()
            });
            (index, events, changes)
        })
    }
}

/// The end the text asks of the syllable before it with its leading breaks
/// and spaces, its own syllables, and what the reading changed of the text: a
/// leading `/` or `\` ([`LyricChange::BreakMark`]) or hyphens at the end of a
/// syllable ([`LyricChange::HyphenStripped`]).
fn split(
    text: &str,
) -> (
    Option<SyllableEnd>,
    Result<Vec<Piece>, DropReason>,
    Vec<LyricChange>,
) {
    let mut chars = text.chars().peekable();
    let mut before = None;
    let mut changes = Vec::new();
    while let Some(end) = chars.peek().and_then(|&c| match c {
        '/' | '\r' | '\n' => Some(SyllableEnd::Line),
        '\\' => Some(SyllableEnd::Paragraph),
        ' ' => Some(SyllableEnd::Word),
        _ => None,
    }) {
        let c = chars.next();
        if matches!(c, Some('/' | '\\')) && !changes.contains(&LyricChange::BreakMark) {
            changes.push(LyricChange::BreakMark);
        }
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
    let mut refused = None;
    // Each syllable, noting once that hyphens left the text.
    let mut push = |body: &str, end| {
        let (piece, stripped) = syllable(body, end);
        match piece {
            Ok(piece) => syllables.extend(piece),
            Err(reason) => refused = refused.or(Some(reason)),
        }
        if stripped && !changes.contains(&LyricChange::HyphenStripped) {
            changes.push(LyricChange::HyphenStripped);
        }
    };
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
                push(&std::mem::take(&mut body), end);
                end = SyllableEnd::None;
            }
            body.push(c);
        }
    }
    push(&body, end);
    (before, refused.map_or(Ok(syllables), Err), changes)
}

/// `body` as a syllable without the hyphens that marked its word going on, and
/// whether there were any; a body of only hyphens is no syllable.
fn syllable(body: &str, end: SyllableEnd) -> (Result<Option<Piece>, DropReason>, bool) {
    let trimmed = body.trim_end_matches('-');
    let piece = if trimmed.is_empty() {
        Ok(None)
    } else {
        match SyllableBody::try_from(trimmed) {
            Ok(body) => Ok(Some(Piece { body, end })),
            Err(e) => Err(DropReason::Text(e)),
        }
    };
    (piece, trimmed.len() != body.len())
}
