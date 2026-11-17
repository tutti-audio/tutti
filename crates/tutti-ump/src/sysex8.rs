//! System Exclusive 8 (M2-104 section 7.8): a complete message,
//! [`Sysex8Packets`] that writes it as packets, and [`Sysex8Assembler`] that
//! reads packets back into it.

use core::iter::FusedIterator;

use crate::{Data128, Message, MidiGroup, Sysex8Packet, SysexStatus, Ump};

/// The data bytes one packet carries after the Stream ID: 13 of the 14 bytes
/// `# of bytes` counts.
const BYTES_PER_PACKET: usize = 13;
/// The most Groups a UMP stream has; also the most messages open at once, one
/// per lane.
const LANES: usize = 16;

/// A complete System Exclusive 8 message (section 7.8) a
/// [`Sysex8Assembler`] hands back: the Group and Stream ID it travelled on
/// and its bytes after the Stream ID, which begin with the Manufacturer ID,
/// Device ID and Sub IDs of the message's protocol and are not interpreted
/// here.
///
/// Any byte value is data: that is the point of SysEx8. The Stream ID is the
/// plain byte the sender wrote. A sender has no way to write one itself: it
/// takes a [`Sysex8Stream`] from the [`Sysex8Streams`] of its receiver and
/// writes with [`Sysex8Stream::packets`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sysex8Message<'a> {
    /// The Group it travelled on.
    pub group: MidiGroup,
    /// The stream it was on: messages of different streams interleave.
    pub stream_id: u8,
    /// The message's bytes after the Stream ID.
    pub payload: &'a [u8],
}

/// The SysEx 8 streams a receiver accepts (section 7.8, "Stream id") and
/// which of them a sender has open: the only source of a [`Sysex8Stream`], so
/// the only source of a Stream ID on the wire.
///
/// A receiver that declares `N` simultaneous streams (the `Max Number of
/// SysEx8 Streams` of its Function Block Info, `2..=255`) takes the Stream
/// IDs `0` to `N - 1`. A receiver that declares `0` or `1`, or that has not
/// been discovered, takes one stream, and only Stream ID 0: a sender that has
/// not discovered more sends one stream at a time. [`open`](Self::open) hands
/// out the lowest free ID of `0..N` and answers `None` while all `N` are
/// open, so a message is never interleaved with another of the same stream.
/// One `Sysex8Streams` per receiver serves a merger: every source it merges
/// opens its stream here.
///
/// ```
/// use tutti_ump::Sysex8Streams;
///
/// let mut streams = Sysex8Streams::discovered(2);
/// let first = streams.open().unwrap();
/// let second = streams.open().unwrap();
/// assert_eq!((first.id(), second.id()), (0, 1));
/// assert!(streams.open().is_none());
/// streams.close(first);
/// assert_eq!(streams.open().unwrap().id(), 0);
/// ```
///
/// It is neither `Clone` nor `Copy`: a copy would duplicate which IDs are
/// open and hand the same one out twice. Share it as `&mut`.
///
/// ```compile_fail
/// use tutti_ump::Sysex8Streams;
///
/// let streams = Sysex8Streams::undiscovered();
/// let mut copy = streams.clone();
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct Sysex8Streams {
    /// The streams the receiver takes, at least one.
    count: u8,
    /// Bit `i` is set while Stream ID `i` is open.
    open: [u64; 4],
}

impl Sysex8Streams {
    /// The streams of a receiver whose Function Block Info has not been
    /// discovered: one, Stream ID 0.
    #[must_use]
    pub const fn undiscovered() -> Self {
        Self::discovered(0)
    }

    /// The streams of a receiver that declared `max_streams` simultaneous
    /// SysEx 8 streams (its Function Block Info): `0` and `1` are one stream.
    #[must_use]
    pub const fn discovered(max_streams: u8) -> Self {
        Self {
            count: if max_streams == 0 { 1 } else { max_streams },
            open: [0; 4],
        }
    }

    /// How many streams the receiver takes; the Stream IDs are `0` to one
    /// less.
    #[must_use]
    pub const fn count(&self) -> u8 {
        self.count
    }

    /// Open the lowest free stream, or `None` while all are open.
    pub fn open(&mut self) -> Option<Sysex8Stream> {
        let id =
            (0..usize::from(self.count)).find(|&i| self.open[i / 64] & (1 << (i % 64)) == 0)?;
        self.open[id / 64] |= 1 << (id % 64);
        Some(Sysex8Stream {
            id: u8::try_from(id).ok()?,
        })
    }

    /// Close a stream whose message is written, so [`open`](Self::open) can
    /// hand its ID out again.
    pub fn close(&mut self, stream: Sysex8Stream) {
        let id = usize::from(stream.id);
        self.open[id / 64] &= !(1 << (id % 64));
    }
}

