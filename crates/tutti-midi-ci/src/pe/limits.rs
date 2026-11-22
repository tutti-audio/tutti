//! The text types of a Header Data property, each holding the limit M2-103 sets
//! on it (§6.1, §7.1.1 to §7.5, §8.4, §8.5, §11.1).

use std::fmt;

use tutti_units::{Text, TextError};

use super::write::escaped_len;

/// Why a text is not a Property Exchange text type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum PeTextError {
    /// The text is empty where the type needs at least one character.
    #[error("the text is empty")]
    Empty,
    /// The text is longer than `max` bytes (for a [`PeMessage`], bytes once escaped).
    #[error("the text is longer than {max} bytes")]
    TooLong {
        /// The most bytes the type holds.
        max: usize,
    },
    /// The text has more than `max` characters (a [`ModeId`](super::ModeId) or
    /// [`RoleName`](super::RoleName), whose limit counts characters).
    #[error("the text has more than {max} characters")]
    TooManyChars {
        /// The most characters the type holds.
        max: usize,
    },
    /// The text holds a character outside what the type allows.
    #[error("the text holds a character outside {allowed}")]
    Character {
        /// The characters the type allows.
        allowed: &'static str,
    },
}

impl From<TextError> for PeTextError {
    fn from(error: TextError) -> Self {
        match error {
            TextError::TooShort { .. } => Self::Empty,
            TextError::TooLong { max } => Self::TooLong { max },
            TextError::Nul => Self::Character {
                allowed: "text without a NUL",
            },
            TextError::Bom => Self::Character {
                allowed: "text without a leading byte order mark",
            },
        }
    }
}

/// Why a text is not a [`PropertyName`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum PropertyNameError {
    /// The text breaks a limit every Property Exchange text has.
    #[error(transparent)]
    Text(#[from] PeTextError),
    /// The name is all digits (§6.1), after an `x-` prefix if it has one.
    #[error("the name is all digits")]
    AllDigits,
}

impl From<TextError> for PropertyNameError {
    fn from(error: TextError) -> Self {
        Self::Text(error.into())
    }
}

/// How a text type holds its characters, so one macro defines them all.
pub(crate) trait Held: Sized + AsRef<str> {
    /// The text held, or why its length or bytes are refused.
    fn hold(text: &str) -> Result<Self, TextError>;
}

impl<const MIN: usize, const MAX: usize> Held for Text<MIN, MAX> {
    fn hold(text: &str) -> Result<Self, TextError> {
        Self::try_from(text)
    }
}

impl Held for String {
    fn hold(text: &str) -> Result<Self, TextError> {
        Ok(text.to_owned())
    }
}

