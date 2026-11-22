//! System Exclusive 8 (M2-104 section 7.8): a complete message,
//! [`Sysex8Packets`] that writes it as packets, and [`Sysex8Assembler`] that
//! reads packets back into it.

use core::cell::Cell;
use core::iter::FusedIterator;

use crate::lanes::{Entry, LanePool};
use crate::{Data128, Message, MessageType, MidiGroup, Sysex8Packet, SysexStatus, Ump};

/// The data bytes one packet carries after the Stream ID: 13 of the 14 bytes
/// `# of bytes` counts.
const BYTES_PER_PACKET: usize = 13;

/// A SysEx 8 Stream ID (M2-104 §7.8): the whole byte, `0` to `255`, so every
/// `u8` is one. A receiver of `N` streams takes `0` to `N - 1`
/// ([`Sysex8StreamIds`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub struct Sysex8StreamId(u8);

// Every `u8` is one, so the serde form (the bare byte) reads back through a
// conversion that cannot fail.
#[cfg(feature = "serde")]
#[allow(
    clippy::infallible_try_from,
    reason = "no `From<u8>`: a value enters through `new`; this is the serde form"
)]
impl TryFrom<u8> for Sysex8StreamId {
    type Error = core::convert::Infallible;

    fn try_from(raw: u8) -> Result<Self, Self::Error> {
        Ok(Self(raw))
    }
}

impl Sysex8StreamId {
    /// The Stream ID `raw`.
    #[inline]
    #[must_use]
    pub const fn new(raw: u8) -> Self {
        Self(raw)
    }

    /// The Stream ID byte.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl From<Sysex8StreamId> for u8 {
    fn from(id: Sysex8StreamId) -> Self {
        id.0
    }
}

/// How many simultaneous SysEx 8 streams a Function Block takes (`Max Number
/// of SysEx 8 Streams`, M2-104 §7.1.8): the whole byte, where `0` is no
/// System Exclusive 8 at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub struct Sysex8StreamCount(u8);

// Every `u8` is one, so the serde form (the bare byte) reads back through a
// conversion that cannot fail.
#[cfg(feature = "serde")]
#[allow(
    clippy::infallible_try_from,
    reason = "no `From<u8>`: a value enters through `new`; this is the serde form"
)]
impl TryFrom<u8> for Sysex8StreamCount {
    type Error = core::convert::Infallible;

    fn try_from(raw: u8) -> Result<Self, Self::Error> {
        Ok(Self(raw))
    }
}

impl Sysex8StreamCount {
    /// The count `raw`.
    #[inline]
    #[must_use]
    pub const fn new(raw: u8) -> Self {
        Self(raw)
    }

    /// The count byte.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl From<Sysex8StreamCount> for u8 {
    fn from(count: Sysex8StreamCount) -> Self {
        count.0
    }
}

/// A received System Exclusive 8 message (section 7.8), which a
/// [`Sysex8Assembler`] hands back: the Group and Stream ID it travelled on
/// and its bytes after the Stream ID, which begin with the Manufacturer ID,
/// Device ID and Sub IDs of the message's protocol and are not interpreted
/// here.
///
/// Any byte value is data: that is the point of SysEx8. The Stream ID is the
/// byte the sender wrote. A sender has no way to write one itself: it
/// takes a [`Sysex8Stream`] from the [`Sysex8StreamIds`] of its receiver, and
/// [`Sysex8Stream::packets`] is the only public writer of SysEx 8 message packets (`Data128::Sysex8Abort`, which a decoder returns, is the one
/// variant that still takes a [`Sysex8StreamId`] of its own).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sysex8Message<'a> {
    /// The Group it travelled on.
    pub group: MidiGroup,
    /// The stream it was on: messages of different streams interleave.
    pub stream_id: Sysex8StreamId,
    /// The message's bytes after the Stream ID.
    pub payload: &'a [u8],
}