/// One open SysEx 8 stream of a [`Sysex8Streams`]: the proof that its Stream
/// ID is one the receiver takes, and that no other message is using it.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "an open stream is held until `Sysex8Streams::close`"]
pub struct Sysex8Stream {
    id: u8,
}

impl Sysex8Stream {
    /// The Stream ID it writes.
    #[must_use]
    pub const fn id(&self) -> u8 {
        self.id
    }

    /// `payload` (the bytes after the Stream ID) as packets of this stream on
    /// `group`: see [`Sysex8Packets`].
    #[must_use]
    pub fn packets<'a>(&self, group: MidiGroup, payload: &'a [u8]) -> Sysex8Packets<'a> {
        Sysex8Packets {
            message: Sysex8Message {
                group,
                stream_id: self.id,
                payload,
            },
            rest: payload,
            started: false,
        }
    }
}

/// The packets of one SysEx 8 message (section 7.8), from
/// [`Sysex8Stream::packets`].
///
/// A payload of up to 13 bytes is one `Complete` packet (an empty one is a
/// `Complete` that holds the Stream ID alone, `# of bytes` 1). A longer one is
/// a `Start`, any number of `Continue`, then an `End`, each carrying 13 bytes
/// but the last, which carries one to 13. The packets carry no timing and
/// nothing is allocated. [`Sysex8Assembler`] is the inverse.
///
/// ```
/// use tutti_ump::{Data128, Message, MidiGroup, Sysex8Streams, SysexStatus};
///
/// let stream = Sysex8Streams::undiscovered().open().unwrap();
/// let mut packets = stream.packets(MidiGroup::of(0), &[0xFF; 14]);
/// assert_eq!(packets.len(), 2);
/// let Ok(Message::Data128(_, Data128::Sysex8(first))) = packets.next().unwrap().decode() else {
///     unreachable!()
/// };
/// assert_eq!(first.status(), SysexStatus::Start);
/// ```
#[derive(Clone, Debug)]
pub struct Sysex8Packets<'a> {
    message: Sysex8Message<'a>,
    rest: &'a [u8],
    /// Whether a packet has been yielded, so an empty payload is one packet.
    started: bool,
}

impl Iterator for Sysex8Packets<'_> {
    type Item = Ump;

    fn next(&mut self) -> Option<Ump> {
        if self.started && self.rest.is_empty() {
            return None;
        }
        let status = match (self.started, self.rest.len() > BYTES_PER_PACKET) {
            (false, false) => SysexStatus::Complete,
            (false, true) => SysexStatus::Start,
            (true, true) => SysexStatus::Continue,
            (true, false) => SysexStatus::End,
        };
        let (chunk, rest) = self.rest.split_at(self.rest.len().min(BYTES_PER_PACKET));
        self.rest = rest;
        self.started = true;
        let packet =
            Sysex8Packet::new(status, self.message.stream_id, chunk).expect("at most 13 bytes");
        Some(Ump::from(Message::Data128(
            self.message.group,
            Data128::Sysex8(packet),
        )))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match (self.started, self.rest.len()) {
            (true, 0) => 0,
            (_, len) => len.div_ceil(BYTES_PER_PACKET).max(1),
        };
        (n, Some(n))
    }
}

impl ExactSizeIterator for Sysex8Packets<'_> {}
impl FusedIterator for Sysex8Packets<'_> {}

/// What one UMP did to a [`Sysex8Assembler`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sysex8Step<'a> {
    /// Not a System Exclusive 8 UMP: for the caller to read as usual.
    NotSysex8,
    /// A System Exclusive 8 UMP, taken in with no message to hand on yet: a
    /// part of one, a part of one that was dropped (and counted), or a UMP
    /// that is not a well-formed packet.
    Held,
    /// A message completed.
    Complete(Sysex8Message<'a>),
}

#[derive(Clone, Copy, Debug)]
enum Run {
    /// A message being collected: `len` bytes so far in the entry's lane.
    Assembling { len: usize },
    /// A message dropped and counted: its packets are skipped up to its `End`.
    Discarding,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    group: MidiGroup,
    stream_id: u8,
    run: Run,
}