/// Defines a text type: `$check` refuses what the alphabet or form does not
/// allow, then `$store` (a [`Text`] for a bound in bytes, a `String` otherwise)
/// refuses a length. A check runs first, so a character the type does not allow
/// is named before a length.
macro_rules! text_type {
    ($(#[$meta:meta])* $name:ident, $store:ty, $err:ty, $check:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        #[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
        pub struct $name($store);

        impl $name {
            /// The value, or `None` when `text` breaks the limit. [`TryFrom`]
            /// says which.
            #[must_use]
            pub fn new(text: &str) -> Option<Self> {
                Self::try_from(text).ok()
            }

            /// The text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                ::std::convert::AsRef::as_ref(&self.0)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = $err;

            fn try_from(text: &str) -> Result<Self, $err> {
                let check: fn(&str) -> Result<(), $err> = $check;
                check(text)?;
                <$store as $crate::pe::limits::Held>::hold(text)
                    .map(Self)
                    .map_err(<$err>::from)
            }
        }

        impl TryFrom<String> for $name {
            type Error = $err;

            fn try_from(text: String) -> Result<Self, $err> {
                Self::try_from(text.as_str())
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.as_str().to_owned()
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

pub(super) use text_type;

/// A non-empty run of ASCII letters and digits.
fn alphanumeric(body: &str) -> Result<(), PeTextError> {
    if body.is_empty() {
        return Err(PeTextError::Empty);
    }
    if !body.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(PeTextError::Character {
            allowed: "ASCII letters and digits",
        });
    }
    Ok(())
}

text_type! {
    /// The name of a Resource: at most 36 ASCII alphanumeric characters (M2-103
    /// Table 13), or the `X-` of a manufacturer-specific Resource (§8.4) and
    /// alphanumeric characters after it. The 36 is the length of the whole `resource`
    /// value, so a custom name has 1 to 34 characters after its `X-`. The prefix is
    /// the one hyphen the name may hold: Table 13 lists alphanumerics only, and §8.4
    /// requires a custom Resource to start `X-`, so the prefix is the reading that
    /// holds both.
    ResourceName, Text<1, 36>, PeTextError,
    |t| alphanumeric(t.strip_prefix("X-").unwrap_or(t))
}

text_type! {
    /// A Resource ID (§8.5): 1 to 36 ASCII alphanumeric or `_` characters
    /// (Table 13).
    ResourceId, Text<1, 36>, PeTextError,
    |t| alphabet(t, |b| b.is_ascii_alphanumeric() || b == b'_', "ASCII letters, digits and `_`")
}

text_type! {
    /// A Subscription Id (§11.1, Table 39): 1 to 8 characters of `a-z`, `0-9` and `_`.
    SubscribeId, Text<1, 8>, PeTextError,
    |t| alphabet(t, |b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'), "`a-z`, `0-9` and `_`")
}

/// `text` made only of the bytes `allowed` accepts, which `named` describes in
/// the error.
pub(super) fn alphabet(
    text: &str,
    allowed: fn(u8) -> bool,
    named: &'static str,
) -> Result<(), PeTextError> {
    if text.bytes().all(allowed) {
        Ok(())
    } else {
        Err(PeTextError::Character { allowed: named })
    }
}

text_type! {
    /// The name of a header property: 1 to 20 ASCII alphanumeric characters, not
    /// all digits (§6.1, §7.1.1), or the `x-` prefix of a manufacturer-specific
    /// property (§6.1.4) and alphanumerics.
    PropertyName, Text<1, 20>, PropertyNameError,
    |t| {
        let body = t.strip_prefix("x-").unwrap_or(t);
        alphanumeric(body)?;
        if body.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PropertyNameError::AllDigits);
        }
        Ok(())
    }
}

text_type! {
    /// A Media Type, as `mediaType` carries it (§7.5, Table 16): 1 to 75 printable
    /// ASCII characters.
    MediaType, Text<1, 75>, PeTextError,
    |t| alphabet(t, |b| matches!(b, b' '..=b'~'), "printable ASCII")
}

const fn media_type_valid(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > 75 {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        if !matches!(bytes[i], b' '..=b'~') {
            return false;
        }
        i += 1;
    }
    true
}

impl MediaType {
    /// The media type of a literal, checked when it is a constant.
    ///
    /// # Panics
    ///
    /// When `text` is not 1 to 75 printable ASCII characters; in a `const` that
    /// is a compile error.
    #[must_use]
    pub const fn of(text: &'static str) -> Self {
        assert!(media_type_valid(text), "not a media type");
        Self(Text::of(text))
    }

    /// `application/json`, the media type of Property Data unless a Resource
    /// lists others (M2-103 §8.3).
    pub const JSON: Self = Self::of("application/json");

    /// `application/octet-stream`, the media type of Property Data no format
    /// names.
    pub const OCTET_STREAM: Self = Self::of("application/octet-stream");
}

/// The explanatory text of a Reply (§7.3, Table 14), held as the text it says:
/// at most 512 bytes once escaped, which is how it travels.
///
/// Owned: the bound is on the escaped length, which is not the bytes held.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeMessage(String);

impl PeMessage {
    /// The most escaped bytes a message may take, 512.
    pub const MAX_ESCAPED_LEN: usize = 512;

    /// The message, or `None` when its escaped form is longer than
    /// [`MAX_ESCAPED_LEN`](Self::MAX_ESCAPED_LEN) bytes.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::try_from(text).ok()
    }

    /// The text, unescaped.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for PeMessage {
    type Error = PeTextError;

    fn try_from(text: &str) -> Result<Self, PeTextError> {
        if escaped_len(text) > Self::MAX_ESCAPED_LEN {
            return Err(PeTextError::TooLong {
                max: Self::MAX_ESCAPED_LEN,
            });
        }
        Ok(Self(text.to_owned()))
    }
}

impl From<PeMessage> for String {
    fn from(message: PeMessage) -> String {
        message.0
    }
}

impl fmt::Display for PeMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModeId, PeText, RoleName, StateId};

    // `tutti_fixtures::check_range_contract` does not fit these types: it takes a
    // `Copy` value read back by `Into<raw>`, and these are built by a `Result`
    // that says why and leave as `as_str`. `new` is `try_from(..).ok()` by
    // definition, so no test compares the two.

    fn too_long(max: usize) -> Result<(), PeTextError> {
        Err(PeTextError::TooLong { max })
    }

    /// A type of 1 to `max` bytes of `a`: exactly `max` is held, one more is
    /// too long, nothing is empty, and a two-byte character across `max` is
    /// refused for its character (these alphabets are ASCII), never split.
    fn bound<T>(max: usize)
    where
        for<'a> T: TryFrom<&'a str, Error = PeTextError> + fmt::Debug,
    {
        assert!(T::try_from("a".repeat(max).as_str()).is_ok());
        assert_eq!(
            T::try_from("a".repeat(max + 1).as_str()).map(drop),
            too_long(max)
        );
        assert_eq!(T::try_from("").map(drop), Err(PeTextError::Empty));
        let straddling = format!("{}é", "a".repeat(max - 1));
        assert_eq!(straddling.len(), max + 1);
        assert!(matches!(
            T::try_from(straddling.as_str()),
            Err(PeTextError::Character { .. })
        ));
    }

    /// M2-103 Table 13, Table 39, Table 16 and M2-111 §2.3: each type's limit
    /// in bytes.
    #[test]
    fn a_byte_bounded_text_holds_exactly_its_limit() {
        bound::<ResourceName>(36);
        bound::<ResourceId>(36);
        bound::<SubscribeId>(8);
        bound::<MediaType>(75);
        bound::<StateId>(36);
    }

    /// M2-103 §8.4: the `X-` of a custom Resource counts in the 36, and a name
    /// of the prefix alone has no body.
    #[test]
    fn a_resource_name_refuses_by_its_reason() {
        assert!(ResourceName::new(&format!("X-{}", "a".repeat(34))).is_some());
        assert_eq!(
            ResourceName::try_from(format!("X-{}", "a".repeat(35))).map(drop),
            too_long(36)
        );
        assert_eq!(
            ResourceName::try_from("X-").map(drop),
            Err(PeTextError::Empty)
        );
        for bad in [
            "Channel List",
            "Channel_List",
            "Cha-nnel",
            "x-Custom",
            "Channel\u{e9}",
        ] {
            assert_eq!(
                ResourceName::try_from(bad).map(drop),
                Err(PeTextError::Character {
                    allowed: "ASCII letters and digits"
                }),
                "{bad}"
            );
        }
    }

    /// M2-103 Table 13 and Table 39: the alphabets of an id.
    #[test]
    fn an_id_refuses_a_character_outside_its_alphabet() {
        for bad in ["prog-1", "prog 1", "prog\u{e9}"] {
            assert!(matches!(
                ResourceId::try_from(bad),
                Err(PeTextError::Character { .. })
            ));
            assert!(matches!(
                StateId::try_from(bad),
                Err(PeTextError::Character { .. })
            ));
        }
        assert!(ResourceId::new("prog_1").is_some());
        for bad in ["Sub328", "sub-1"] {
            assert!(matches!(
                SubscribeId::try_from(bad),
                Err(PeTextError::Character { .. })
            ));
        }
        assert!(SubscribeId::new("sub_328").is_some());
        // Printable ASCII is 32 to 126 only.
        for bad in ["\u{1f}", "\u{7f}", "a\0"] {
            assert!(matches!(
                MediaType::try_from(bad),
                Err(PeTextError::Character { .. })
            ));
        }
        assert!(MediaType::new(" ~").is_some());
    }

    /// M2-103 §6.1, §6.1.4: 1 to 20 bytes with the `x-` prefix among them, and
    /// not all digits.
    #[test]
    fn a_property_name_refuses_by_its_reason() {
        let text = |e| PropertyNameError::Text(e);
        assert!(PropertyName::new(&"a".repeat(20)).is_some());
        assert_eq!(
            PropertyName::try_from("a".repeat(21)).map(drop),
            Err(text(PeTextError::TooLong { max: 20 }))
        );
        assert!(PropertyName::new(&format!("x-{}", "a".repeat(18))).is_some());
        assert_eq!(
            PropertyName::try_from(format!("x-{}", "a".repeat(19))).map(drop),
            Err(text(PeTextError::TooLong { max: 20 }))
        );
        let straddling = format!("{}é", "a".repeat(19));
        assert!(matches!(
            PropertyName::try_from(straddling),
            Err(PropertyNameError::Text(PeTextError::Character { .. }))
        ));
        assert_eq!(
            PropertyName::try_from("").map(drop),
            Err(text(PeTextError::Empty))
        );
        assert_eq!(
            PropertyName::try_from("x-").map(drop),
            Err(text(PeTextError::Empty))
        );
        assert_eq!(
            PropertyName::try_from("120").map(drop),
            Err(PropertyNameError::AllDigits)
        );
        assert_eq!(
            PropertyName::try_from("x-12").map(drop),
            Err(PropertyNameError::AllDigits)
        );
        assert!(PropertyName::new("a120").is_some());
        for bad in ["my_key", "my key", "X-key"] {
            assert!(matches!(
                PropertyName::try_from(bad),
                Err(PropertyNameError::Text(PeTextError::Character { .. }))
            ));
        }
    }

    /// M2-103 Table 14: 512 bytes after escaping. A character that escapes to
    /// six bytes across the bound is refused whole.
    #[test]
    fn a_message_is_bounded_by_its_escaped_bytes() {
        let ok = |t: &str| PeMessage::try_from(t).map(drop);
        assert_eq!(ok(&"a".repeat(512)), Ok(()));
        assert_eq!(ok(&"a".repeat(513)), too_long(512));
        // A quote escapes to two bytes, a BMP character to six, one outside the
        // BMP to twelve.
        assert_eq!(ok(&"\"".repeat(256)), Ok(()));
        assert_eq!(ok(&"\"".repeat(257)), too_long(512));
        assert_eq!(ok(&"\u{1f3b9}".repeat(42)), Ok(()));
        assert_eq!(ok(&"\u{1f3b9}".repeat(43)), too_long(512));
        // 85 notes are 510 escaped bytes: two more bytes fit, a third does not.
        let notes = "\u{266a}".repeat(85);
        assert_eq!(ok(&format!("{notes}aa")), Ok(()));
        assert_eq!(ok(&format!("{notes}aaa")), too_long(512));
        assert_eq!(ok(&format!("{notes}\u{266a}")), too_long(512));
    }

    /// M2-106 §2.2 and M2-103 Table 64: the limit counts characters, so a
    /// multibyte character is one and the bytes held can pass the count.
    #[test]
    fn a_character_bounded_text_counts_characters() {
        fn chars<T>(max: usize)
        where
            for<'a> T: TryFrom<&'a str, Error = PeTextError> + fmt::Debug,
        {
            assert!(T::try_from("é".repeat(max).as_str()).is_ok());
            assert_eq!(
                T::try_from("é".repeat(max + 1).as_str()).map(drop),
                Err(PeTextError::TooManyChars { max })
            );
            assert_eq!(T::try_from("").map(drop), Err(PeTextError::Empty));
        }
        chars::<ModeId>(36);
        chars::<RoleName>(32);
    }

    /// M2-103 §8.3: a required string is not empty, and has no bound.
    #[test]
    fn a_required_text_is_refused_only_when_empty() {
        assert_eq!(PeText::try_from("").map(drop), Err(PeTextError::Empty));
        assert_eq!(
            PeText::new(&"é".repeat(10_000)).unwrap().as_str().len(),
            20_000
        );
    }

    /// A NUL or a length the inline text refuses reads as the type's own refusal.
    #[test]
    fn a_text_refusal_reads_as_a_pe_refusal() {
        assert_eq!(
            PeTextError::from(TextError::TooShort { min: 1 }),
            PeTextError::Empty
        );
        assert_eq!(
            PeTextError::from(TextError::TooLong { max: 9 }),
            PeTextError::TooLong { max: 9 }
        );
        assert!(matches!(
            PeTextError::from(TextError::Nul),
            PeTextError::Character { .. }
        ));
        assert!(matches!(
            PeTextError::from(TextError::Bom),
            PeTextError::Character { .. }
        ));
    }

    /// An ordinary mixed-case suffix is accepted by the types that take one.
    #[test]
    fn a_mixed_case_name_is_accepted() {
        assert_eq!(
            PropertyName::new("x-uniqueKey").unwrap().as_str(),
            "x-uniqueKey"
        );
        assert_eq!(
            ResourceName::new("X-MyResource").unwrap().as_str(),
            "X-MyResource"
        );
        assert_eq!(SubscribeId::new("abcdefgh").unwrap().as_str(), "abcdefgh");
    }

    /// M2-103 Table 16: the literal media types are valid media types, and the
    /// one a Resource lists by default is JSON.
    #[test]
    fn the_named_media_types_are_the_ones_new_accepts() {
        assert_eq!(MediaType::JSON, MediaType::new("application/json").unwrap());
        assert_eq!(
            MediaType::OCTET_STREAM,
            MediaType::new("application/octet-stream").unwrap()
        );
    }

    #[cfg(feature = "serde")]
    mod serde_text {
        use super::*;

        /// A type serialises as its bare text and reads back; the text it
        /// refuses is refused at decode.
        fn round_trip<T>(valid: &str, invalid: &str)
        where
            T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + fmt::Debug,
        {
            let json = serde_json::to_string(valid).unwrap();
            let value: T = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&value).unwrap(), json);
            assert_eq!(value, serde_json::from_str(&json).unwrap());
            assert!(serde_json::from_str::<T>(&serde_json::to_string(invalid).unwrap()).is_err());
        }

        #[test]
        fn the_text_types_round_trip_as_text_and_refuse_a_bad_decode() {
            round_trip::<ResourceName>("X-Custom", "bad name");
            round_trip::<ResourceId>("prog_1", "prog-1");
            round_trip::<SubscribeId>("sub_1", "Sub1");
            round_trip::<PropertyName>("x-key", "12");
            round_trip::<MediaType>("audio/midi", "caf\u{e9}");
            round_trip::<StateId>("fullState", "full state");
            round_trip::<ModeId>("mode é", "");
            round_trip::<RoleName>("role", "");
            round_trip::<PeText>("title", "");
        }
    }
}
