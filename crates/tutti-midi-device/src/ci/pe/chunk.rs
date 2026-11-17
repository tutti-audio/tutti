//! The chunks of a Property Exchange message (M2-101 §8.3, M2-103 §5.2): the
//! numbers that label them, the chunk itself, and the split of one message into
//! chunks that fit a device's Receivable Maximum SysEx Message Size.

use super::header::{MESSAGE_OVERHEAD, PE_MAX_HEADER_LEN, PeHeader, PeHeaderTooLong, PeStatus};
use crate::MaxSysexSize;
use tutti_ump::{U14, first_non_seven_bit};

/// The most bytes the Property Data of one chunk holds, 16383: its length is a
/// 14-bit value (M2-101 Tables 33 to 35).
pub const PE_MAX_CHUNK_DATA_LEN: usize = 0x3FFF;

tutti_types::ranged! {
    /// A Request ID: 0 to 127 (M2-103 §5.3). It ties the chunks of a message
    /// together and a reply to its inquiry, and is unique among the
    /// transactions between one Initiator and one Responder.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    PeRequestId(u8), NotARequestId, "a request id (0 to 127)", |raw| raw <= 127
}

tutti_types::ranged! {
    /// The number of a chunk, or a count of chunks: 1 to 16383, since the
    /// count starts from 1 and is a 14-bit value (M2-101 §8.3, Table 33).
    #[derive(Eq, Hash, PartialOrd, Ord)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u16", into = "u16"))]
    ChunkNo(u16), NotAChunkNo, "a chunk number (1 to 16383)", |raw| raw >= 1 && raw <= 0x3FFF
}

impl ChunkNo {
    /// The two 7-bit wire bytes, least significant first.
    #[must_use]
    pub const fn to_le7(self) -> [u8; 2] {
        // The range holds 14 bits, so nothing is masked off.
        U14::masked(self.get()).to_le7()
    }

    /// The largest chunk number, 16383.
    pub const MAX: u16 = 0x3FFF;

    /// The first chunk.
    pub const FIRST: Self = Self::of(1);

    /// The number after this one, or `None` past the 14-bit limit.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        Self::new(self.get() + 1)
    }
}

/// The Number of Chunks in Message (M2-101 §8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChunkTotal {
    /// The wire value 0: the sender does not know the count in advance, and
    /// the final chunk declares it.
    Unknown,
    /// The count.
    Known(ChunkNo),
}

impl ChunkTotal {
    /// The two 7-bit wire bytes, least significant first.
    #[must_use]
    pub const fn to_le7(self) -> [u8; 2] {
        match self {
            Self::Unknown => [0, 0],
            Self::Known(n) => n.to_le7(),
        }
    }

    /// The wire value.
    #[must_use]
    pub const fn wire(self) -> u16 {
        match self {
            Self::Unknown => 0,
            Self::Known(n) => n.get(),
        }
    }

    /// The total a wire value names: 0 is [`Unknown`](Self::Unknown).
    #[must_use]
    pub const fn from_wire(raw: u16) -> Option<Self> {
        if raw == 0 {
            Some(Self::Unknown)
        } else {
            match ChunkNo::new(raw) {
                Some(n) => Some(Self::Known(n)),
                None => None,
            }
        }
    }
}

/// The Number of This Chunk (M2-101 §8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChunkPosition {
    /// The place of the chunk in the message, counting from 1.
    Number(ChunkNo),
    /// The wire value 0, on a final chunk: the sender cannot say whether the
    /// data it sent is complete or usable, so the message is abandoned.
    Abandoned,
}

impl ChunkPosition {
    /// The two 7-bit wire bytes, least significant first.
    #[must_use]
    pub const fn to_le7(self) -> [u8; 2] {
        match self {
            Self::Abandoned => [0, 0],
            Self::Number(n) => n.to_le7(),
        }
    }

    /// The wire value.
    #[must_use]
    pub const fn wire(self) -> u16 {
        match self {
            Self::Abandoned => 0,
            Self::Number(n) => n.get(),
        }
    }

    /// The position a wire value names: 0 is [`Abandoned`](Self::Abandoned).
    #[must_use]
    pub const fn from_wire(raw: u16) -> Option<Self> {
        if raw == 0 {
            Some(Self::Abandoned)
        } else {
            match ChunkNo::new(raw) {
                Some(n) => Some(Self::Number(n)),
                None => None,
            }
        }
    }
}

