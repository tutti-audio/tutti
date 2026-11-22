//! The writer of Flex Data text (M2-104 sections 7.5.9 to 7.5.12): each kind
//! of text has an entry point whose parameters are the values the
//! specification allows, so a wrong message cannot be written.

use core::iter::FusedIterator;

use thiserror::Error;
use tutti_units::TextError;

use crate::iso8601::is_date_with_optional_time;
use crate::{
    Flex, FlexAddress, FlexTextKind, Form, Message, MidiGroup, TextPackets, Ump, text_packets,
};

/// The bytes one Flex text packet carries.
pub(crate) const BYTES_PER_PACKET: usize = 12;

/// The most packets one Flex Data message has (M2-104 section 7.5.1).
const MAX_PACKETS: usize = 32;

/// The longest text one Flex Data message carries: a `Start`, 30 `Continue`
/// and an `End` of 12 bytes each.
pub const FLEX_TEXT_MAX_LEN: usize = MAX_PACKETS * BYTES_PER_PACKET;

/// Why a [`Text`] cannot be made.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FlexTextError {
    /// The text holds a NUL, which is the padding: a reader cannot tell it
    /// from the end of the text.
    #[error("a Flex text holds no NUL byte")]
    Nul,
    /// The text is longer than [`FLEX_TEXT_MAX_LEN`] bytes.
    #[error("a Flex text is at most {FLEX_TEXT_MAX_LEN} bytes")]
    TooLong,
    /// The text starts with a byte order mark, which a Flex text does not
    /// carry (M2-104 section 7.5.9).
    #[error("a Flex text does not start with a byte order mark")]
    Bom,
}

/// Why an [`Entry`] cannot be made.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EntryError {
    /// The entry holds a comma, which separates the entries of a list.
    #[error("an entry of a list holds no comma")]
    Comma,
    /// The entry is empty: in a list it would leave two separators in a row.
    #[error("an entry of a list is not empty")]
    Empty,
    /// The entry is not a [`Text`].
    #[error(transparent)]
    Text(#[from] FlexTextError),
}

/// A text a Flex Data message carries: UTF-8 with no NUL, no leading byte
/// order mark and at most [`FLEX_TEXT_MAX_LEN`] bytes (M2-104 sections 7.5.1
/// and 7.5.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Text(tutti_units::Text<0, FLEX_TEXT_MAX_LEN>);

impl TryFrom<&str> for Text {
    type Error = FlexTextError;

    /// The text `text`.
    ///
    /// # Errors
    ///
    /// [`FlexTextError::Nul`] for a NUL, [`FlexTextError::Bom`] for a leading
    /// byte order mark, [`FlexTextError::TooLong`] past [`FLEX_TEXT_MAX_LEN`]
    /// bytes.
    fn try_from(text: &str) -> Result<Self, FlexTextError> {
        tutti_units::Text::try_from(text)
            .map(Self)
            .map_err(|error| match error {
                TextError::Nul => FlexTextError::Nul,
                TextError::Bom => FlexTextError::Bom,
                // `Text<0, _>` has no minimum, so `TooShort` cannot occur.
                TextError::TooLong { .. } | TextError::TooShort { .. } => FlexTextError::TooLong,
            })
    }
}

impl TryFrom<&[Entry]> for Text {
    type Error = FlexTextError;

    /// The entries as one text, separated by a comma and a space: "John
    /// Lennon, Paul McCartney" (M2-104 section 7.5.9.1, Multiple
    /// Entities/Names).
    ///
    /// # Errors
    ///
    /// [`FlexTextError::TooLong`] past [`FLEX_TEXT_MAX_LEN`] bytes.
    ///
    /// ```
    /// use tutti_ump::{Entry, Text};
    ///
    /// let entries = ["John Lennon", "Paul McCartney"].map(|e| Entry::try_from(e).unwrap());
    /// let composers = Text::try_from(&entries[..]).unwrap();
    /// assert_eq!(composers.as_str(), "John Lennon, Paul McCartney");
    /// ```
    fn try_from(entries: &[Entry]) -> Result<Self, FlexTextError> {
        let mut bytes = [0; FLEX_TEXT_MAX_LEN];
        let mut len = 0;
        for (i, entry) in entries.iter().enumerate() {
            for part in [if i > 0 { ", " } else { "" }, entry.0.as_str()] {
                let end = len + part.len();
                bytes
                    .get_mut(len..end)
                    .ok_or(FlexTextError::TooLong)?
                    .copy_from_slice(part.as_bytes());
                len = end;
            }
        }
        // The bytes are whole `&str`s, so they are UTF-8.
        core::str::from_utf8(&bytes[..len])
            .map_err(|_| FlexTextError::TooLong)
            .and_then(Self::try_from)
    }
}