/// The Stream IDs of a SysEx 8 receiver (section 7.8, "Stream id") and which
/// of them are in use: the only source of a [`Sysex8Stream`], so the only
/// source of a Stream ID on the wire.
///
/// A receiver that declares `N` simultaneous streams (the `Max Number of
/// SysEx 8 Streams` of its Function Block Info, `2..=255`) takes the Stream
/// IDs `0` to `N - 1`. `1` is one stream, ID 0, and `0` is a receiver that
/// takes no System Exclusive 8 at all: [`open`](Self::open) never succeeds.
/// A sender that cannot discover the receiver ([`undiscovered`](Self::undiscovered))
/// sends one stream at a time, ID 0. [`open`](Self::open) hands out the lowest
/// free ID and answers `None` while all are in use, so two messages never
/// share an ID. A merger opens one stream per source it is writing, all from
/// the one `Sysex8StreamIds` of its receiver.
///
/// An ID is in use from [`open`](Self::open) until its [`Sysex8Stream`] is
/// dropped unused, or the [`Sysex8Packets`] of its message is dropped
/// finished, [aborted](Sysex8Packets::abort) or
/// [ended early](Sysex8Packets::end_early). A message dropped part-way owes
/// an abort: its ID stays in use until [`take_abort`](Self::take_abort).
///
/// The count is the Function Block's, shared by all its Groups: with the
/// receiver keying messages by Group and ID, that only uses fewer IDs than
/// allowed. The allocator is single-threaded (`Cell`, so not `Sync`); a
/// merger over several threads needs its own lock.
///
/// ```
/// use tutti_ump::{Data128, Message, Sysex8StreamCount, Sysex8StreamIds};
///
/// let ids = Sysex8StreamIds::discovered(Sysex8StreamCount::new(2));
/// let first = ids.open().unwrap();
/// let second = ids.open().unwrap();
/// assert!(ids.open().is_none());
/// let wire = |s: tutti_ump::Sysex8Stream<'_>| {
///     let ump = s.packets(tutti_ump::MidiGroup::of(0), &[1]).next().unwrap();
///     let Ok(Message::Data128(_, Data128::Sysex8(p))) = ump.decode() else { unreachable!() };
///     p.stream_id()
/// };
/// assert_eq!((wire(first).get(), wire(second).get()), (0, 1));
/// // Both messages are written: both IDs are free again.
/// assert!(ids.open().is_some());
/// ```
///
/// It is neither `Clone` nor `Copy`: a copy would duplicate which IDs are in
/// use and hand the same one out twice.
///
/// ```compile_fail,E0599
/// use tutti_ump::Sysex8StreamIds;
///
/// let ids = Sysex8StreamIds::undiscovered();
/// let copy = ids.clone();
/// ```
#[derive(Debug)]
pub struct Sysex8StreamIds {
    /// The streams the receiver takes.
    count: Sysex8StreamCount,
    /// Bit `i` is set while Stream ID `i` is in use.
    used: Cell<[u64; 4]>,
    /// The Group of the unfinished message of each Stream ID that is owed an
    /// abort; the ID stays in use until [`take_abort`](Self::take_abort).
    owed: Cell<[Option<MidiGroup>; 255]>,
}

impl Sysex8StreamIds {
    /// The Stream IDs of a receiver whose Function Block Info has not been
    /// discovered: one stream, ID 0.
    #[must_use]
    pub const fn undiscovered() -> Self {
        Self::discovered(Sysex8StreamCount::new(1))
    }

    /// The Stream IDs of a receiver that declared `max_streams` simultaneous
    /// SysEx 8 streams in its Function Block Info: `0` takes none, `1` takes
    /// one, `N` takes `N`.
    #[must_use]
    pub const fn discovered(max_streams: Sysex8StreamCount) -> Self {
        Self {
            count: max_streams,
            used: Cell::new([0; 4]),
            owed: Cell::new([None; 255]),
        }
    }

    /// How many streams the receiver takes; the Stream IDs are `0` to one
    /// less.
    #[must_use]
    pub const fn count(&self) -> Sysex8StreamCount {
        self.count
    }

