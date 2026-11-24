//! Fixture bytes for the SMF reader: a builder that frames raw events as a
//! Standard MIDI File, so a test states the bytes it feeds the reader.

/// The variable-length quantity: 7 bits per byte, big-endian, continuation bit
/// set on every byte but the last (SMF 1.0 section 1.1).
pub(crate) fn write_vlq(out: &mut Vec<u8>, mut value: u32) {
    let mut buf = [0u8; 4];
    let mut n = 0;
    loop {
        buf[n] = (value & 0x7F) as u8;
        n += 1;
        value >>= 7;
        if value == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        // Continuation bit on everything but the final byte.
        out.push(buf[i] | if i == 0 { 0x00 } else { 0x80 });
    }
}

/// One event to encode, as a delta plus raw status/data bytes.
pub(crate) struct RawEvent {
    pub delta: u32,
    /// The full event, status byte first. Meta events start `0xFF`, sysex
    /// `0xF0`; channel messages carry their own status.
    pub bytes: Vec<u8>,
}

impl RawEvent {
    pub(crate) fn new(delta: u32, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            delta,
            bytes: bytes.into(),
        }
    }

    pub(crate) fn note_on(delta: u32, channel: u8, key: u8, vel: u8) -> Self {
        Self::new(delta, vec![0x90 | (channel & 0x0F), key, vel])
    }

    pub(crate) fn note_off(delta: u32, channel: u8, key: u8, vel: u8) -> Self {
        Self::new(delta, vec![0x80 | (channel & 0x0F), key, vel])
    }

    /// Set Tempo, microseconds per quarter note.
    pub(crate) fn tempo(delta: u32, us_per_quarter: u32) -> Self {
        let b = us_per_quarter.to_be_bytes();
        Self::new(delta, vec![0xFF, 0x51, 0x03, b[1], b[2], b[3]])
    }

    pub(crate) fn track_name(delta: u32, name: &str) -> Self {
        let mut v = vec![0xFF, 0x03];
        write_vlq(&mut v, name.len() as u32);
        v.extend_from_slice(name.as_bytes());
        Self::new(delta, v)
    }

    /// `numerator`/`2^denominator_pow`, with the usual 24/8 clock defaults.
    pub(crate) fn time_signature(delta: u32, numerator: u8, denominator_pow: u8) -> Self {
        Self::new(
            delta,
            vec![0xFF, 0x58, 0x04, numerator, denominator_pow, 24, 8],
        )
    }

    /// An F0 sysex with a length-prefixed payload, terminated 0xF7.
    pub(crate) fn sysex(delta: u32, payload: &[u8]) -> Self {
        let mut v = vec![0xF0];
        write_vlq(&mut v, payload.len() as u32 + 1);
        v.extend_from_slice(payload);
        v.push(0xF7);
        Self::new(delta, v)
    }

    /// A channel message with its status byte omitted — legal when the
    /// previous event carried the same status. a reader must reinstate it.
    pub(crate) fn running_status(delta: u32, data: impl Into<Vec<u8>>) -> Self {
        Self::new(delta, data)
    }
}

/// How the header declares its time division.
#[derive(Clone, Copy)]
pub(crate) enum Division {
    /// Ticks per quarter note. Zero is representable and must be rejected by
    /// any reader that divides by it.
    Metrical(u16),
    /// SMPTE: negative frames-per-second byte plus ticks-per-frame.
    Smpte { fps: i8, ticks_per_frame: u8 },
}

impl Division {
    fn to_bytes(self) -> [u8; 2] {
        match self {
            Division::Metrical(tpq) => tpq.to_be_bytes(),
            Division::Smpte {
                fps,
                ticks_per_frame,
            } => [fps as u8, ticks_per_frame],
        }
    }
}

/// Build a complete SMF byte stream. `format` is 0, 1 or 2.
pub(crate) fn build_smf(format: u16, division: Division, tracks: &[Vec<RawEvent>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&format.to_be_bytes());
    out.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
    out.extend_from_slice(&division.to_bytes());

    for events in tracks {
        let mut body = Vec::new();
        for e in events {
            write_vlq(&mut body, e.delta);
            body.extend_from_slice(&e.bytes);
        }
        // End of Track is mandatory and is what tells a reader the chunk is
        // whole; omitting it is a different (also interesting) test.
        write_vlq(&mut body, 0);
        body.extend_from_slice(&[0xFF, 0x2F, 0x00]);

        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
    }
    out
}