impl Text {
    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_str().as_bytes()
    }
}

/// One entity of a list in a text, such as one composer of a Composer Name
/// message: a [`Text`] with no comma, as the comma separates the entities
/// (M2-104 section 7.5.9.1). A list is `Text::try_from(&[Entry][..])`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry(Text);

impl TryFrom<&str> for Entry {
    type Error = EntryError;

    /// The entry `entry`.
    ///
    /// # Errors
    ///
    /// [`EntryError::Empty`] for an empty entry and [`EntryError::Comma`]
    /// for a comma; otherwise [`EntryError::Text`], as for [`Text`].
    fn try_from(entry: &str) -> Result<Self, EntryError> {
        if entry.is_empty() {
            return Err(EntryError::Empty);
        }
        if entry.contains(',') {
            return Err(EntryError::Comma);
        }
        Ok(Text::try_from(entry).map(Self)?)
    }
}

/// An ISO 8601 date with an optional time, as a Recording/Concert Date holds
/// it (M2-104 section 7.5.9.2). It keeps the text it was parsed from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Iso8601Date(Text);

/// A text that is not an ISO 8601 date with an optional time.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[error("not an ISO 8601 date with an optional time")]
pub struct Iso8601DateError;

impl core::str::FromStr for Iso8601Date {
    type Err = Iso8601DateError;

    /// Parses `text`: a year (`2026`), a month (`2026-11`), a calendar date
    /// (`2026-11-15`, `20261115`), an ordinal date (`2026-319`, `2026319`) or
    /// a week date (`2026-W46`, `2026-W46-7`, `2026W467`). A date to the day
    /// (a calendar, ordinal or week day) may be followed by `T` and a time:
    /// `hh`, `hh:mm` or `hh:mm:ss` (the basic form has no colons) with a
    /// fraction (`.` or `,`) of the last part, and `24:00` for the end of the
    /// day, then `Z` or an offset `+hh`, `+hh:mm` (`+hhmm`) or the same with
    /// `-`. A year, a month or a week has no time: ISO 8601 defines none for
    /// reduced precision. The date and the time are both in the extended
    /// form or both in the basic one.
    ///
    /// # Errors
    ///
    /// [`Iso8601DateError`] for any other text, or a part out of its range
    /// (a month 13, February 29 in a year that is not a leap year, a week
    /// 53 in a year of 52).
    ///
    /// ```
    /// use tutti_ump::Iso8601Date;
    ///
    /// assert!("2026-11-15T10:30:00+09:00".parse::<Iso8601Date>().is_ok());
    /// assert!("11/15/2026".parse::<Iso8601Date>().is_err());
    /// ```
    fn from_str(text: &str) -> Result<Self, Iso8601DateError> {
        if !is_date_with_optional_time(text) {
            return Err(Iso8601DateError);
        }
        Text::try_from(text).map(Self).map_err(|_| Iso8601DateError)
    }
}

impl Iso8601Date {
    /// The text it was parsed from.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// The texts of Table 16 that are plain text: not a name sent to the Group,
/// a date or a lyric, which have their own entry points.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PlainTextKind {
    /// Unknown Metadata Text (bank 1, status 0).
    UnknownMetadata,
    /// Composition (Song) Name.
    CompositionName,
    /// Copyright Notice.
    Copyright,
    /// Composer Name.
    Composer,
    /// Lyricist Name.
    Lyricist,
    /// Arranger Name.
    Arranger,
    /// Publisher Name.
    Publisher,
    /// Primary Performer Name.
    PrimaryPerformer,
    /// Accompanying Performer Name.
    AccompanyingPerformer,
    /// Recording/Concert Location.
    RecordingLocation,
    /// Unknown Performance Text Events (bank 2, status 0).
    UnknownPerformance,
    /// Lyrics Language (a BCP 47 identifier).
    LyricsLanguage,
    /// Ruby Language (a BCP 47 identifier).
    RubyLanguage,
}

