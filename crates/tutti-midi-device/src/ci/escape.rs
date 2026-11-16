//! The 7-bit string codec two dialects share: M2-101 §5.10.4 (Message Text) and
//! M2-103 §6.1.1 (JSON string values) give the same rule for a character that
//! is not 7-bit ASCII: it becomes UTF-16 and each unit a `\uXXXX` escape, as in
//! JSON. What differs is the rest of each dialect, which its own module holds:
//! Message Text is not a JSON document (a quote is plain text, a line feed is
//! allowed and every other control character is ignored), where a Property
//! Exchange string value is one (a quote, a backslash and every control
//! character are escaped).

use std::fmt::Write as _;

/// Whether `c` is 7-bit ASCII from space to DEL, which both dialects send as it
/// is (a dialect escapes some of these itself, a quote or a backslash).
pub(crate) fn is_plain(c: char) -> bool {
    matches!(c, ' '..='\u{7f}')
}

/// Appends `c` as one `\uXXXX` per UTF-16 unit: a character past the Basic
/// Multilingual Plane is a surrogate pair.
pub(crate) fn push_utf16_escapes(out: &mut String, c: char) {
    let mut units = [0u16; 2];
    for unit in c.encode_utf16(&mut units) {
        // Writing to a `String` cannot fail.
        let _ = write!(out, "\\u{unit:04x}");
    }
}

/// How many bytes [`push_utf16_escapes`] writes for `c`.
pub(crate) fn utf16_escapes_len(c: char) -> usize {
    6 * c.len_utf16()
}

/// Four ASCII hex digits, either case, as the UTF-16 unit they spell.
pub(crate) fn hex4(digits: &[u8]) -> Option<u16> {
    if digits.len() != 4 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u16::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escaped(c: char) -> String {
        let mut out = String::new();
        push_utf16_escapes(&mut out, c);
        out
    }

    // M2-101 §5.10.4 and M2-103 §6.1.1 examples: the note sign is `♪`.
    /// Spec: m2-101/5.10.4#4, m2-103/4.1.1#2
    #[test]
    fn a_character_outside_ascii_is_one_u_escape_of_its_utf16_unit() {
        assert_eq!(escaped('\u{266a}'), "\\u266a");
        assert_eq!(escaped('\u{6b8b}'), "\\u6b8b");
    }

    // Both specs say "converted to UTF-16, then escaped as JSON": a character
    // past the Basic Multilingual Plane is a surrogate pair.
    /// Spec: m2-101/5.10.4#4, m2-103/4.1.1#2
    #[test]
    fn a_character_past_the_bmp_is_a_surrogate_pair_of_two_escapes() {
        assert_eq!(escaped('\u{1d11e}'), "\\ud834\\udd1e");
        assert_eq!(utf16_escapes_len('\u{1d11e}'), 12);
        assert_eq!(utf16_escapes_len('\u{266a}'), 6);
    }

    // The specs write the escapes in both cases (`残`, `ノ`).
    #[test]
    fn four_hex_digits_of_either_case_are_a_unit_and_anything_else_is_not() {
        assert_eq!(hex4(b"6B8b"), Some(0x6b8b));
        assert_eq!(hex4(b"266a"), Some(0x266a));
        assert_eq!(hex4(b"26g6"), None);
        assert_eq!(hex4(b"266"), None);
    }

    #[test]
    fn plain_is_space_to_del() {
        assert!(is_plain(' ') && is_plain('~') && is_plain('\u{7f}'));
        assert!(!is_plain('\n') && !is_plain('\u{80}'));
    }
}
