//! [`Text`]: a UTF-8 string of a bounded byte length, held inline.

use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};

/// Why a string is not a [`Text`]. Lengths are in bytes, not characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum TextError {
    /// The string is shorter than `min` bytes.
    #[error("the text is shorter than {min} bytes")]
    TooShort {
        /// The fewest bytes the type holds.
        min: usize,
    },
    /// The string is longer than `max` bytes.
    #[error("the text is longer than {max} bytes")]
    TooLong {
        /// The most bytes the type holds.
        max: usize,
    },
    /// The string holds a NUL, which ends a string on a wire that pads with it.
    #[error("the text holds a NUL")]
    Nul,
    /// The string starts with U+FEFF, a byte order mark, which UTF-8 text on a
    /// wire does not carry. U+FEFF later in a string is a zero-width no-break
    /// space and is kept.
    #[error("the text starts with a byte order mark")]
    Bom,
}

/// A UTF-8 string of `MIN` to `MAX` bytes with no NUL and no leading byte order
/// mark (U+FEFF), held inline.
///
/// The bounds are in **bytes**: a multi-byte character counts all its bytes,
/// and one that would straddle `MAX` is refused, never split. Construction
/// never truncates. The storage is an inline `[u8; MAX]` and a `u16` length, so
/// the type needs no allocator, and a `MAX` past `u16::MAX` or a `MIN` above
/// `MAX` fails to compile.
///
/// ```
/// use tutti_units::{Text, TextError};
///
/// type Name = Text<1, 8>;
///
/// assert_eq!(Name::new("Rosa").unwrap().as_str(), "Rosa");
/// assert_eq!(Name::try_from(""), Err(TextError::TooShort { min: 1 }));
/// assert_eq!(Name::try_from("123456789"), Err(TextError::TooLong { max: 8 }));
/// // "é" is two bytes, so four of them are eight and five are ten.
/// assert!(Name::new("éééé").is_some());
/// assert!(Name::new("ééééé").is_none());
/// // A leading byte order mark is refused; U+FEFF inside the text is kept.
/// assert_eq!(Name::try_from("\u{FEFF}a"), Err(TextError::Bom));
/// assert!(Name::new("a\u{FEFF}b").is_some());
/// ```
///
/// A bound that cannot hold is a compile error:
///
/// ```compile_fail
/// let _ = tutti_units::Text::<5, 2>::new("");
/// ```
///
/// ```compile_fail
/// let _ = tutti_units::Text::<0, 70_000>::new("");
/// ```
///
/// It is `Copy`, orders and hashes as its string. It has no `From<&str>` (a conversion
/// can fail) and no `Deref`: [`as_str`](Self::as_str) is the way out.
///
/// With the `serde` feature it serialises as a string and deserialises through
/// [`TryFrom`], so a decoded string out of range is refused.
#[derive(Clone, Copy)]
pub struct Text<const MIN: usize, const MAX: usize> {
    bytes: [u8; MAX],
    len: u16,
}

impl<const MIN: usize, const MAX: usize> Text<MIN, MAX> {
    /// The bounds are valid: `MAX` fits the `u16` length and `MIN <= MAX`.
    const BOUNDS: () = {
        assert!(MAX <= u16::MAX as usize, "Text: MAX must fit a u16");
        assert!(MIN <= MAX, "Text: MIN must not exceed MAX");
    };

    /// The text `text`, or `None` for a NUL, a leading U+FEFF or a length
    /// outside `MIN..=MAX` bytes; never truncates. [`TryFrom`] says why.
    #[must_use]
    pub const fn new(text: &str) -> Option<Self> {
        match Self::check(text) {
            Ok(text) => Some(text),
            Err(_) => None,
        }
    }