    /// Take the lowest free Stream ID, or `None` while all are in use (or the
    /// receiver takes none).
    pub fn open(&self) -> Option<Sysex8Stream<'_>> {
        let used = self.used.get();
        let id = (0..self.count.get())
            .find(|&i| used[usize::from(i / 64)] & (1 << (i % 64)) == 0)
            .map(Sysex8StreamId::new)?;
        self.set(id, true);
        Some(Sysex8Stream {
            ids: self,
            id,
            keep: false,
        })
    }

    /// The abort (section 7.8.1: an End with `# of bytes` `0xF`) of the lowest
    /// Stream ID whose message was dropped part-way, on that message's Group,
    /// or `None` when none is owed. Taking it frees the ID: the caller writes
    /// what it returns. A sender drains this before [`open`](Self::open), or
    /// in a loop, because [`open`](Self::open) never hands out an ID that is
    /// owed an abort.
    #[must_use = "the abort is written to the receiver; the Stream ID is free once it is taken"]
    pub fn take_abort(&self) -> Option<Ump> {
        let mut owed = self.owed.get();
        let id = (0..self.count.get())
            .find(|&i| owed[usize::from(i)].is_some())
            .map(Sysex8StreamId::new)?;
        let group = owed[usize::from(id.get())].take()?;
        self.owed.set(owed);
        self.set(id, false);
        Some(Ump::from(Message::Data128(
            group,
            Data128::Sysex8Abort { stream_id: id },
        )))
    }

    fn set(&self, id: Sysex8StreamId, on: bool) {
        let mut used = self.used.get();
        let id = id.get();
        let (word, bit) = (usize::from(id / 64), 1u64 << (id % 64));
        if on {
            used[word] |= bit;
        } else {
            used[word] &= !bit;
        }
        self.used.set(used);
    }
}

/// One Stream ID taken from a [`Sysex8StreamIds`]: the proof that the ID is
/// one the receiver takes and that no other message is using it. It borrows
/// its allocator, and [`packets`](Self::packets) consumes it.
///
/// ```compile_fail,E0382
/// use tutti_ump::{MidiGroup, Sysex8StreamIds};
///
/// let ids = Sysex8StreamIds::undiscovered();
/// let stream = ids.open().unwrap();
/// let _first = stream.packets(MidiGroup::of(0), &[1]);
/// let _second = stream.packets(MidiGroup::of(0), &[2]);
/// ```
///
/// ```compile_fail,E0599
/// use tutti_ump::Sysex8StreamIds;
///
/// let ids = Sysex8StreamIds::undiscovered();
/// let stream = ids.open().unwrap();
/// let _copy = stream.clone();
/// ```
///
/// ```compile_fail,E0597
/// use tutti_ump::Sysex8StreamIds;
///
/// let stream = {
///     let ids = Sysex8StreamIds::undiscovered();
///     ids.open().unwrap()
/// };
/// drop(stream);
/// ```
#[derive(Debug)]
pub struct Sysex8Stream<'a> {
    ids: &'a Sysex8StreamIds,
    id: Sysex8StreamId,
    /// Set by an unfinished message: the ID is owed an abort, not free.
    keep: bool,
}

impl Drop for Sysex8Stream<'_> {
    /// An ID dropped unused, or whose message was written, is free again.
    fn drop(&mut self) {
        if !self.keep {
            self.ids.set(self.id, false);
        }
    }
}

