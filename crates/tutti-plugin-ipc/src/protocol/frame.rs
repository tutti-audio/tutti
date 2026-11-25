//! The one framing of the lane sockets, shared by the host and the
//! subprocess.
//!
//! A frame is a four-byte big-endian length, then that many bytes of
//! [postcard](https://docs.rs/postcard), which encode exactly one message.
//! The length is bounded by the message type's own cap ([`Framed::CAP`], one
//! per lane and direction) on both ends: a sender refuses
//! to emit a frame its peer must reject (and never lets a cast truncate the
//! prefix, which would desynchronise a stream that has no resynchronisation
//! point), and a receiver refuses an advertised length before allocating for
//! it.
//!
//! The host reads incrementally against a deadline, so it takes the prefix and
//! body halves ([`decode_prefix`], [`decode_body`]); the subprocess blocks, and
//! takes [`read_frame`]. Both send through [`write_frame`].
//!
//! **A frame costs no allocation once its buffers are warm**.
//! Each thread writes through one buffer and the subprocess reads through one
//! (the host's read buffer is its `PartialFrame`'s), each kept between frames
//! and trimmed back to [`KEEP_BYTES`] after a larger one ([`keep`]). A message
//! with no heap fields (a block's reply) then decodes without allocating too.

use crate::error::codec;
use std::cell::RefCell;
use std::io::{Read, Write};
use tutti_plugin_types::{FrameTooLarge, IpcError, IpcResult};

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{AUDIO_REPLY_FRAME_BYTES, AUDIO_REQUEST_FRAME_BYTES, CONTROL_FRAME_BYTES};
use super::{AudioReply, AudioRequest, ControlFrame, ControlReply, Request};
use crate::BlobWire;

/// A wire type with the longest frame its lane carries it in.
///
/// The cap is the type's, so a read or a write of a message cannot be given
/// another lane's: the control lane's messages are held to
/// [`CONTROL_FRAME_BYTES`] and a block's reply to [`AUDIO_REPLY_FRAME_BYTES`].
pub trait Framed: Serialize + DeserializeOwned {
    /// The most body bytes a frame of this type may advertise or carry.
    const CAP: usize;

    /// The blob this frame announces, which follows it on the stream as a
    /// message of its own: what a reader takes right after the frame.
    fn blob(&self) -> Option<&BlobWire> {
        None
    }
}

impl Framed for AudioRequest {
    const CAP: usize = AUDIO_REQUEST_FRAME_BYTES;
}
impl Framed for AudioReply {
    const CAP: usize = AUDIO_REPLY_FRAME_BYTES;
}
impl Framed for Request {
    const CAP: usize = CONTROL_FRAME_BYTES;
}
impl Framed for ControlFrame {
    const CAP: usize = CONTROL_FRAME_BYTES;

    fn blob(&self) -> Option<&BlobWire> {
        match self {
            Self::Reply {
                reply: ControlReply::StateReady { blob, .. },
                ..
            } => Some(blob),
            _ => None,
        }
    }
}

/// The error for a frame of `len` bytes, over `cap`.
const fn too_large(len: usize, cap: usize) -> FrameTooLarge {
    FrameTooLarge { len, limit: cap }
}

/// The length prefix for a body of `body_len` bytes.
///
/// # Errors
///
/// [`FrameTooLarge`] when `body_len` is over `cap`.
pub(crate) fn encode_prefix(
    body_len: usize,
    cap: usize,
) -> std::result::Result<[u8; 4], FrameTooLarge> {
    if body_len > cap {
        return Err(too_large(body_len, cap));
    }
    // The cap is checked at compile time to fit a `u32` (the `const _` below).
    Ok((body_len as u32).to_be_bytes())
}

/// The body length a prefix advertises.
///
/// # Errors
///
/// [`FrameTooLarge`] when it is over `cap`; no body is to be
/// read or allocated for it.
pub(crate) fn decode_prefix(
    prefix: [u8; 4],
    cap: usize,
) -> std::result::Result<usize, FrameTooLarge> {
    let len = u32::from_be_bytes(prefix) as usize;
    if len > cap {
        return Err(too_large(len, cap));
    }
    Ok(len)
}

