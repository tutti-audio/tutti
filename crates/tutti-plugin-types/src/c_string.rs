//! The one reader and one writer of the fixed-size text buffers a plugin ABI passes.
//!
//! VST2, VST3 and the other C ABIs hand a host a buffer of a fixed size to read a string
//! from or to write one into. A reader takes the bytes up to the first NUL, or the whole
//! buffer when it has none, and decodes them lossily: a plugin's text is not guaranteed to
//! be valid. A writer cuts at a character boundary (a UTF-8 sequence, a UTF-16 surrogate
//! pair is never split), writes a NUL, and writes nothing past it.
//!
//! Every function takes a slice, so the only `unsafe` is the caller's, where a raw pointer
//! is first turned into one.

use std::ffi::c_char;

/// `bytes` up to its first NUL (or its end), lossily decoded as UTF-8.
///
/// The bytes after the first NUL are never decoded.
#[must_use]
pub fn read_c_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// A C `char` field (`i8` or `u8`, whichever the target's `c_char` is) up to its first NUL (or
/// its end), lossily decoded as UTF-8, scanned once.
#[must_use]
pub fn read_c_chars(field: &[c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c.to_ne_bytes()[0])
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `units` up to its first NUL (or its end), lossily decoded as UTF-16.
///
/// The units after the first NUL are never decoded.
#[must_use]
pub fn read_utf16_str(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// The longest prefix of `src` that fits `capacity` bytes with its NUL.
///
/// `src` is cut at its first NUL (a C string holds none), then to at most `capacity - 1`
/// bytes at a character boundary. A `capacity` of `0` holds no NUL either, and gives `""`.
#[must_use]
pub fn c_str_prefix(src: &str, capacity: usize) -> &str {
    let src = src.split('\0').next().unwrap_or("");
    let mut len = src.len().min(capacity.saturating_sub(1));
    while !src.is_char_boundary(len) {
        len -= 1;
    }
    &src[..len]
}

/// Writes `src` into `dst` as a NUL-terminated UTF-8 string and returns the number of text
/// bytes written (the NUL not counted).
///
/// What is written is [`c_str_prefix`]`(src, dst.len())` and one NUL; the rest of `dst` is
/// untouched. An empty `dst` is not written and returns `0`.
pub fn write_c_str(dst: &mut [u8], src: &str) -> usize {
    if dst.is_empty() {
        return 0;
    }
    let text = c_str_prefix(src, dst.len()).as_bytes();
    dst[..text.len()].copy_from_slice(text);
    dst[text.len()] = 0;
    text.len()
}

/// [`write_c_str`] into a C `char` buffer (`i8` or `u8`, whichever the target's `c_char` is).
pub fn write_c_chars(dst: &mut [c_char], src: &str) -> usize {
    if dst.is_empty() {
        return 0;
    }
    let text = c_str_prefix(src, dst.len()).as_bytes();
    for (slot, &byte) in dst.iter_mut().zip(text) {
        *slot = c_char::from_ne_bytes([byte]);
    }
    dst[text.len()] = 0;
    text.len()
}

/// Writes the UTF-16 text `src` into `dst` as a NUL-terminated string and returns the number of
/// text units written (the NUL not counted).
///
/// `src` is cut at its first NUL, then to at most `dst.len() - 1` units, without splitting a
/// surrogate pair: a high surrogate left last by the cut is dropped with its pair. Exactly
/// the text and one NUL are written. An empty `dst` is not written and returns `0`.
pub fn write_utf16_units(dst: &mut [u16], src: &[u16]) -> usize {
    let Some(room) = dst.len().checked_sub(1) else {
        return 0;
    };
    let text = &src[..src.iter().position(|&u| u == 0).unwrap_or(src.len())];
    let mut len = text.len().min(room);
    if len < text.len() && len > 0 && (0xD800..0xDC00).contains(&text[len - 1]) {
        len -= 1;
    }
    dst[..len].copy_from_slice(&text[..len]);
    dst[len] = 0;
    len
}

/// Writes `src` into `dst` as a NUL-terminated UTF-16 string and returns the number of
/// text units written (the NUL not counted).
///
/// `src` is cut at its first NUL, then to at most `dst.len() - 1` units without splitting a
/// surrogate pair. Exactly the text and one NUL are written; the rest of `dst` is
/// untouched. An empty `dst` is not written and returns `0`.
pub fn write_utf16_str(dst: &mut [u16], src: &str) -> usize {
    let Some(room) = dst.len().checked_sub(1) else {
        return 0;
    };
    let src = src.split('\0').next().unwrap_or("");
    let mut len = 0;
    for c in src.chars() {
        let width = c.len_utf16();
        if len + width > room {
            break;
        }
        c.encode_utf16(&mut dst[len..len + width]);
        len += width;
    }
    dst[len] = 0;
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer with no NUL reads whole, and what follows a NUL is never read.
    #[test]
    fn a_reader_stops_at_the_first_nul_and_at_the_end() {
        assert_eq!(read_c_str(b"abcd"), "abcd");
        assert_eq!(read_c_str(b"ab\0cd"), "ab");
        assert_eq!(read_c_str(b"\0ab"), "");
        assert_eq!(read_c_str(b""), "");
        // A byte after the NUL that is not UTF-8 does not reach the text.
        assert_eq!(read_c_str(&[b'a', 0, 0xFF]), "a");
        // One that is, inside the text, is replaced and the rest kept.
        assert_eq!(read_c_str(&[b'a', 0xFF, b'b']), "a\u{FFFD}b");

        assert_eq!(read_utf16_str(&[0x61, 0x62]), "ab");
        assert_eq!(read_utf16_str(&[0x61, 0, 0xD800]), "a");
        assert_eq!(read_utf16_str(&[0xD800, 0]), "\u{FFFD}");
    }

    /// A UTF-8 writer never cuts inside a sequence: `é` is two bytes, so room for one
    /// text byte after `a` leaves it out.
    #[test]
    fn a_utf8_writer_cuts_on_a_character_boundary() {
        let mut dst = [0xAAu8; 4];
        assert_eq!(write_c_str(&mut dst, "aéz"), 3);
        assert_eq!(&dst, b"a\xC3\xA9\0");
        let mut dst = [0xAAu8; 3];
        assert_eq!(write_c_str(&mut dst, "aéz"), 1);
        assert_eq!(&dst, b"a\0\xAA");
        let mut dst = [0xAAu8; 3];
        assert_eq!(write_c_str(&mut dst, "音"), 0);
        assert_eq!(&dst, b"\0\xAA\xAA");
    }

    /// A writer terminates a string that fills the buffer and writes nothing past its NUL.
    #[test]
    fn a_utf8_writer_always_terminates_and_stops_at_its_nul() {
        let mut dst = [0xAAu8; 8];
        assert_eq!(write_c_str(&mut dst, "abcdefghij"), 7);
        assert_eq!(&dst, b"abcdefg\0");
        let mut dst = [0xAAu8; 8];
        assert_eq!(write_c_str(&mut dst, "hi"), 2);
        assert_eq!(&dst[..3], b"hi\0");
        assert!(dst[3..].iter().all(|&b| b == 0xAA));
        let mut dst = [0xAAu8; 8];
        assert_eq!(write_c_str(&mut dst, "a\0b"), 1);
        assert_eq!(&dst[..2], b"a\0");
        assert_eq!(dst[2], 0xAA);
        assert_eq!(write_c_str(&mut [], "x"), 0);
        assert_eq!(c_str_prefix("abc", 0), "");
        assert_eq!(c_str_prefix("abc", 1), "");
        assert_eq!(c_str_prefix("abc", 3), "ab");
        assert_eq!(c_str_prefix("a\0bc", 10), "a");
        assert_eq!(c_str_prefix("aé", 3), "a");
        assert_eq!(c_str_prefix("aé", 4), "aé");
    }

    /// A UTF-16 writer never splits a surrogate pair: U+1F3B5 is two units.
    #[test]
    fn a_utf16_writer_cuts_on_a_surrogate_pair_boundary() {
        let mut dst = [0xAAAAu16; 4];
        assert_eq!(write_utf16_str(&mut dst, "a\u{1F3B5}"), 3);
        assert_eq!(dst, [0x61, 0xD83C, 0xDFB5, 0]);
        // Room for 2 text units: the pair would need units 1 and 2, so it is left out whole.
        let mut dst = [0xAAAAu16; 3];
        assert_eq!(write_utf16_str(&mut dst, "a\u{1F3B5}"), 1);
        assert_eq!(dst, [0x61, 0, 0xAAAA]);
        // Read back, no lone surrogate was written.
        assert_eq!(read_utf16_str(&dst), "a");
    }

    /// A UTF-16 writer terminates a full buffer and stops at an interior NUL.
    #[test]
    fn a_utf16_writer_always_terminates_and_stops_at_its_nul() {
        let mut dst = [0xAAAAu16; 4];
        assert_eq!(write_utf16_str(&mut dst, "abcdef"), 3);
        assert_eq!(dst, [0x61, 0x62, 0x63, 0]);
        let mut dst = [0xAAAAu16; 4];
        assert_eq!(write_utf16_str(&mut dst, "a\0b"), 1);
        assert_eq!(dst, [0x61, 0, 0xAAAA, 0xAAAA]);
        assert_eq!(write_utf16_str(&mut [], "x"), 0);
    }

    /// A four-byte UTF-8 character is cut whole at the boundary: with room for text bytes
    /// `0..n` it is written only when all four fit.
    #[test]
    fn a_four_byte_character_is_written_whole_or_not_at_all() {
        for (capacity, written) in [(5, 0), (6, 0), (7, 0), (8, 4)] {
            let mut dst = vec![0xAAu8; capacity.max(8)];
            let dst = &mut dst[..capacity];
            assert_eq!(
                write_c_str(dst, "\u{1F3B5}"),
                written,
                "capacity {capacity}"
            );
            assert_eq!(dst[written], 0);
        }
        let mut dst = [0xAAu8; 6];
        assert_eq!(write_c_str(&mut dst, "a\u{1F3B5}"), 1);
        assert_eq!(&dst[..2], b"a\0");
    }

    /// A string that exactly fills the buffer less its NUL is written whole; one byte
    /// more loses its last byte. Capacity 1 holds only the NUL.
    #[test]
    fn an_exact_fit_is_whole_and_capacity_one_holds_only_the_nul() {
        let mut dst = [0xAAu8; 4];
        assert_eq!(write_c_str(&mut dst, "abc"), 3);
        assert_eq!(&dst, b"abc\0");
        let mut dst = [0xAAu8; 4];
        assert_eq!(write_c_str(&mut dst, "abcd"), 3);
        assert_eq!(&dst, b"abc\0");
        let mut dst = [0xAAu8; 1];
        assert_eq!(write_c_str(&mut dst, "abc"), 0);
        assert_eq!(dst, [0]);

        let mut dst = [0xAAAAu16; 4];
        assert_eq!(write_utf16_str(&mut dst, "abc"), 3);
        assert_eq!(dst, [0x61, 0x62, 0x63, 0]);
        let mut dst = [0xAAAAu16; 1];
        assert_eq!(write_utf16_str(&mut dst, "abc"), 0);
        assert_eq!(dst, [0]);
    }

    /// The `c_char` forms read and write the same bytes whichever signedness `c_char` has,
    /// including a byte above 127.
    #[test]
    fn the_c_char_forms_round_trip_a_high_byte() {
        let mut dst = [1 as c_char; 8];
        assert_eq!(write_c_chars(&mut dst, "Éclat"), 6);
        assert_eq!(read_c_chars(&dst), "Éclat");
        assert_eq!(
            read_c_chars(&[0x61 as c_char, 0xFFu8 as c_char, 0]),
            "a\u{FFFD}"
        );
        assert_eq!(read_c_chars(&[0x61 as c_char, 0, 0x62 as c_char]), "a");
        let mut dst = [1 as c_char; 3];
        assert_eq!(write_c_chars(&mut dst, "Éa"), 2);
        assert_eq!(read_c_chars(&dst), "É");
    }

    /// Copying UTF-16 units cuts before a surrogate pair that does not fit, at an interior
    /// NUL, and never writes past the NUL.
    #[test]
    fn utf16_units_are_cut_without_splitting_a_pair() {
        let src: Vec<u16> = "a\u{1F3B5}b".encode_utf16().collect(); // a, hi, lo, b
        let mut dst = [0xAAAAu16; 3];
        assert_eq!(write_utf16_units(&mut dst, &src), 1);
        assert_eq!(dst, [0x61, 0, 0xAAAA]);
        let mut dst = [0xAAAAu16; 4];
        assert_eq!(write_utf16_units(&mut dst, &src), 3);
        assert_eq!(read_utf16_str(&dst), "a\u{1F3B5}");
        let mut dst = [0xAAAAu16; 6];
        assert_eq!(write_utf16_units(&mut dst, &[0x61, 0, 0x62]), 1);
        assert_eq!(dst[..2], [0x61, 0]);
        assert_eq!(write_utf16_units(&mut [], &src), 0);
    }
}