impl From<PlainTextKind> for FlexTextKind {
    fn from(kind: PlainTextKind) -> Self {
        match kind {
            PlainTextKind::UnknownMetadata => Self::UnknownMetadata,
            PlainTextKind::CompositionName => Self::CompositionName,
            PlainTextKind::Copyright => Self::Copyright,
            PlainTextKind::Composer => Self::Composer,
            PlainTextKind::Lyricist => Self::Lyricist,
            PlainTextKind::Arranger => Self::Arranger,
            PlainTextKind::Publisher => Self::Publisher,
            PlainTextKind::PrimaryPerformer => Self::PrimaryPerformer,
            PlainTextKind::AccompanyingPerformer => Self::AccompanyingPerformer,
            PlainTextKind::RecordingLocation => Self::RecordingLocation,
            PlainTextKind::UnknownPerformance => Self::UnknownPerformance,
            PlainTextKind::LyricsLanguage => Self::LyricsLanguage,
            PlainTextKind::RubyLanguage => Self::RubyLanguage,
        }
    }
}

/// Which of the two lyric texts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LyricKind {
    /// Lyric Data (section 7.5.10).
    Lyrics,
    /// Ruby Data (section 7.5.12).
    Ruby,
}

impl From<LyricKind> for FlexTextKind {
    fn from(kind: LyricKind) -> Self {
        match kind {
            LyricKind::Lyrics => Self::Lyrics,
            LyricKind::Ruby => Self::Ruby,
        }
    }
}

/// Why a [`SyllableBody`] cannot be made.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SyllableError {
    /// A space, carriage return or line feed inside the syllable: a message
    /// is one syllable, and these end a word, a line or a paragraph.
    #[error("a syllable holds no space, carriage return or line feed")]
    Separator,
    /// The syllable is not a [`Text`], or is too long to keep room for the
    /// longest end, a carriage return and a line feed.
    #[error(transparent)]
    Text(#[from] FlexTextError),
}

/// The text of one syllable of a Lyric Data or Ruby Data message: no space,
/// carriage return, line feed or NUL (M2-104 sections 7.5.10 and 7.5.12). It
/// may be empty, for a message that only ends a word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SyllableBody(Text);

impl TryFrom<&str> for SyllableBody {
    type Error = SyllableError;

    /// The syllable `body`.
    ///
    /// # Errors
    ///
    /// [`SyllableError::Separator`] for a space, carriage return or line
    /// feed; otherwise [`SyllableError::Text`], for a NUL or a body past
    /// [`FLEX_TEXT_MAX_LEN`] bytes less the two of the longest end.
    fn try_from(body: &str) -> Result<Self, SyllableError> {
        if body.contains([' ', '\r', '\n']) {
            return Err(SyllableError::Separator);
        }
        if body.len() > FLEX_TEXT_MAX_LEN - 2 {
            return Err(FlexTextError::TooLong.into());
        }
        Ok(Text::try_from(body).map(Self)?)
    }
}

impl SyllableBody {
    /// The syllable.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// How a syllable ends: what, at the end of the text, M2-104 sections 7.5.10
/// and 7.5.12 give as the end of a word, a line or a paragraph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SyllableEnd {
    /// Nothing: the word goes on in the next message.
    None,
    /// A space: the word ends.
    Word,
    /// A carriage return: the line ends.
    Line,
    /// A carriage return and a line feed: the paragraph ends.
    Paragraph,
}

impl SyllableEnd {
    const fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Word => " ",
            Self::Line => "\r",
            Self::Paragraph => "\r\n",
        }
    }
}

/// One Lyric Data or Ruby Data message: a syllable and how it ends. An empty
/// body with no end is the melisma (sections 7.5.10.1 and 7.5.12.1), an
/// all-zero `Complete` packet: the previous syllable continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Syllable {
    /// The syllable.
    pub body: SyllableBody,
    /// How it ends.
    pub end: SyllableEnd,
}

