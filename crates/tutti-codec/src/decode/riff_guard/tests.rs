use std::io::{self, Cursor};

use super::*;
use crate::{DecodeError, read_wave, read_wave_metadata};

const PCM: u16 = 1;
const FLOAT: u16 = 3;
const PCM_GUID: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];
const AMBISONIC_PCM_GUID: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x21, 0x07, 0xd3, 0x11, 0x86, 0x44, 0xc8, 0xc1, 0xca, 0x00, 0x00, 0x00,
];

/// A chunk: tag, length, body, and the pad byte of an odd length.
fn chunk(tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = tag.to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// The 16-byte basic `fmt ` body.
fn basic_fmt(format: u16, channels: u16, bits: u16) -> Vec<u8> {
    let block = channels.wrapping_mul(bits / 8);
    let mut out = Vec::new();
    out.extend(format.to_le_bytes());
    out.extend(channels.to_le_bytes());
    out.extend(48_000u32.to_le_bytes());
    out.extend((48_000 * u32::from(block)).to_le_bytes());
    out.extend(block.to_le_bytes());
    out.extend(bits.to_le_bytes());
    out
}

/// The 40-byte extensible `fmt ` body with a 16-bit container.
fn extensible_fmt(channels: u16, mask: u32, guid: [u8; 16]) -> Vec<u8> {
    let mut out = basic_fmt(FORMAT_EXTENSIBLE, channels, 16);
    out.extend(22u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(mask.to_le_bytes());
    out.extend(guid);
    out
}

/// A RIFF/WAVE file with `fmt` and four frames of silence, after `prefix`.
fn wav(prefix: &[u8], fmt: &[u8]) -> Vec<u8> {
    let mut body = b"WAVE".to_vec();
    body.extend(chunk(b"fmt ", fmt));
    body.extend(chunk(b"data", &[0u8; 64]));
    let mut out = prefix.to_vec();
    out.extend(b"RIFF");
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

fn guard(bytes: &[u8]) -> Result<(), Error> {
    check(&mut Cursor::new(bytes))
}

/// Junk before `RIFF`, then a PCM `fmt `
/// of 65,535 channels at 16 bits, whose `channels * (bits / 8)` overflows `u16`.
#[test]
fn pcm_of_65535_channels_after_a_prefix_is_refused_not_a_panic() {
    let junk = [
        0x52, 0x52, 0xd3, 0xd3, 0xd3, 0xd3, 0xd3, 0xd3, 0xd3, 0xd3, 0x96,
    ];
    let bytes = wav(&junk, &basic_fmt(PCM, 65_535, 16));
    assert!(matches!(guard(&bytes), Err(Error::Unsupported(_))));
    assert!(matches!(
        read_wave::<f32>(bytes),
        Err(DecodeError::Unsupported(_))
    ));
}

/// Channel counts whose block align overflows `u16` at each sample width are
/// refused, whatever the format tag.
#[test]
fn a_channel_count_whose_block_align_overflows_u16_is_refused_at_every_width() {
    for (format, bits) in [
        (PCM, 8),
        (PCM, 16),
        (PCM, 24),
        (PCM, 32),
        (FLOAT, 32),
        (FLOAT, 64),
    ] {
        for channels in [16_384u16, 21_846, 32_768, u16::MAX] {
            let bytes = wav(&[], &basic_fmt(format, channels, bits));
            assert!(
                matches!(guard(&bytes), Err(Error::Unsupported(_))),
                "format {format} bits {bits} channels {channels}"
            );
        }
    }
}

/// The widest count the guard lets through is 32; one more is refused.
#[test]
fn the_channel_count_is_capped_at_32() {
    assert!(guard(&wav(&[], &basic_fmt(PCM, 32, 32))).is_ok());
    assert!(guard(&wav(&[], &basic_fmt(PCM, 33, 8))).is_err());
}

/// A valid mono and a valid stereo 16-bit file pass the guard and decode, with
/// the cursor where it started.
#[test]
fn valid_mono_and_stereo_16_bit_files_pass_and_decode() {
    for channels in [1u16, 2] {
        let bytes = wav(&[], &basic_fmt(PCM, channels, 16));
        let mut cursor = Cursor::new(bytes.as_slice());
        assert!(check(&mut cursor).is_ok());
        assert_eq!(cursor.position(), 0);
        let wave = read_wave::<f32>(bytes).expect("decodes");
        assert_eq!(wave.channels(), usize::from(channels));
        assert_eq!(wave.len().get(), 32 / usize::from(channels));
    }
}

/// An extensible mask short of ones by 32 or more is refused: the mask fix-up
/// shifts `1 << 32`.
#[test]
fn an_extensible_mask_missing_32_ones_is_refused() {
    let bytes = wav(&[], &extensible_fmt(32, 0, PCM_GUID));
    assert!(matches!(guard(&bytes), Err(Error::DecodeError(_))));
}

/// An extensible mask with bit 31 set and ones to add is refused: the fix-up
/// shifts by 32. With nothing to add it passes.
#[test]
fn an_extensible_mask_with_bit_31_and_missing_ones_is_refused() {
    let bad = wav(&[], &extensible_fmt(2, 0x8000_0000, PCM_GUID));
    assert!(matches!(guard(&bad), Err(Error::DecodeError(_))));
    let exact = wav(&[], &extensible_fmt(1, 0x8000_0000, PCM_GUID));
    assert!(guard(&exact).is_ok());
}

/// An ordinary extensible mask, stereo `0b11` or one missing ones low in the
/// word, passes.
#[test]
fn an_ordinary_extensible_mask_passes() {
    assert!(guard(&wav(&[], &extensible_fmt(2, 0b11, PCM_GUID))).is_ok());
    assert!(guard(&wav(&[], &extensible_fmt(6, 0b11, PCM_GUID))).is_ok());
}

/// Ambisonic B-format never reads the mask, so its mask is not judged.
#[test]
fn an_ambisonic_extensible_format_is_not_judged_by_its_mask() {
    let bytes = wav(&[], &extensible_fmt(4, 0x8000_0000, AMBISONIC_PCM_GUID));
    assert!(guard(&bytes).is_ok());
}

/// A bad `fmt ` behind another chunk and behind an odd-length chunk's pad byte
/// is found.
#[test]
fn a_fmt_chunk_after_other_chunks_is_found() {
    let mut body = b"WAVE".to_vec();
    body.extend(chunk(b"fact", &[1, 2, 3]));
    body.extend(chunk(b"fmt ", &basic_fmt(PCM, 65_535, 16)));
    body.extend(chunk(b"data", &[0u8; 8]));
    let mut bytes = b"RIFF".to_vec();
    bytes.extend((body.len() as u32).to_le_bytes());
    bytes.extend(body);
    assert!(guard(&bytes).is_err());
}

/// A marker further into the source than one window is found, and one cut by
/// a window's end is too.
#[test]
fn a_marker_past_the_first_window_and_across_a_window_edge_is_found() {
    for prefix_len in [
        WINDOW + 100,
        WINDOW - 2,
        WINDOW - 1,
        WINDOW - 3,
        3 * WINDOW - 6,
    ] {
        let bytes = wav(&vec![0u8; prefix_len], &basic_fmt(PCM, 65_535, 16));
        assert!(guard(&bytes).is_err(), "prefix {prefix_len}");
    }
}

/// Bytes that are no WAVE, and a truncated header, pass untouched for the
/// decoder to judge.
#[test]
fn non_wave_and_truncated_sources_pass() {
    assert!(guard(&[]).is_ok());
    assert!(guard(b"RIFF").is_ok());
    assert!(guard(b"RIFF\x04\0\0\0WAVE").is_ok());
    assert!(guard(b"RIFF\x04\0\0\0AVI ").is_ok());
    assert!(guard(&[0x5a; 10_000]).is_ok());
}

/// The path entry points refuse the same file, as `read_wave` and `read_metadata`.
#[test]
fn a_file_of_65535_channels_is_refused_by_the_path_entry_points() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("wide.wav");
    std::fs::write(&path, wav(&[0xd3; 5], &basic_fmt(PCM, 65_535, 16))).expect("write");
    assert!(matches!(
        read_wave::<f32>(&path),
        Err(DecodeError::Unsupported(_))
    ));
    assert!(matches!(
        read_wave_metadata(&path),
        Err(DecodeError::Unsupported(_))
    ));
}

/// A source that counts the bytes read from it.
struct Counting<'a> {
    inner: Cursor<&'a [u8]>,
    read: usize,
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n;
        Ok(n)
    }
}