impl<'a> Sysex8Stream<'a> {
    /// `payload` (the bytes after the Stream ID) as the packets of one
    /// message of this stream on `group`: see [`Sysex8Packets`].
    pub fn packets(self, group: MidiGroup, payload: &'a [u8]) -> Sysex8Packets<'a> {
        Sysex8Packets {
            stream: self,
            group,
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
/// The Stream ID is in use until the iterator is dropped having written every
/// packet, or [`abort`](Self::abort) or [`end_early`](Self::end_early) ends
/// the message. Dropped part-way, the message is open on the receiver: its ID
/// stays in use and is owed an abort, which
/// [`Sysex8StreamIds::take_abort`] gives (section 7.8.1).
///
/// ```
/// use tutti_ump::{Data128, Message, MidiGroup, Sysex8StreamIds, SysexStatus};
///
/// let ids = Sysex8StreamIds::undiscovered();
/// let mut packets = ids.open().unwrap().packets(MidiGroup::of(0), &[0xFF; 14]);
/// assert_eq!(packets.len(), 2);
/// let Ok(Message::Data128(_, Data128::Sysex8(first))) = packets.next().unwrap().decode() else {
///     unreachable!()
/// };
/// assert_eq!(first.status(), SysexStatus::Start);
/// ```
#[derive(Debug)]
#[must_use = "packets are written by iterating; an unfinished message keeps its Stream ID"]
pub struct Sysex8Packets<'a> {
    stream: Sysex8Stream<'a>,
    group: MidiGroup,
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
        let (packet, rest) = Sysex8Packet::take(self.started, self.stream.id, self.rest);
        self.rest = rest;
        self.started = true;
        Some(Ump::from(Message::Data128(
            self.group,
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

impl Sysex8Packets<'_> {
    /// Whether every packet has been yielded.
    fn finished(&self) -> bool {
        self.started && self.rest.is_empty()
    }

    /// End the message with the abort of section 7.8.1: an End with all data
    /// bytes zero and `# of bytes` `0xF`, for "the previous data is an
    /// incomplete message, or the resulting quality of previous data is
    /// unknown". The Stream ID is free again. Written after the packets
    /// yielded so far.
    pub fn abort(mut self) -> Ump {
        self.rest = &[];
        self.started = true;
        Ump::from(Message::Data128(
            self.group,
            Data128::Sysex8Abort {
                stream_id: self.stream.id,
            },
        ))
    }

    /// End the message early (section 7.8.1): an End with all data bytes zero
    /// and `# of bytes` `0x1`, for "the Sender knows that the previous data in
    /// the SysEx8 message is valid"; the Stream ID alone is valid and no
    /// further data is included. The Stream ID is free again. Written after
    /// the packets yielded so far.
    pub fn end_early(mut self) -> Ump {
        self.rest = &[];
        self.started = true;
        // Message type 5, status End (3), `# of bytes` 1: the Stream ID alone.
        Ump([
            0x5031_0000
                | (u32::from(self.group.get()) << 24)
                | (u32::from(self.stream.id.get()) << 8),
            0,
            0,
            0,
        ])
    }
}

impl Drop for Sysex8Packets<'_> {
    fn drop(&mut self) {
        if self.started && !self.finished() {
            let mut owed = self.stream.ids.owed.get();
            owed[usize::from(self.stream.id.get())] = Some(self.group);
            self.stream.ids.owed.set(owed);
            self.stream.keep = true;
        }
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

impl Run {
    const fn is_assembling(&self) -> bool {
        matches!(self, Self::Assembling { .. })
    }
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
/// use tutti_ump::{MidiGroup, Sysex8Assembler, Sysex8StreamIds, Sysex8Step};
///
/// let ids = Sysex8StreamIds::undiscovered();
/// let mut assembler = Sysex8Assembler::new([0u8; 64], 64, 1);
/// let mut done = 0;
/// for ump in ids.open().unwrap().packets(MidiGroup::of(0), &[0x80; 40]) {
///     if let Sysex8Step::Complete(whole) = assembler.push(ump) {
///         assert_eq!((whole.stream_id.get(), whole.payload), (0, &[0x80; 40][..]));
///         done += 1;
///     }
/// }
/// assert_eq!(done, 1);
/// ```
#[derive(Debug)]
pub struct Sysex8Assembler<B> {
    /// One lane per open stream, the lane's slot naming the stream.
    pool: LanePool<B, Entry<Sysex8StreamId, Run>>,
    /// A `Complete` packet's bytes, which have no lane.
    single: [u8; BYTES_PER_PACKET],
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> Sysex8Assembler<B> {
    /// An assembler over `buf` for messages of up to `max_payload` bytes after
    /// the Stream ID, with up to `lanes` open at once (at most sixteen). It
    /// needs `lanes * max_payload` bytes: as many lanes as `buf` holds are
    /// used.
    #[must_use]
    pub fn new(buf: B, max_payload: usize, lanes: usize) -> Self {
        Self {
            pool: LanePool::new(buf, max_payload, lanes, |e| e.run.is_assembling()),
            single: [0; BYTES_PER_PACKET],
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
        self.pool.dropped()
    }

    /// An upstream queue dropped UMPs after the ones just pushed: drop every
    /// open message, counted.
    pub fn lost_tail(&mut self) {
        self.pool.lost_tail();
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
                self.close(group, stream_id);
                Sysex8Step::Held
            }
            // Status 0 to 3 of type 5 with a `# of bytes` that is no count.
            Err(_)
                if ump.message_type() == MessageType::Data128 && (ump.0[0] >> 20) & 0xF <= 0x3 =>
            {
                Sysex8Step::Held
            }
            _ => Sysex8Step::NotSysex8,
        }
    }

    fn find(&self, group: MidiGroup, stream_id: Sysex8StreamId) -> Option<usize> {
        self.pool.find_key(group, stream_id)
    }

    fn set(&mut self, i: usize, group: MidiGroup, stream_id: Sysex8StreamId, run: Run) {
        self.pool.set_key(i, group, stream_id, run);
    }

    /// The stream's open message ends without its `End`: counted if it was
    /// being collected.
    fn close(&mut self, group: MidiGroup, stream_id: Sysex8StreamId) {
        if let Some(i) = self.find(group, stream_id) {
            self.pool.end(i);
        }
    }

    fn packet(&mut self, group: MidiGroup, p: &Sysex8Packet) -> Sysex8Step<'_> {
        let stream_id = p.stream_id();
        let bytes = p.bytes();
        match p.status() {
            SysexStatus::Complete => {
                self.close(group, stream_id);
                if bytes.len() > self.pool.lane_len() {
                    self.pool.count_drop();
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
                if let Some(i) = i {
                    self.pool.end(i);
                }
                let Some(i) = i.or_else(|| self.pool.free()) else {
                    self.pool.count_drop();
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
                        }) = self.pool.slot(i)
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
                    self.pool.count_drop();
                    return Sysex8Step::Held;
                };
                match self.pool.slot(i) {
                    Some(Entry {
                        run: Run::Assembling { len },
                        ..
                    }) => {
                        self.extend(i, group, stream_id, len, bytes);
                        let Some(Entry {
                            run: Run::Assembling { len },
                            ..
                        }) = self.pool.slot(i)
                        else {
                            self.pool.clear(i);
                            return Sysex8Step::Held;
                        };
                        self.pool.clear(i);
                        Sysex8Step::Complete(Sysex8Message {
                            group,
                            stream_id,
                            payload: &self.pool.lane(i)[..len],
                        })
                    }
                    _ => {
                        self.pool.clear(i);
                        Sysex8Step::Held
                    }
                }
            }
        }
    }

    /// A `Continue` with no message open: counted, and its stream skipped to
    /// the `End` if a lane is free to remember it.
    fn orphan(&mut self, group: MidiGroup, stream_id: Sysex8StreamId) {
        self.pool.count_drop();
        if let Some(i) = self.pool.free() {
            self.set(i, group, stream_id, Run::Discarding);
        }
    }

    /// Append `bytes` to the message of `len` bytes in lane `i`, or drop it
    /// whole past capacity.
    fn extend(
        &mut self,
        i: usize,
        group: MidiGroup,
        stream_id: Sysex8StreamId,
        len: usize,
        bytes: &[u8],
    ) {
        if len + bytes.len() > self.pool.lane_len() {
            self.pool.count_drop();
            self.set(i, group, stream_id, Run::Discarding);
            return;
        }
        self.pool.lane_mut(i)[len..len + bytes.len()].copy_from_slice(bytes);
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