/// What follows a melisma that ends a word (see [`flex_melisma_end_packets`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WordEnd {
    /// A space: the word ends.
    Word,
    /// A space and a carriage return: the word and the line end.
    Line,
    /// A space, a carriage return and a line feed: the word, the line and the
    /// paragraph end.
    Paragraph,
}

impl WordEnd {
    const fn text(self) -> &'static str {
        match self {
            Self::Word => " ",
            Self::Line => " \r",
            Self::Paragraph => " \r\n",
        }
    }
}

/// The packets of the Project Name `name`, addressed to the Group (Table 16:
/// the address field should be 1).
#[must_use]
pub fn flex_project_name_packets(group: MidiGroup, name: &Text) -> FlexTextPackets<'_> {
    FlexTextPackets::new(
        group,
        FlexAddress::Group,
        FlexTextKind::ProjectName,
        name.as_bytes(),
    )
}

/// The packets of the MIDI Clip Name `name`, addressed to the Group (Table
/// 16: the address field should be 1).
#[must_use]
pub fn flex_clip_name_packets(group: MidiGroup, name: &Text) -> FlexTextPackets<'_> {
    FlexTextPackets::new(
        group,
        FlexAddress::Group,
        FlexTextKind::ClipName,
        name.as_bytes(),
    )
}

/// The packets of the Recording/Concert Date `date` (section 7.5.9.2).
#[must_use]
pub fn flex_recording_date_packets(
    group: MidiGroup,
    address: FlexAddress,
    date: &Iso8601Date,
) -> FlexTextPackets<'_> {
    FlexTextPackets::new(
        group,
        address,
        FlexTextKind::RecordingDate,
        date.0.as_bytes(),
    )
}

/// The packets of `text` of a plain kind.
///
/// A text of up to 12 bytes is one `Complete` packet (an empty one is a
/// `Complete` of zeros). A longer one is a `Start`, any number of `Continue`,
/// then an `End`, each carrying 12 bytes but the last, which carries one to 12
/// and is padded with zeros. Nothing is allocated, and the packets carry no
/// timing.
///
/// ```
/// use tutti_ump::{
///     Flex, FlexAddress, Form, Message, MidiGroup, PlainTextKind, Text, flex_text_packets,
/// };
///
/// let text = Text::try_from("thirteen byte").unwrap();
/// let mut packets =
///     flex_text_packets(MidiGroup::of(0), FlexAddress::Group, PlainTextKind::Copyright, &text);
/// assert_eq!(packets.len(), 2);
/// let Ok(Message::Flex(_, Flex::Text { form, .. })) = packets.next().unwrap().decode() else {
///     unreachable!()
/// };
/// assert_eq!(form, Form::Start);
/// ```
#[must_use]
pub fn flex_text_packets(
    group: MidiGroup,
    address: FlexAddress,
    kind: PlainTextKind,
    text: &Text,
) -> FlexTextPackets<'_> {
    FlexTextPackets::new(group, address, kind.into(), text.as_bytes())
}

/// The packets of one Lyric Data or Ruby Data message (sections 7.5.10 and
/// 7.5.12): the syllable and the space, carriage return or carriage return
/// and line feed that ends it, in one `Complete` packet or a run ending in
/// `End`, the bytes after the end zero.
#[must_use]
pub fn flex_lyric_packets(
    group: MidiGroup,
    address: FlexAddress,
    kind: LyricKind,
    syllable: &Syllable,
) -> LyricPackets {
    let body = syllable.body.0.as_bytes();
    let end = syllable.end.text().as_bytes();
    let mut text = [0; FLEX_TEXT_MAX_LEN];
    text[..body.len()].copy_from_slice(body);
    text[body.len()..body.len() + end.len()].copy_from_slice(end);
    LyricPackets {
        group,
        address,
        kind: kind.into(),
        text,
        len: body.len() + end.len(),
        at: 0,
        started: false,
    }
}