/// A chunk that cannot be, or a message that cannot be split.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PeChunkError {
    /// Header Data in a chunk other than the first (M2-101 §8.3.1).
    #[error("only the first chunk carries Header Data")]
    HeaderNotInFirst,
    /// More Header Data than its 14-bit length holds.
    #[error("the header is {len} bytes and a chunk holds at most {PE_MAX_HEADER_LEN}")]
    HeaderTooLong {
        /// Its length.
        len: usize,
    },
    /// More Property Data than its 14-bit length holds.
    #[error("the data is {len} bytes and a chunk holds at most {PE_MAX_CHUNK_DATA_LEN}")]
    DataTooLong {
        /// Its length.
        len: usize,
    },
    /// A byte with its top bit set, which SysEx data cannot carry.
    #[error("byte {at} has its top bit set")]
    NotSevenBit {
        /// The offset of the byte in the field it is in.
        at: usize,
    },
    /// The header does not fit the Receivable Maximum SysEx Message Size.
    #[error(transparent)]
    Header(#[from] PeHeaderTooLong),
    /// The message needs more chunks than the 14-bit count holds.
    #[error(
        "the message needs {needed} chunks and a count holds at most {}",
        ChunkNo::MAX
    )]
    TooManyChunks {
        /// The chunks it needs.
        needed: usize,
    },
    /// The bytes end before a field does, or run on after the last.
    #[error("the chunk's fields do not fill its bytes")]
    Length,
}

/// One SysEx message of a Property Exchange message: the fields from the
/// Request ID to the Property Data (M2-101 Tables 33 to 35).
///
/// Its fields are private: a chunk carries Header Data only in the first
/// (§8.3.1), and a field is at most 14 bits long, and each constructor holds both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeChunk {
    request_id: PeRequestId,
    total: ChunkTotal,
    position: ChunkPosition,
    header: Vec<u8>,
    header_len: U14,
    data: Vec<u8>,
    data_len: U14,
}

fn seven_bit(bytes: &[u8]) -> Result<(), PeChunkError> {
    match first_non_seven_bit(bytes) {
        Some(at) => Err(PeChunkError::NotSevenBit { at }),
        None => Ok(()),
    }
}

impl TryFrom<&[u8]> for PeChunk {
    type Error = PeChunkError;

    /// Reads the fields [`encode_fields`](Self::encode_fields) writes.
    ///
    /// # Errors
    ///
    /// [`PeChunkError::Length`] when the declared lengths do not fill `bytes`,
    /// and those of [`new`](Self::new), including for a Request ID or a count
    /// past 7 bits.
    fn try_from(bytes: &[u8]) -> Result<Self, PeChunkError> {
        let mut cursor = Cursor(bytes);
        let request_id = cursor.byte()?;
        let header_len = cursor.pair()?;
        let header = cursor.take(header_len.into())?.to_vec();
        let total = cursor.pair()?;
        let position = cursor.pair()?;
        let data_len = cursor.pair()?;
        let data = cursor.take(data_len.into())?.to_vec();
        if !cursor.0.is_empty() {
            return Err(PeChunkError::Length);
        }
        Self::new(
            PeRequestId::new(request_id).ok_or(PeChunkError::NotSevenBit { at: 0 })?,
            ChunkTotal::from_wire(total).ok_or(PeChunkError::Length)?,
            ChunkPosition::from_wire(position).ok_or(PeChunkError::Length)?,
            header,
            data,
        )
    }
}

impl PeChunk {
    /// A chunk.
    ///
    /// # Errors
    ///
    /// [`PeChunkError::HeaderNotInFirst`] for a `header` that is not empty
    /// unless `position` is chunk 1, [`PeChunkError::HeaderTooLong`] and
    /// [`PeChunkError::DataTooLong`] for a field past its 14-bit length, and
    /// [`PeChunkError::NotSevenBit`] for a byte with its top bit set.
    pub fn new(
        request_id: PeRequestId,
        total: ChunkTotal,
        position: ChunkPosition,
        header: Vec<u8>,
        data: Vec<u8>,
    ) -> Result<Self, PeChunkError> {
        if !header.is_empty() && position != ChunkPosition::Number(ChunkNo::FIRST) {
            return Err(PeChunkError::HeaderNotInFirst);
        }
        let header_len = U14::from_len(header.len())
            .filter(|len| usize::from(len.get()) <= PE_MAX_HEADER_LEN)
            .ok_or(PeChunkError::HeaderTooLong { len: header.len() })?;
        let data_len = U14::from_len(data.len())
            .filter(|len| usize::from(len.get()) <= PE_MAX_CHUNK_DATA_LEN)
            .ok_or(PeChunkError::DataTooLong { len: data.len() })?;
        seven_bit(&header)?;
        seven_bit(&data)?;
        Ok(Self {
            request_id,
            total,
            position,
            header,
            header_len,
            data,
            data_len,
        })
    }

    /// The fields of a Notify message for `request_id` (M2-101 Table 39): Header
    /// Data of the one property `status`, one chunk of one, no Property Data.
    /// [`PeCiMessage::Notify`](super::PeCiMessage::Notify) carries it.
    #[must_use]
    pub fn notify(request_id: PeRequestId, status: PeStatus) -> Self {
        let header = format!("{{\"status\":{}}}", status.code()).into_bytes();
        Self {
            // `{"status":` and a status code are a few bytes.
            header_len: U14::masked(header.len() as u16),
            request_id,
            total: ChunkTotal::Known(ChunkNo::FIRST),
            position: ChunkPosition::Number(ChunkNo::FIRST),
            header,
            data: Vec::new(),
            data_len: U14::of(0),
        }
    }