/// Decodes one frame's body, which is exactly one message.
///
/// # Errors
///
/// A codec error for a body that does not decode, or that has bytes left
/// after the message: either is a protocol-version skew the handshake missed.
pub(crate) fn decode_body<T: DeserializeOwned>(body: &[u8]) -> IpcResult<T> {
    match postcard::take_from_bytes(body).map_err(codec)? {
        (msg, []) => Ok(msg),
        (_, rest) => Err(IpcError::TrailingBytes {
            left: rest.len(),
            frame: body.len(),
        }),
    }
}

/// How much capacity a frame buffer keeps between frames. A block's frames
/// are a few KiB; a large control frame is rare, so a buffer it grew is
/// trimmed back rather than held for the connection's life.
pub(crate) const KEEP_BYTES: usize = 256 * 1024;

/// Trim `buf` back to [`KEEP_BYTES`] of capacity after a larger frame.
pub(crate) fn keep(buf: &mut Vec<u8>) {
    if buf.capacity() > KEEP_BYTES {
        buf.clear();
        buf.shrink_to(KEEP_BYTES);
    }
}

thread_local! {
    /// This thread's outgoing frame: prefix and body, serialized in place.
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// This thread's incoming body, for [`read_frame`].
    static IN: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Serializes `msg` and writes it as one frame, in one write, then flushes.
///
/// The frame is serialized into this thread's reused buffer behind a prefix
/// placeholder, so nothing is allocated once the buffer is warm, and one
/// `write_all` sends prefix and body together.
///
/// # Errors
///
/// `FrameTooLarge` (as `IpcError::FrameTooLarge`) when the body is
/// over the cap, with nothing written; a serialization or I/O error otherwise.
pub fn write_frame<W: Write, T: Framed>(w: &mut W, msg: &T) -> IpcResult<()> {
    OUT.with_borrow_mut(|frame| {
        frame.clear();
        frame.extend_from_slice(&[0; 4]);
        // `to_extend` takes the buffer and hands it back; taking it out of the
        // cell keeps its capacity, so the warm path still allocates nothing.
        let written = postcard::to_extend(msg, std::mem::take(frame))
            .map_err(codec)
            .and_then(|out| {
                *frame = out;
                Ok(encode_prefix(frame.len() - 4, T::CAP)?)
            })
            .and_then(|prefix| {
                frame[..4].copy_from_slice(&prefix);
                w.write_all(frame)?;
                Ok(w.flush()?)
            });
        keep(frame);
        written
    })
}

/// Blocks until one whole frame has been read, and decodes it.
///
/// # Errors
///
/// An I/O error on EOF or a short read; `FrameTooLarge` (as
/// [`IpcError::FrameTooLarge`]) for an advertised length over the cap,
/// before any allocation; a codec error for a body that does not decode as
/// exactly one message.
pub fn read_frame<R: Read, T: Framed>(r: &mut R) -> IpcResult<T> {
    let mut prefix = [0u8; 4];
    r.read_exact(&mut prefix)?;
    let len = decode_prefix(prefix, T::CAP)?;
    IN.with_borrow_mut(|body| {
        body.clear();
        body.resize(len, 0);
        let msg = r
            .read_exact(body)
            .map_err(IpcError::from)
            .and_then(|()| decode_body(body));
        keep(body);
        msg
    })
}

/// The frame buffers of one connection that may not allocate per frame: an
/// inbound and an outbound body of a capacity fixed when it is built.
///
/// [`read`](Self::read) refuses a frame longer than the inbound capacity
/// before reading its body (the length is the peer's to choose), and
/// [`write`](Self::write) refuses a message that does not serialize into the
/// outbound one. Neither grows a buffer or touches the heap, and the buffers
/// are never trimmed: build one off the thread that reads and writes, and
/// move it there. The wire is the same as [`read_frame`] and [`write_frame`].
///
/// ```
/// use tutti_plugin_ipc::{ApplyOutcome, AudioReply, FrameBuf, ReqId};
///
/// let mut buf = FrameBuf::new(16, 16);
/// let mut wire = Vec::new();
/// buf.write(&mut wire, &AudioReply::ParametersApplied { id: ReqId::new(7), outcome: ApplyOutcome::Applied }).unwrap();
/// let back: AudioReply = buf.read(&mut wire.as_slice()).unwrap();
/// assert!(matches!(back, AudioReply::ParametersApplied { id, .. } if id == ReqId::new(7)));
/// ```
#[derive(Debug)]
pub struct FrameBuf {
    inbound: Box<[u8]>,
    outbound: Box<[u8]>,
}

impl FrameBuf {
    /// Buffers for inbound bodies of up to `inbound` bytes and outbound
    /// bodies of up to `outbound`; a message type's own [`Framed::CAP`] still
    /// bounds each frame. Allocates: build it off the audio thread.
    #[must_use]
    pub fn new(inbound: usize, outbound: usize) -> Self {
        Self {
            inbound: vec![0; inbound].into_boxed_slice(),
            outbound: vec![0; outbound + PREFIX_BYTES].into_boxed_slice(),
        }
    }

    /// Blocks until one whole frame has been read, and decodes it.
    ///
    /// # Errors
    ///
    /// An I/O error on EOF or a short read; `FrameTooLarge` for an advertised
    /// length over the inbound capacity (or the type's [`Framed::CAP`]), before any
    /// body is read; a codec error for a body that does not decode as exactly
    /// one message.
    pub fn read<R: Read, T: Framed>(&mut self, r: &mut R) -> IpcResult<T> {
        let mut prefix = [0u8; PREFIX_BYTES];
        r.read_exact(&mut prefix)?;
        let len = decode_prefix(prefix, T::CAP)?;
        if len > self.inbound.len() {
            return Err(FrameTooLarge {
                len,
                limit: self.inbound.len(),
            }
            .into());
        }
        let body = &mut self.inbound[..len];
        r.read_exact(body)?;
        decode_body(body)
    }

    /// Serializes `msg` and writes it as one frame, in one write, then flushes.
    ///
    /// # Errors
    ///
    /// A serialization error when the message does not fit the outbound
    /// capacity, with nothing written; an I/O error otherwise.
    pub fn write<W: Write, T: Framed>(&mut self, w: &mut W, msg: &T) -> IpcResult<()> {
        let (prefix, body) = self.outbound.split_at_mut(PREFIX_BYTES);
        let len = postcard::to_slice(msg, body).map_err(codec)?.len();
        prefix.copy_from_slice(&encode_prefix(len, T::CAP)?);
        w.write_all(&self.outbound[..PREFIX_BYTES + len])?;
        Ok(w.flush()?)
    }
}

/// The bytes of a frame's length prefix.
const PREFIX_BYTES: usize = 4;

/// The bounds the lane caps have to sit between, checked at compile time.
///
/// A `const` block rather than a `#[test]`: every operand is a constant, so a
/// bad cap fails the build rather than one test binary.
const _: () = {
    // `encode_prefix` writes `body_len as u32`. That cast is only safe because
    // the guard above it rejects everything larger. The audio request cap is
    // the largest of the three.
    assert!(
        CONTROL_FRAME_BYTES <= AUDIO_REQUEST_FRAME_BYTES
            && AUDIO_REPLY_FRAME_BYTES <= AUDIO_REQUEST_FRAME_BYTES
            && AUDIO_REQUEST_FRAME_BYTES <= u32::MAX as usize,
        "a lane cap exceeds what a u32 length prefix can carry, so \
         `body_len as u32` in `encode_prefix` can still truncate and desynchronise \
         the stream"
    );
};

#[cfg(test)]
mod tests {
    use super::*;

    impl Framed for u8 {
        const CAP: usize = 64;
    }
    impl Framed for u16 {
        const CAP: usize = 64;
    }
    impl Framed for (u32, [u8; 3]) {
        const CAP: usize = 64;
    }
    impl Framed for [u8; 8] {
        const CAP: usize = 64;
    }

    /// **The frame is a big-endian `u32` length and the postcard body.** Bytes
    /// written out by hand for a `u16` message, so a change of prefix width or
    /// endianness fails here rather than as a desync between two binaries.
    #[test]
    fn a_frame_is_a_big_endian_length_then_the_body() {
        let mut out = Vec::new();
        write_frame(&mut out, &0x0102u16).unwrap();
        // postcard writes an integer as a varint: seven bits a byte, low first.
        assert_eq!(out, [0, 0, 0, 2, 0x82, 0x02]);

        let back: u16 = read_frame(&mut out.as_slice()).unwrap();
        assert_eq!(back, 0x0102);
    }

    /// **A `FrameBuf` speaks the wire `write_frame` and `read_frame` do**: the
    /// bytes it writes are the bytes `write_frame` writes, and it reads what
    /// `write_frame` wrote.
    #[test]
    fn a_frame_buf_writes_and_reads_the_same_wire_as_the_free_functions() {
        let mut buf = FrameBuf::new(64, 64);
        let (mut mine, mut theirs) = (Vec::new(), Vec::new());
        buf.write(&mut mine, &(7u32, [1u8, 2, 3])).unwrap();
        write_frame(&mut theirs, &(7u32, [1u8, 2, 3])).unwrap();
        assert_eq!(mine, theirs);
        let back: (u32, [u8; 3]) = buf.read(&mut theirs.as_slice()).unwrap();
        assert_eq!(back, (7, [1, 2, 3]));
    }

    /// **A frame over the buffer's capacity is refused before its body is
    /// read, and a message over the outbound capacity writes nothing**: the
    /// buffers never grow.
    #[test]
    fn a_frame_buf_refuses_what_it_cannot_hold_and_never_grows() {
        let mut buf = FrameBuf::new(4, 4);
        // A prefix advertising 5 bytes, with no body behind it: the refusal
        // comes before a read of the body would fail.
        let mut wire: &[u8] = &5u32.to_be_bytes();
        assert_eq!(
            buf.read::<_, u8>(&mut wire).unwrap_err().to_string(),
            IpcError::from(FrameTooLarge { len: 5, limit: 4 }).to_string()
        );
        let mut sink = Vec::new();
        assert!(buf.write(&mut sink, &[0u8; 8]).is_err());
        assert!(sink.is_empty());
        // What fits still goes through after a refusal.
        buf.write(&mut sink, &1u8).unwrap();
        assert_eq!(sink, [0, 0, 0, 1, 1]);
    }

    /// **A frame is exactly one message:** a body with bytes after the message
    /// is refused, not decoded from its front. A peer whose message is longer
    /// than ours is a version skew, and reading a prefix of it would hide that.
    #[test]
    fn a_body_with_bytes_after_the_message_is_refused() {
        assert_eq!(decode_body::<u16>(&[0x82, 0x02]).unwrap(), 0x0102);
        match decode_body::<u16>(&[0x82, 0x02, 0x00]) {
            Err(IpcError::TrailingBytes { left: 1, frame: 3 }) => {}
            other => panic!("expected TrailingBytes, got {other:?}"),
        }
    }

    /// **Each lane's cap is inclusive on both halves, and a length over it
    /// names the length and that lane's cap.** The decode side refuses before
    /// any body is read; the lanes' caps differ, and a length one lane takes
    /// another refuses.
    #[test]
    fn each_lane_cap_is_inclusive_and_over_it_is_typed() {
        for cap in [
            AudioRequest::CAP,
            AudioReply::CAP,
            Request::CAP,
            ControlFrame::CAP,
        ] {
            let at = u32::try_from(cap).unwrap();
            assert_eq!(decode_prefix(at.to_be_bytes(), cap), Ok(cap));
            assert_eq!(
                decode_prefix((at + 1).to_be_bytes(), cap),
                Err(too_large(cap + 1, cap))
            );
            assert_eq!(encode_prefix(cap, cap), Ok(at.to_be_bytes()));
            assert_eq!(encode_prefix(cap + 1, cap), Err(too_large(cap + 1, cap)));
        }
    }

    /// **The caps differ by lane:** a block's request may be megabytes of
    /// automation, a block's reply is bounded by its MIDI, and the control
    /// lane holds a parameter or preset list. A reply the audio lane takes at its cap is
    /// refused one byte over, though the control lane would take it.
    #[test]
    fn the_lanes_hold_different_caps() {
        let over_audio_reply = u32::try_from(AudioReply::CAP + 1).unwrap().to_be_bytes();
        match read_frame::<_, AudioReply>(&mut over_audio_reply.as_slice()) {
            Err(IpcError::FrameTooLarge(e)) => {
                assert_eq!((e.len, e.limit), (AudioReply::CAP + 1, AudioReply::CAP));
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
        let mut control = over_audio_reply.as_slice();
        assert!(
            !matches!(
                read_frame::<_, ControlFrame>(&mut control),
                Err(IpcError::FrameTooLarge(_))
            ),
            "a length the control lane's cap holds is not refused for its size"
        );
    }
}