/// A melisma that ends a word: the all-zero `Complete` Lyric or Ruby packet,
/// and at once after it the message holding the space (and carriage return
/// and line feed) that ends the word (M2-104 sections 7.5.10.1 and 7.5.12.1).
///
/// ```
/// use tutti_ump::{FlexAddress, LyricKind, MidiGroup, WordEnd, flex_melisma_end_packets};
///
/// let (group, address) = (MidiGroup::of(0), FlexAddress::Group);
/// let packets: Vec<_> =
///     flex_melisma_end_packets(group, address, LyricKind::Lyrics, WordEnd::Word).collect();
/// assert_eq!(packets.len(), 2);
/// ```
#[must_use]
pub fn flex_melisma_end_packets(
    group: MidiGroup,
    address: FlexAddress,
    kind: LyricKind,
    end: WordEnd,
) -> MelismaEndPackets {
    MelismaEndPackets {
        group,
        address,
        kind: kind.into(),
        melisma: true,
        word_end: text_packets(end.text().as_bytes()),
    }
}

/// The Ump of one packet of a text message.
fn flex_ump(
    group: MidiGroup,
    address: FlexAddress,
    kind: FlexTextKind,
    (form, bytes): (Form, [u8; BYTES_PER_PACKET]),
) -> Ump {
    Ump::from(Message::Flex(
        group,
        Flex::Text {
            address,
            kind,
            form,
            bytes,
        },
    ))
}

/// The packets of one text; see [`flex_text_packets`].
#[derive(Clone, Debug)]
pub struct FlexTextPackets<'a> {
    group: MidiGroup,
    address: FlexAddress,
    kind: FlexTextKind,
    packets: TextPackets<'a, BYTES_PER_PACKET>,
}

impl<'a> FlexTextPackets<'a> {
    fn new(group: MidiGroup, address: FlexAddress, kind: FlexTextKind, bytes: &'a [u8]) -> Self {
        Self {
            group,
            address,
            kind,
            packets: text_packets(bytes),
        }
    }
}

impl Iterator for FlexTextPackets<'_> {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        let packet = self.packets.next()?;
        Some(flex_ump(self.group, self.address, self.kind, packet))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.packets.size_hint()
    }
}

impl ExactSizeIterator for FlexTextPackets<'_> {}
impl FusedIterator for FlexTextPackets<'_> {}

/// The packets of one lyric message; see [`flex_lyric_packets`]. It holds
/// the syllable joined to its end, which the framing of [`text_packets`]
/// reads from.
#[derive(Clone, Debug)]
pub struct LyricPackets {
    group: MidiGroup,
    address: FlexAddress,
    kind: FlexTextKind,
    text: [u8; FLEX_TEXT_MAX_LEN],
    len: usize,
    at: usize,
    started: bool,
}

impl LyricPackets {
    /// The packets still to come.
    fn rest(&self) -> TextPackets<'_, BYTES_PER_PACKET> {
        TextPackets::resume(&self.text[self.at..self.len], self.started)
    }
}

impl Iterator for LyricPackets {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        let packet = self.rest().next()?;
        self.at += (self.len - self.at).min(BYTES_PER_PACKET);
        self.started = true;
        Some(flex_ump(self.group, self.address, self.kind, packet))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rest().size_hint()
    }
}

impl ExactSizeIterator for LyricPackets {}
impl FusedIterator for LyricPackets {}

/// The packets of a melisma that ends a word: see
/// [`flex_melisma_end_packets`].
#[derive(Clone, Debug)]
pub struct MelismaEndPackets {
    group: MidiGroup,
    address: FlexAddress,
    kind: FlexTextKind,
    /// Whether the melisma, the all-zero `Complete` packet, is still to come.
    melisma: bool,
    word_end: TextPackets<'static, BYTES_PER_PACKET>,
}

impl Iterator for MelismaEndPackets {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        let packet = if self.melisma {
            self.melisma = false;
            (Form::Complete, [0; BYTES_PER_PACKET])
        } else {
            self.word_end.next()?
        };
        Some(flex_ump(self.group, self.address, self.kind, packet))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = usize::from(self.melisma) + self.word_end.len();
        (n, Some(n))
    }
}

impl ExactSizeIterator for MelismaEndPackets {}
impl FusedIterator for MelismaEndPackets {}
