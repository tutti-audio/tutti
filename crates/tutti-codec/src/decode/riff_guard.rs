//! A pre-flight over every RIFF/WAVE `fmt ` chunk a source holds, run before
//! the decoder probes it.
//!
//! The WAVE demuxer does arithmetic on the channel count and channel mask of
//! the `fmt ` chunk (`channels * (bits / 8)` in `u16`, and shifts by the number
//! of channels the mask lacks) that overflows for counts no real file states.
//! A build with overflow checks panics there, so [`check`] refuses such a
//! chunk first: a wrong value is refused before it reaches the arithmetic.
//!
//! The probe anchors on the first container marker in the first
//! [`SCAN_DEPTH`] bytes, which may follow a prefix, so [`check`] walks the
//! chunks of every `RIFF....WAVE` header in that range, up to the `data` chunk
//! the demuxer stops at. It reads through a fixed buffer and allocates nothing.

use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::errors::Error;

/// How far into a source a marker is looked for: the probe's own default depth.
const SCAN_DEPTH: u64 = 1 << 20;
/// The bytes read per step of the marker scan.
const WINDOW: usize = 4096;
/// The widest channel count any WAVE channel map takes: a channel mask has 32
/// bits, and the plain counts the demuxer maps stop below that. A wider
/// count is refused here; one this wide the demuxer still refuses itself.
const MAX_CHANNELS: u16 = 32;
/// `WAVE_FORMAT_EXTENSIBLE`.
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// The bytes of a `fmt ` chunk the guard reads: the extensible layout, whose
/// channel mask is at 20 and sub-format GUID at 24.
const FMT_HEAD: usize = 40;
/// The last twelve bytes of the sub-format GUIDs of Ambisonic B-format, which
/// map a count to components and never touch the channel mask.
const AMBISONIC_TAIL: [u8; 12] = [
    0x21, 0x07, 0xd3, 0x11, 0x86, 0x44, 0xc8, 0xc1, 0xca, 0x00, 0x00, 0x00,
];

/// Refuses a source holding a WAVE `fmt ` chunk the demuxer would overflow on,
/// and leaves the source where it found it.
///
/// A chunk is refused when it states more than [`MAX_CHANNELS`] channels
/// (`channels * (bits / 8)` is then within `u16`), or, as an extensible
/// non-Ambisonic format, a channel mask that cannot be corrected to its count:
/// the demuxer corrects a mask short of ones by shifting `(1 << missing) - 1`
/// above the mask's top bit, which overflows for 32 or more missing ones or a
/// top bit at 31.
pub(super) fn check<R: Read + Seek + ?Sized>(source: &mut R) -> Result<(), Error> {
    let start = source.stream_position().map_err(Error::IoError)?;
    let result = scan(source, start);
    source
        .seek(SeekFrom::Start(start))
        .map_err(Error::IoError)?;
    result
}

/// Checks the WAVE chunks behind every `RIFF` in the first [`SCAN_DEPTH`] bytes.
fn scan<R: Read + Seek + ?Sized>(source: &mut R, start: u64) -> Result<(), Error> {
    let mut window = [0u8; WINDOW];
    let mut base = start;
    loop {
        source.seek(SeekFrom::Start(base)).map_err(Error::IoError)?;
        let n = read_up_to(source, &mut window).map_err(Error::IoError)?;
        for at in 0..n.saturating_sub(3) {
            if &window[at..at + 4] == b"RIFF" {
                check_riff(source, base + at as u64)?;
            }
        }
        if n < WINDOW || base + n as u64 >= start + SCAN_DEPTH {
            return Ok(());
        }
        // Three bytes overlap, so a marker cut by the window's end is found by the next.
        base += n as u64 - 3;
    }
}

/// Walks the chunks of the RIFF header at `at`, checking each `fmt ` chunk,
/// until the `data` chunk, the end of the RIFF chunk or the end of the source.
fn check_riff<R: Read + Seek + ?Sized>(source: &mut R, at: u64) -> Result<(), Error> {
    let mut head = [0u8; 8];
    if !read_header(source, at + 4, &mut head)? || &head[4..8] != b"WAVE" {
        return Ok(());
    }
    let riff_len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let end = if riff_len == u32::MAX {
        u64::MAX
    } else {
        at + 8 + u64::from(riff_len)
    };
    let mut pos = at + 12;
    while pos.saturating_add(8) <= end {
        if !read_header(source, pos, &mut head)? {
            return Ok(());
        }
        let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        match &head[0..4] {
            b"data" => return Ok(()),
            b"fmt " => check_fmt(source, pos + 8, len)?,
            _ => {}
        }
        // A chunk is padded to an even length.
        pos += 8 + u64::from(len) + u64::from(len & 1);
    }
    Ok(())
}

/// Checks the `fmt ` chunk body at `at`, `len` bytes long.
fn check_fmt<R: Read + Seek + ?Sized>(source: &mut R, at: u64, len: u32) -> Result<(), Error> {
    // A chunk shorter than the basic layout is refused by the demuxer itself.
    if len < 16 {
        return Ok(());
    }
    let take = (len as usize).min(FMT_HEAD);
    let mut body = [0u8; FMT_HEAD];
    source.seek(SeekFrom::Start(at)).map_err(Error::IoError)?;
    match source.read_exact(&mut body[..take]) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(e) => return Err(Error::IoError(e)),
    }
    let format = u16::from_le_bytes([body[0], body[1]]);
    let channels = u16::from_le_bytes([body[2], body[3]]);
    if channels > MAX_CHANNELS {
        return Err(Error::Unsupported("wav: more than 32 channels"));
    }
    if format == FORMAT_EXTENSIBLE && take == FMT_HEAD && body[28..40] != AMBISONIC_TAIL {
        let mask = u32::from_le_bytes([body[20], body[21], body[22], body[23]]);
        let missing = u32::from(channels).saturating_sub(mask.count_ones());
        if missing > 0 && (missing >= 32 || mask >> 31 != 0) {
            return Err(Error::DecodeError(
                "wav: the channel mask cannot hold the channel count",
            ));
        }
    }
    Ok(())
}

/// Reads eight bytes at `at`; `false` when the source ends first.
fn read_header<R: Read + Seek + ?Sized>(
    source: &mut R,
    at: u64,
    head: &mut [u8; 8],
) -> Result<bool, Error> {
    source.seek(SeekFrom::Start(at)).map_err(Error::IoError)?;
    match source.read_exact(head) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(Error::IoError(e)),
    }
}

/// Reads until `buf` is full or the source ends; the count read.
fn read_up_to<R: Read + ?Sized>(source: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match source.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(all(test, feature = "decode-wav"))]
mod tests;