    /// The status this chunk's Header Data gives, as a Notify message carries it,
    /// or `None` when the Header Data is not a header with a status the spec
    /// lists.
    #[must_use]
    pub fn notified(&self) -> Option<PeStatus> {
        match PeHeader::try_from(self.header.as_slice()) {
            Ok(PeHeader::Reply(reply)) => Some(reply.status),
            _ => None,
        }
    }

    /// The Request ID.
    #[must_use]
    pub const fn request_id(&self) -> PeRequestId {
        self.request_id
    }

    /// The Number of Chunks in Message.
    #[must_use]
    pub const fn total(&self) -> ChunkTotal {
        self.total
    }

    /// The Number of This Chunk.
    #[must_use]
    pub const fn position(&self) -> ChunkPosition {
        self.position
    }

    /// The Header Data: empty unless this is chunk 1.
    #[must_use]
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    /// The Property Data of this chunk.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The bytes of the whole SysEx message, `F0` and `F7` counted: what has to
    /// fit the Receivable Maximum SysEx Message Size.
    #[must_use]
    pub fn sysex_len(&self) -> usize {
        MESSAGE_OVERHEAD + self.header.len() + self.data.len()
    }

    /// The fields from the Request ID to the Property Data, as they follow the
    /// destination MUID.
    #[must_use]
    pub fn encode_fields(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.sysex_len() - MESSAGE_OVERHEAD + 9);
        out.push(self.request_id.get());
        out.extend(self.header_len.to_le7());
        out.extend(&self.header);
        out.extend(self.total.to_le7());
        out.extend(self.position.to_le7());
        out.extend(self.data_len.to_le7());
        out.extend(&self.data);
        out
    }
}

/// The unread bytes of a chunk.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PeChunkError> {
        if self.0.len() < n {
            return Err(PeChunkError::Length);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn byte(&mut self) -> Result<u8, PeChunkError> {
        Ok(self.take(1)?[0])
    }

    /// A 14-bit value of two bytes, least significant first.
    fn pair(&mut self) -> Result<u16, PeChunkError> {
        let two = self.take(2)?;
        U14::from_le7([two[0], two[1]])
            .map(U14::get)
            .ok_or(PeChunkError::NotSevenBit { at: 0 })
    }
}

/// Splits one Property Exchange message into the chunks that carry it.
///
/// `header` goes in chunk 1 only (M2-101 §8.3.1) and `body` is the Property
/// Data, already in its [`PeEncoding`](super::PeEncoding). Every chunk is a SysEx
/// message of at most `max` bytes, and holds at most [`PE_MAX_CHUNK_DATA_LEN`]
/// bytes of data. A message with no Property Data is one chunk, numbered 1 of 1
/// (§8.3.2); any other counts its chunks in each, so the Number of Chunks in
/// Message is never the unknown 0 and the last chunk's number equals it.
///
/// # Errors
///
/// [`PeChunkError::Header`] when the header alone does not fit `max`,
/// [`PeChunkError::NotSevenBit`] for a body byte with its top bit set and
/// [`PeChunkError::TooManyChunks`] for a body past 16383 chunks.
pub fn split_message(
    request_id: PeRequestId,
    header: &PeHeader,
    body: &[u8],
    max: MaxSysexSize,
) -> Result<Vec<PeChunk>, PeChunkError> {
    seven_bit(body)?;
    let header = header.encode(max)?;
    let room = (max.get() as usize).saturating_sub(MESSAGE_OVERHEAD);
    let first_room = (room - header.len()).min(PE_MAX_CHUNK_DATA_LEN);
    let next_room = room.min(PE_MAX_CHUNK_DATA_LEN);
    let first_len = first_room.min(body.len());
    let (first, rest) = body.split_at(first_len);
    let needed = 1 + rest.len().div_ceil(next_room);
    let count = u16::try_from(needed)
        .ok()
        .and_then(ChunkNo::new)
        .ok_or(PeChunkError::TooManyChunks { needed })?;
    let total = ChunkTotal::Known(count);
    let mut chunks = Vec::with_capacity(needed);
    chunks.push(PeChunk::new(
        request_id,
        total,
        ChunkPosition::Number(ChunkNo::FIRST),
        header,
        first.to_vec(),
    )?);
    let mut number = ChunkNo::FIRST;
    for data in rest.chunks(next_room) {
        number = number
            .next()
            .ok_or(PeChunkError::TooManyChunks { needed })?;
        chunks.push(PeChunk::new(
            request_id,
            total,
            ChunkPosition::Number(number),
            Vec::new(),
            data.to_vec(),
        )?);
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests;