impl Seek for Counting<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.inner.seek(to)
    }
}

/// A source that opens as a FLAC or Ogg stream this build reads is not scanned for a WAVE
/// header: at most the opening bytes are read, though a bad WAVE follows.
#[test]
fn a_source_opening_as_another_container_is_not_scanned() {
    let bad = wav(&[], &basic_fmt(PCM, 65_535, 16));
    let mut openings: Vec<&[u8]> = Vec::new();
    if cfg!(feature = "decode-flac") {
        openings.push(b"fLaC");
    }
    if cfg!(feature = "decode-ogg") {
        openings.push(b"OggS");
    }
    assert!(!openings.is_empty(), "run with the decode features");
    for opening in openings {
        let mut bytes = opening.to_vec();
        bytes.extend(vec![0u8; 20_000]);
        bytes.extend(&bad);
        let mut source = Counting {
            inner: Cursor::new(&bytes),
            read: 0,
        };
        assert!(check(&mut source).is_ok());
        assert!(source.read <= 12, "{opening:?} read {}", source.read);
    }
}

/// An ID3v2 tag is skipped by its syncsafe size: a `RIFF` inside it is not the
/// anchor, the one after it is.
#[test]
fn an_id3v2_tag_holding_riff_is_skipped_past() {
    let bad = wav(&[], &basic_fmt(PCM, 65_535, 16));
    let good = wav(&[], &basic_fmt(PCM, 2, 16));
    let tag = id3(4, 0, &bad);
    let mut skipped = tag.clone();
    skipped.extend(&good);
    assert!(guard(&skipped).is_ok());
    let mut refused = tag;
    refused.extend(&bad);
    assert!(guard(&refused).is_err());
}