    /// The text `text`, or why it is not one: [`TextError::Nul`] for a NUL,
    /// [`TextError::Bom`] for a leading U+FEFF,
    /// [`TextError::TooShort`] under `MIN` bytes, [`TextError::TooLong`] over
    /// `MAX` bytes.
    const fn check(text: &str) -> Result<Self, TextError> {
        let () = Self::BOUNDS;
        let src = text.as_bytes();
        let len = src.len();
        let mut i = 0;
        while i < len {
            if src[i] == 0 {
                return Err(TextError::Nul);
            }
            i += 1;
        }
        // U+FEFF is `EF BB BF` in UTF-8.
        if len >= 3 && src[0] == 0xEF && src[1] == 0xBB && src[2] == 0xBF {
            return Err(TextError::Bom);
        }
        if len < MIN {
            return Err(TextError::TooShort { min: MIN });
        }
        if len > MAX {
            return Err(TextError::TooLong { max: MAX });
        }
        let mut bytes = [0; MAX];
        let mut i = 0;
        while i < len {
            bytes[i] = src[i];
            i += 1;
        }
        #[expect(clippy::cast_possible_truncation, reason = "len <= MAX <= u16::MAX")]
        Ok(Self {
            bytes,
            len: len as u16,
        })
    }

    /// The text of a literal.
    ///
    /// # Panics
    ///
    /// Panics (at compile time in a `const`) when `text` is not a `Text`.
    #[must_use]
    pub const fn of(text: &'static str) -> Self {
        match Self::new(text) {
            Some(text) => text,
            None => panic!("the literal is not a Text of this size"),
        }
    }

    /// The text.
    #[must_use]
    pub const fn as_str(&self) -> &str {
        // `new` is the only constructor and it stores whole `&str` bytes, so
        // this is valid UTF-8; the fallback is unreachable.
        match core::str::from_utf8(self.bytes.split_at(self.len as usize).0) {
            Ok(text) => text,
            Err(_) => "",
        }
    }
}

impl<const MAX: usize> Default for Text<0, MAX> {
    /// The empty text.
    fn default() -> Self {
        let () = Self::BOUNDS;
        Self {
            bytes: [0; MAX],
            len: 0,
        }
    }
}

impl<const MIN: usize, const MAX: usize> TryFrom<&str> for Text<MIN, MAX> {
    type Error = TextError;

    /// Agrees with [`Text::new`], and says why a string is refused.
    fn try_from(text: &str) -> Result<Self, TextError> {
        Self::check(text)
    }
}

impl<const MIN: usize, const MAX: usize> AsRef<str> for Text<MIN, MAX> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<const MIN: usize, const MAX: usize> fmt::Debug for Text<MIN, MAX> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const MIN: usize, const MAX: usize> fmt::Display for Text<MIN, MAX> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const MIN: usize, const MAX: usize> PartialEq for Text<MIN, MAX> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<const MIN: usize, const MAX: usize> Eq for Text<MIN, MAX> {}

impl<const MIN: usize, const MAX: usize> PartialOrd for Text<MIN, MAX> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<const MIN: usize, const MAX: usize> Ord for Text<MIN, MAX> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl<const MIN: usize, const MAX: usize> Hash for Text<MIN, MAX> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

#[cfg(feature = "serde")]
impl<const MIN: usize, const MAX: usize> serde::Serialize for Text<MIN, MAX> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl<'de, const MIN: usize, const MAX: usize> serde::Deserialize<'de> for Text<MIN, MAX> {
    /// Reads a string through [`Text::new`], with a visitor that never
    /// allocates, so the no-alloc build reads it too.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<const MIN: usize, const MAX: usize>;

