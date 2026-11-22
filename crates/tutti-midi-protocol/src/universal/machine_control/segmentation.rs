//! Segmentation (RP-013 p.5, COMMAND SEGMENT p.40, RESPONSE SEGMENT p.73): a
//! string of commands or fields longer than one message goes out as
//! [`MmcSegment`]s, and arrives through an [`MmcReassembler`].

use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use tutti_ump::first_non_seven_bit;

use super::field::{MmcSegment, MmcSegmentCount, MmcSegmentPlace};
use super::values::{MMC_SEGMENT_BYTES, MmcSegmentBytes};
use super::{MmcCommand, MmcInfoField, MmcItem};

/// The most segments a string has: the identification byte counts down from 63.
pub const MMC_MAX_SEGMENTS: usize = 64;

/// The longest string that goes in segments.
pub const MMC_MAX_SEGMENTED: usize = MMC_MAX_SEGMENTS * MMC_SEGMENT_BYTES;

/// A string that cannot go in segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum MmcSegmentsRefused {
    /// More than [`MMC_MAX_SEGMENTED`] bytes.
    #[error("a segmented string holds at most {MMC_MAX_SEGMENTED} bytes")]
    TooLong,
    /// A byte above 127, which no data byte is.
    #[error("a segmented string holds data bytes, each below 128")]
    NotSevenBit,
}

/// The segments of one string, first to last: each [`MmcSegment`] names its
/// place and how many segments follow it.
///
/// ```
/// use tutti_midi_protocol::{MmcCommand, MmcCommands, MmcSegments, Recipient};
///
/// // 100 bytes of PLAY: 45, 45 and 10 of them, counting down from 2.
/// let string = [0x02; 100];
/// let mut ids = Vec::new();
/// for segment in MmcSegments::new(&string).unwrap() {
///     let mut commands = MmcCommands::new();
///     commands.push(&MmcCommand::CommandSegment(segment)).unwrap();
///     ids.push(commands.payload(Recipient::AllCall)[5]);
/// }
/// assert_eq!(ids, [0x42, 0x01, 0x00]);
/// ```
#[derive(Clone, Debug)]
pub struct MmcSegments<'a> {
    rest: &'a [u8],
    /// The segments still to yield, this one included.
    left: usize,
    first: bool,
}

impl<'a> MmcSegments<'a> {
    /// The segments of `string`: [`MMC_SEGMENT_BYTES`] bytes each, the last
    /// holding the rest. An empty string has none.
    ///
    /// # Errors
    ///
    /// [`MmcSegmentsRefused`] for a string over [`MMC_MAX_SEGMENTED`] bytes or
    /// holding a byte above 127.
    pub fn new(string: &'a [u8]) -> Result<Self, MmcSegmentsRefused> {
        if string.len() > MMC_MAX_SEGMENTED {
            return Err(MmcSegmentsRefused::TooLong);
        }
        if first_non_seven_bit(string).is_some() {
            return Err(MmcSegmentsRefused::NotSevenBit);
        }
        Ok(Self {
            rest: string,
            left: string.len().div_ceil(MMC_SEGMENT_BYTES),
            first: true,
        })
    }
}

impl Iterator for MmcSegments<'_> {
    type Item = MmcSegment;

    fn next(&mut self) -> Option<MmcSegment> {
        if self.left == 0 {
            return None;
        }
        let (now, rest) = self.rest.split_at(self.rest.len().min(MMC_SEGMENT_BYTES));
        self.rest = rest;
        self.left -= 1;
        let place = if core::mem::take(&mut self.first) {
            MmcSegmentPlace::First
        } else {
            MmcSegmentPlace::Later
        };
        // `new` checked the string: at most 63 segments follow and every
        // segment's bytes are seven-bit and short, so each step is `Some`; a
        // string that were not would end the iterator here.
        let remaining = MmcSegmentCount::new(u8::try_from(self.left).ok()?)?;
        let bytes = MmcSegmentBytes::new(now)?;
        Some(MmcSegment::new(place, remaining, bytes))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for MmcSegments<'_> {}

/// Why a segment did not belong to the string in progress (RP-013 p.40 c and
/// d, the "Segmentation Error" of COMMAND ERROR,
/// [`MmcErrorKind::Segmentation`](super::MmcErrorKind::Segmentation)), or why a
/// finished string is not commands. The string in progress is cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum MmcSegmentError {
    /// A later segment came with no first one before it.
    #[error("a later segment arrived with no first segment")]
    MissingFirst,
    /// A first segment came while a string was in progress. The string is
    /// cancelled and the segment is not taken in: [`push`](MmcReassembler::push)
    /// it again to start its string.
    #[error("a first segment arrived inside a string")]
    FirstInsideString,
    /// A later segment's count is not the one that follows the last.
    #[error("segment {} arrived where {} was due", got.get(), expected.get())]
    OutOfOrder {
        /// The count the next segment had to carry.
        expected: MmcSegmentCount,
        /// The count it carried.
        got: MmcSegmentCount,
    },
    /// The string is complete but is not a whole number of well-formed items.
    #[error("the segments do not make a whole number of well-formed items")]
    Malformed,
}