/// Reassembles System Exclusive 8 packets (section 7.8) that
/// [`Sysex8Packets`] writes into whole messages, in a buffer the caller owns
/// and that is never grown. It allocates nothing.
///
/// A consumer feeds every UMP of its stream to [`push`](Self::push). The
/// assembler reads the System Exclusive 8 packets and answers
/// [`Sysex8Step::NotSysex8`] to the rest (a Mixed Data Set UMP included). A
/// completed message borrows the assembler until the next `push`.
///
/// # Rules (section 7.8 and 7.8.1)
///
/// - **A message is a `Complete` packet, or a `Start`, any number of
///   `Continue` and an `End`,** all of one Group and one Stream ID. Streams
///   of one Group, and Groups, interleave freely, and so do other UMPs: the
///   section 7.8.1 note lifts the prohibition SysEx7 has, so no other UMP
///   ends a message.
/// - **A message whose stream sees a `Start` or a `Complete` before its `End`
///   is dropped, counted,** and the new packet is read as it stands.
/// - **A `Continue` or `End` with no open message on its stream is dropped,
///   counted once:** a `Continue` skips the stream's packets up to the next
///   `End`.
/// - **An `End` with `# of bytes` 0xF aborts the stream's message** (section
///   7.8.1): the open message is dropped, counted. An `End` whose `# of
///   bytes` is 1 is a normal one with no data.
/// - **A packet whose `# of bytes` is 0, or 0xF outside an `End`,** is not a
///   packet (section 7.8): it is [`Sysex8Step::Held`] and has no other
///   effect.
/// - **Over capacity the whole message is dropped,** never cut: more than
///   `max_payload` bytes, or no free lane. Its remaining packets are skipped
///   up to its `End`.
///
/// At most `lanes` messages are open at once (at most sixteen); a dropped
/// message that is still being skipped holds one too. When all are held, a
/// packet of a message with no lane is counted on its own, so
/// [`dropped`](Self::dropped) can exceed the messages lost.
///
/// ```
/// use tutti_ump::{MidiGroup, Sysex8Assembler, Sysex8Streams, Sysex8Step};
///
/// let stream = Sysex8Streams::undiscovered().open().unwrap();
/// let mut assembler = Sysex8Assembler::new([0u8; 64], 64, 1);
/// let mut done = 0;
/// for ump in stream.packets(MidiGroup::of(0), &[0x80; 40]) {
///     if let Sysex8Step::Complete(whole) = assembler.push(ump) {
///         assert_eq!((whole.stream_id, whole.payload), (0, &[0x80; 40][..]));
///         done += 1;
///     }
/// }
/// assert_eq!(done, 1);
/// ```
#[derive(Debug)]
pub struct Sysex8Assembler<B> {
    buf: B,
    lanes: usize,
    max_payload: usize,
    entries: [Option<Entry>; LANES],
    /// A `Complete` packet's bytes, which have no lane.
    single: [u8; BYTES_PER_PACKET],
    dropped: u64,
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> Sysex8Assembler<B> {
    /// An assembler over `buf` for messages of up to `max_payload` bytes after
    /// the Stream ID, with up to `lanes` open at once (at most sixteen). It
    /// needs `lanes * max_payload` bytes: as many lanes as `buf` holds are
    /// used.
    #[must_use]
    pub fn new(buf: B, max_payload: usize, lanes: usize) -> Self {
        let fit = buf.as_ref().len().checked_div(max_payload).unwrap_or(0);
        Self {
            buf,
            lanes: lanes.min(LANES).min(fit),
            max_payload,
            entries: [None; LANES],
            single: [0; BYTES_PER_PACKET],
            dropped: 0,
        }
    }

    /// The buffer length that holds `lanes` open messages of `max_payload`
    /// bytes.
    #[must_use]
    pub const fn room_for(max_payload: usize, lanes: usize) -> usize {
        max_payload * lanes
    }

    /// Messages dropped whole since this assembler was made: over capacity,
    /// interrupted by a new `Start` or `Complete` on their stream, aborted
    /// (section 7.8.1), packets with no `Start`, cut by
    /// [`lost_tail`](Self::lost_tail), open at a [`reset`](Self::reset), or
    /// found no lane.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// An upstream queue dropped UMPs after the ones just pushed: drop every
    /// open message, counted.
    pub fn lost_tail(&mut self) {
        for entry in &mut self.entries {
            if let Some(Entry {
                run: Run::Assembling { .. },
                ..
            }) = entry
            {
                self.dropped += 1;
            }
            *entry = None;
        }
    }

    /// The consumer was reset: drop the open messages (counted).
    pub fn reset(&mut self) {
        self.lost_tail();
    }

    /// Take in one UMP of the stream, whatever it is. Never allocates.
    pub fn push(&mut self, ump: Ump) -> Sysex8Step<'_> {
        match ump.decode() {
            Ok(Message::Data128(group, Data128::Sysex8(p))) => self.packet(group, &p),
            Ok(Message::Data128(group, Data128::Sysex8Abort { stream_id })) => {
                self.abort(group, stream_id);
                Sysex8Step::Held
            }
            // Status 0 to 3 of type 5 with a `# of bytes` that is no count.
            Err(_) if ump.message_type() == 0x5 && (ump.0[0] >> 20) & 0xF <= 0x3 => {
                Sysex8Step::Held
            }
            _ => Sysex8Step::NotSysex8,
        }
    }