/// Only the first `RIFF....WAVE` is the anchor, and a `RIFF` of another form
/// before it is walked past.
#[test]
fn only_the_first_riff_after_the_start_is_checked() {
    let mut bytes = wav(&[], &basic_fmt(PCM, 2, 16));
    bytes.extend(wav(&[], &basic_fmt(PCM, 65_535, 16)));
    assert!(guard(&bytes).is_ok());
}

/// A `RIFF` of another form, then a WAVE header with
/// a 65,535-channel `fmt `, is refused.
#[test]
fn a_riff_of_another_form_does_not_hide_a_later_wave_header() {
    let mut bytes = b"RIFF\x64\0\0\0AVI ".to_vec();
    bytes.extend(wav(&[], &basic_fmt(PCM, 65_535, 16)));
    assert!(guard(&bytes).is_err());
}

/// A source opening with a bare MP3 frame sync, or the marker of a container
/// this build does not read, is still scanned: the probe scans past both.
#[test]
fn a_bad_mp3_sync_before_a_wave_header_does_not_hide_it() {
    let mut bytes = vec![0xFF, 0xFF, 0xFF, 0x89];
    bytes.extend(wav(&[], &basic_fmt(PCM, 65_535, 16)));
    assert!(guard(&bytes).is_err());
}

/// A `FORM` prefix is no anchor to the guard (an AIFF holds no `fmt ` chunk
/// to check), so it scans on to the WAVE header.
#[test]
fn a_form_marker_does_not_hide_a_later_wave_header() {
    let mut bytes = b"FORM".to_vec();
    bytes.extend(wav(&[], &basic_fmt(PCM, 65_535, 16)));
    assert!(guard(&bytes).is_err());
}