        impl<const MIN: usize, const MAX: usize> serde::de::Visitor<'_> for Visitor<MIN, MAX> {
            type Value = Text<MIN, MAX>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    f,
                    "a string of {MIN} to {MAX} bytes with no NUL or leading byte order mark"
                )
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
                Text::try_from(text).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `tutti_fixtures::check_range_contract` does not fit a string type: it
    // reads a value back through `Into<raw>`, and a `Text` leaves as
    // `as_str`. The cases are pinned in the tests below.
    type Name = Text<1, 8>;

    #[test]
    fn new_and_try_from_agree_on_every_case() {
        let cases = [
            "",
            "a",
            "12345678",
            "123456789",
            "123456é",
            "1234567é",
            "a\0b",
            "\0",
        ];
        for case in cases {
            assert_eq!(Name::new(case), Name::try_from(case).ok(), "{case:?}");
            assert_eq!(Name::new(case).is_none(), Name::try_from(case).is_err());
        }
        assert_eq!(Name::try_from("a\0b"), Err(TextError::Nul));
    }

    #[test]
    fn a_leading_byte_order_mark_is_refused_and_one_inside_is_kept() {
        assert_eq!(Name::try_from("\u{FEFF}"), Err(TextError::Bom));
        assert_eq!(Name::try_from("\u{FEFF}ab"), Err(TextError::Bom));
        assert_eq!(Name::new("\u{FEFF}ab"), None);
        assert_eq!(Name::new("a\u{FEFF}b").unwrap().as_str(), "a\u{FEFF}b");
        assert_eq!(Name::new("ab\u{FEFF}").unwrap().as_str(), "ab\u{FEFF}");
        // Another character that starts with 0xEF is not a BOM.
        assert!(Name::new("\u{FF21}").is_some());
        assert!(Text::<0, 8>::new("\u{FEFF}").is_none());
    }

    #[test]
    fn empty_is_refused_under_min_and_kept_at_zero_min() {
        assert_eq!(Name::try_from(""), Err(TextError::TooShort { min: 1 }));
        assert_eq!(Text::<0, 8>::new("").unwrap().as_str(), "");
        assert_eq!(Text::<0, 8>::default().as_str(), "");
    }

    #[test]
    fn exactly_max_bytes_fits_and_one_more_is_too_long() {
        assert_eq!(Name::new("12345678").unwrap().as_str(), "12345678");
        assert_eq!(
            Name::try_from("123456789"),
            Err(TextError::TooLong { max: 8 })
        );
    }

    /// Lengths are bytes: a character that would straddle `MAX` is refused
    /// whole, never cut.
    #[test]
    fn a_multibyte_char_straddling_max_is_refused_not_split() {
        // Seven ASCII bytes plus "é" (two bytes) is nine: the "é" straddles 8.
        assert_eq!(
            Name::try_from("1234567é"),
            Err(TextError::TooLong { max: 8 })
        );
        assert_eq!(Name::new("123456é").unwrap().as_str(), "123456é");
    }

    #[test]
    fn an_interior_nul_is_refused() {
        assert_eq!(Name::try_from("a\0b"), Err(TextError::Nul));
        assert_eq!(Name::try_from("\0"), Err(TextError::Nul));
    }

    #[test]
    fn it_orders_hashes_and_compares_as_its_string() {
        use core::hash::BuildHasher;
        let a = Name::of("abc");
        let b = Name::of("abd");
        assert!(a < b);
        assert_eq!(a, Name::new("abc").unwrap());
        assert_ne!(a, b);
        // A shorter string sorts before a longer one it prefixes, as `str` does.
        assert!(Name::of("ab") < Name::of("abc"));
        let hasher = std::collections::hash_map::RandomState::new();
        assert_eq!(hasher.hash_one(a), hasher.hash_one("abc"));
    }

    #[test]
    fn display_and_debug_show_the_string() {
        assert_eq!(std::format!("{}", Name::of("ab")), "ab");
        assert_eq!(std::format!("{:?}", Name::of("ab")), "\"ab\"");
        assert_eq!(Name::of("ab").as_ref(), "ab");
    }

    #[test]
    #[should_panic(expected = "not a Text")]
    fn of_panics_for_a_literal_out_of_range() {
        let _ = Name::of("123456789");
    }

    #[test]
    fn error_text_names_the_bound_in_bytes() {
        assert_eq!(
            TextError::TooLong { max: 8 }.to_string(),
            "the text is longer than 8 bytes"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_round_trips_and_refuses_an_out_of_range_decode() {
        let name = Name::of("Rosa");
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"Rosa\"");
        assert_eq!(serde_json::from_str::<Name>(&json).unwrap(), name);
        assert!(serde_json::from_str::<Name>("\"\"").is_err());
        assert!(serde_json::from_str::<Name>("\"123456789\"").is_err());
        assert!(serde_json::from_str::<Name>("\"a\\u0000b\"").is_err());
        assert!(serde_json::from_str::<Name>("\"\\ufeffa\"").is_err());
        assert!(serde_json::from_str::<Name>("5").is_err());
    }
}