/// A string of `T` held as its bytes: the whole of a reassembled message,
/// validated, longer than one message holds. [`iter`](Self::iter) cannot fail.
pub struct MmcString<'a, T> {
    bytes: &'a [u8],
    item: PhantomData<fn() -> T>,
}

impl<T> Clone for MmcString<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for MmcString<'_, T> {}

impl<T> fmt::Debug for MmcString<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MmcString")
            .field("len", &self.bytes.len())
            .finish()
    }
}

impl<'a, T: MmcItem> MmcString<'a, T> {
    /// `bytes` as a string of `T`, or `None` when it is not a whole number of
    /// well-formed items.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Option<Self> {
        let mut rest = bytes;
        while !rest.is_empty() {
            T::parse(&mut rest).ok()?;
        }
        Some(Self {
            bytes,
            item: PhantomData,
        })
    }

    /// The items, in order.
    #[must_use]
    pub fn iter(&self) -> super::MmcItems<'a, T> {
        super::MmcItems::over(self.bytes)
    }

    /// The wire bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

impl<'a, T: MmcItem> IntoIterator for MmcString<'a, T> {
    type Item = T;
    type IntoIter = super::MmcItems<'a, T>;

    fn into_iter(self) -> super::MmcItems<'a, T> {
        self.iter()
    }
}

/// Puts the segments of a string back together (RP-013 p.40).
///
/// A segment is taken in only when it is the first, or the one the last
/// leaves due: its count is one below the last's. The last (count `0`)
/// completes the string, which comes back as an [`MmcString`] of `T` and is
/// valid until the next call. Any segment that does not belong cancels the
/// string in progress, as does [`cancel`](Self::cancel), which a message that
/// is not a segment calls. A reassembler allocates once, when it is made;
/// [`push`](Self::push) allocates nothing.
///
/// ```
/// use tutti_midi_protocol::{MmcCommand, MmcCommandReassembler, MmcSegments};
///
/// let string = [0x02; 100]; // a hundred PLAYs
/// let mut reassembler = MmcCommandReassembler::new();
/// let mut done = None;
/// for segment in MmcSegments::new(&string).unwrap() {
///     done = reassembler.push(segment).unwrap().map(|s| s.iter().count());
/// }
/// assert_eq!(done, Some(100));
/// ```
#[derive(Debug)]
pub struct MmcReassembler<T> {
    bytes: Vec<u8>,
    /// The count the next later segment must carry, while a string is in progress.
    due: Option<MmcSegmentCount>,
    item: PhantomData<fn() -> T>,
}

/// A reassembler of COMMAND SEGMENTs.
pub type MmcCommandReassembler = MmcReassembler<MmcCommand>;
/// A reassembler of RESPONSE SEGMENTs.
pub type MmcResponseReassembler = MmcReassembler<MmcInfoField>;

impl<T: MmcItem> Default for MmcReassembler<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: MmcItem> MmcReassembler<T> {
    /// A reassembler with no string in progress, holding room for the longest.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(MMC_MAX_SEGMENTS * MMC_SEGMENT_BYTES),
            due: None,
            item: PhantomData,
        }
    }

    /// Whether a string is in progress.
    #[must_use]
    pub fn in_progress(&self) -> bool {
        self.due.is_some()
    }

    /// Drops the string in progress, if any.
    pub fn cancel(&mut self) {
        self.due = None;
        self.bytes.clear();
    }

    /// Takes in `segment`: `Ok(None)` while the string is incomplete, and the
    /// string with the last.
    ///
    /// # Errors
    ///
    /// [`MmcSegmentError`] when the segment does not belong, or the finished
    /// string is not whole; the string in progress is cancelled.
    pub fn push(
        &mut self,
        segment: MmcSegment,
    ) -> Result<Option<MmcString<'_, T>>, MmcSegmentError> {
        let wrong = match (segment.place(), self.due) {
            (MmcSegmentPlace::First, None) => None,
            (MmcSegmentPlace::First, Some(_)) => Some(MmcSegmentError::FirstInsideString),
            (MmcSegmentPlace::Later, None) => Some(MmcSegmentError::MissingFirst),
            (MmcSegmentPlace::Later, Some(expected)) if segment.remaining() != expected => {
                Some(MmcSegmentError::OutOfOrder {
                    expected,
                    got: segment.remaining(),
                })
            }
            (MmcSegmentPlace::Later, Some(_)) => None,
        };
        if let Some(error) = wrong {
            self.cancel();
            return Err(error);
        }
        if segment.place() == MmcSegmentPlace::First {
            self.bytes.clear();
        }
        self.bytes.extend_from_slice(segment.bytes().as_bytes());
        if let Some(due) = segment.remaining().get().checked_sub(1) {
            self.due = MmcSegmentCount::new(due);
            return Ok(None);
        }
        self.due = None;
        if MmcString::<T>::new(&self.bytes).is_none() {
            self.bytes.clear();
            return Err(MmcSegmentError::Malformed);
        }
        Ok(MmcString::new(&self.bytes))
    }
}