/// An ID3v2 tag of the given version and flags around `body`, as the probe's
/// tag reader frames it: a 10-byte header and a syncsafe size.
fn id3(major: u8, flags: u8, body: &[u8]) -> Vec<u8> {
    let size = body.len() as u32;
    let mut tag = vec![b'I', b'D', b'3', major, 0, flags];
    tag.extend([
        (size >> 21) as u8 & 0x7f,
        (size >> 14) as u8 & 0x7f,
        (size >> 7) as u8 & 0x7f,
        size as u8 & 0x7f,
    ]);
    tag.extend(body);
    tag
}

/// A header the probe's tag reader refuses (version 5) is no tag: the probe
/// scans into it, so the overflowing WAVE inside it is refused.
#[test]
fn a_version_5_id3_header_is_not_skipped() {
    let bad = wav(&[], &basic_fmt(PCM, 65_535, 16));
    let mut bytes = id3(5, 0, &bad);
    bytes.extend(wav(&[], &basic_fmt(PCM, 2, 16)));
    assert!(guard(&bytes).is_err());
    let mut ff = id3(3, 0, &bad);
    ff[4] = 0xFF;
    assert!(guard(&ff).is_err());
    let mut compressed = id3(2, 0x40, &bad);
    compressed.extend(wav(&[], &basic_fmt(PCM, 2, 16)));
    assert!(guard(&compressed).is_err());
}

/// The probe's tag reader consumes the header and `size` bytes only, so a
/// footer flag leaves the bytes after the tag to the scan, in any version.
#[test]
fn the_footer_flag_does_not_extend_the_skipped_tag() {
    for major in [3, 4] {
        let mut bytes = id3(major, 0x10, b"tag");
        bytes.extend(wav(&[], &basic_fmt(PCM, 65_535, 16)));
        assert!(guard(&bytes).is_err(), "version {major}");
    }
}

/// Consecutive tags are each skipped, and a WAVE header after them is checked.
#[test]
fn two_consecutive_id3_tags_before_a_bad_wave_header_are_skipped_to_it() {
    let bad = wav(&[], &basic_fmt(PCM, 65_535, 16));
    let mut bytes = id3(4, 0, &bad);
    bytes.extend(id3(3, 0, &bad));
    bytes.extend(&bad);
    assert!(guard(&bytes).is_err());
    let mut good = id3(4, 0, &bad);
    good.extend(id3(3, 0, &bad));
    good.extend(wav(&[], &basic_fmt(PCM, 2, 16)));
    assert!(guard(&good).is_ok());
}

/// A tag after a prefix is skipped as well: the probe reads it wherever it
/// finds it.
#[test]
fn an_id3_tag_after_a_prefix_is_skipped() {
    let bad = wav(&[], &basic_fmt(PCM, 65_535, 16));
    let mut bytes = vec![0x11; 7];
    bytes.extend(id3(4, 0, &bad));
    bytes.extend(wav(&[], &basic_fmt(PCM, 2, 16)));
    assert!(guard(&bytes).is_ok());
}

/// Every `fmt ` before the `data` chunk is checked, not only the first.
#[test]
fn a_second_bad_fmt_chunk_after_a_good_one_is_refused() {
    let mut body = b"WAVE".to_vec();
    body.extend(chunk(b"fmt ", &basic_fmt(PCM, 2, 16)));
    body.extend(chunk(b"fmt ", &basic_fmt(PCM, 65_535, 16)));
    body.extend(chunk(b"data", &[0u8; 8]));
    let mut bytes = b"RIFF".to_vec();
    bytes.extend((body.len() as u32).to_le_bytes());
    bytes.extend(body);
    assert!(guard(&bytes).is_err());
}

/// A `fmt ` shorter than the 16-byte basic layout is left to the decoder,
/// which refuses it: the guard passes it.
#[test]
fn a_fmt_chunk_shorter_than_16_bytes_passes_the_guard_and_the_decoder_refuses_it() {
    let bytes = wav(&[], &basic_fmt(PCM, 65_535, 16)[..12]);
    assert!(guard(&bytes).is_ok());
    assert!(read_wave::<f32>(bytes).is_err());
}