    fn find(&self, group: MidiGroup, stream_id: u8) -> Option<usize> {
        (0..self.lanes).find(
            |&i| matches!(self.entries[i], Some(e) if e.group == group && e.stream_id == stream_id),
        )
    }

    fn free(&self) -> Option<usize> {
        (0..self.lanes).find(|&i| self.entries[i].is_none())
    }

    fn lane(&mut self, i: usize) -> &mut [u8] {
        &mut self.buf.as_mut()[i * self.max_payload..][..self.max_payload]
    }

    fn set(&mut self, i: usize, group: MidiGroup, stream_id: u8, run: Run) {
        self.entries[i] = Some(Entry {
            group,
            stream_id,
            run,
        });
    }

    /// The stream's open message ends without its `End`: counted if it was
    /// being collected.
    fn close(&mut self, group: MidiGroup, stream_id: u8) {
        if let Some(i) = self.find(group, stream_id) {
            if matches!(
                self.entries[i],
                Some(Entry {
                    run: Run::Assembling { .. },
                    ..
                })
            ) {
                self.dropped += 1;
            }
            self.entries[i] = None;
        }
    }

    fn abort(&mut self, group: MidiGroup, stream_id: u8) {
        self.close(group, stream_id);
    }

    fn packet(&mut self, group: MidiGroup, p: &Sysex8Packet) -> Sysex8Step<'_> {
        let stream_id = p.stream_id();
        let bytes = p.bytes();
        match p.status() {
            SysexStatus::Complete => {
                self.close(group, stream_id);
                if bytes.len() > self.max_payload {
                    self.dropped += 1;
                    return Sysex8Step::Held;
                }
                self.single[..bytes.len()].copy_from_slice(bytes);
                Sysex8Step::Complete(Sysex8Message {
                    group,
                    stream_id,
                    payload: &self.single[..bytes.len()],
                })
            }
            SysexStatus::Start => {
                let i = self.find(group, stream_id);
                if let Some(i) = i
                    && matches!(
                        self.entries[i],
                        Some(Entry {
                            run: Run::Assembling { .. },
                            ..
                        })
                    )
                {
                    self.dropped += 1;
                }
                let Some(i) = i.or_else(|| self.free()) else {
                    self.dropped += 1;
                    return Sysex8Step::Held;
                };
                self.set(i, group, stream_id, Run::Assembling { len: 0 });
                self.extend(i, group, stream_id, 0, bytes);
                Sysex8Step::Held
            }
            SysexStatus::Continue => {
                match self.find(group, stream_id) {
                    Some(i) => {
                        if let Some(Entry {
                            run: Run::Assembling { len },
                            ..
                        }) = self.entries[i]
                        {
                            self.extend(i, group, stream_id, len, bytes);
                        }
                    }
                    None => self.orphan(group, stream_id),
                }
                Sysex8Step::Held
            }
            SysexStatus::End => {
                let Some(i) = self.find(group, stream_id) else {
                    self.dropped += 1;
                    return Sysex8Step::Held;
                };
                match self.entries[i] {
                    Some(Entry {
                        run: Run::Assembling { len },
                        ..
                    }) => {
                        self.extend(i, group, stream_id, len, bytes);
                        let Some(Entry {
                            run: Run::Assembling { len },
                            ..
                        }) = self.entries[i]
                        else {
                            self.entries[i] = None;
                            return Sysex8Step::Held;
                        };
                        self.entries[i] = None;
                        Sysex8Step::Complete(Sysex8Message {
                            group,
                            stream_id,
                            payload: &self.lane(i)[..len],
                        })
                    }
                    _ => {
                        self.entries[i] = None;
                        Sysex8Step::Held
                    }
                }
            }
        }
    }

    /// A `Continue` with no message open: counted, and its stream skipped to
    /// the `End` if a lane is free to remember it.
    fn orphan(&mut self, group: MidiGroup, stream_id: u8) {
        self.dropped += 1;
        if let Some(i) = self.free() {
            self.set(i, group, stream_id, Run::Discarding);
        }
    }

    /// Append `bytes` to the message of `len` bytes in lane `i`, or drop it
    /// whole past capacity.
    fn extend(&mut self, i: usize, group: MidiGroup, stream_id: u8, len: usize, bytes: &[u8]) {
        if len + bytes.len() > self.max_payload {
            self.dropped += 1;
            self.set(i, group, stream_id, Run::Discarding);
            return;
        }
        self.lane(i)[len..len + bytes.len()].copy_from_slice(bytes);
        self.set(
            i,
            group,
            stream_id,
            Run::Assembling {
                len: len + bytes.len(),
            },
        );
    }
}
