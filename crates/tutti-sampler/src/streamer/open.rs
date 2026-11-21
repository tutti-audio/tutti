//! Opening a stream: the request ([`StreamStart`]), its refusals ([`OpenError`],
//! [`AdoptError`]) and what an adopted stream is made of: the control side's
//! [`StreamHandle`] and the audio side's one [`StreamReader`].
//!
//! The ring's width comes from the file's header, so the ring is built after the
//! file is read ([`OpenStream::run`](crate::OpenStream::run)), on the control thread
//! ([`DiskStreamer::adopt`](crate::DiskStreamer::adopt)). The reader is the value of
//! that adopt, with the stream: it cannot be taken twice, cannot exist before the
//! stream does, and cannot be another open's.

use std::ops::Range;
use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_rt::{PosReader, Publisher};
use tutti_units::{SamplePosition, SampleRate, Samples};

use super::block::{Block, RingReader};
use super::cells::Cells;
use super::command::StreamId;
use super::control::StreamFile;
use super::control_cell::ControlCell;
use super::decode::OpenedStream;
use super::loops::RingControl;
use super::prefetch::SharedReader;
use super::status::StatusCell;
use super::stream::{StreamHandle, StreamerPort};
use crate::voice::PlayDirection;
use crate::voice::interp::Place;
use crate::{Loop, PlaceError};
use tutti_rt::RingFloat;

/// Where in the file a stream's reader starts, in file frames.
///
/// A position is refused, never clamped: a negative or non-finite one would
/// play the wrong audio with no error. A fractional position is floored to the
/// frame it falls in when the stream's window is placed.
///
/// # Raw
///
/// `new` and `get` speak [`SamplePosition`]; serde and `TryFrom` speak the bare
/// `f64` it wraps (a serde boundary, where the unit type stops).
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "f64", into = "f64")
)]
pub struct StreamStart(SamplePosition);

impl StreamStart {
    /// The head of the file.
    pub const HEAD: Self = Self(SamplePosition::new(0.0));

    /// A start at `start` frames into the file; `None` when `start` is
    /// negative or not finite.
    #[must_use]
    pub fn new(start: SamplePosition) -> Option<Self> {
        (start.get().is_finite() && start.get() >= 0.0).then_some(Self(start))
    }

    /// The position, as given.
    #[must_use]
    pub fn get(self) -> SamplePosition {
        self.0
    }

    /// The frame the stream's window starts at: the position floored.
    pub(crate) fn frame(self) -> u64 {
        // Finite and non-negative by construction; a huge value saturates,
        // and the ring parks a window at its position limit.
        self.0.get().floor() as u64
    }
}

/// A position [`StreamStart`] refuses, carrying the refused raw.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0} is not a position in a file: it must be finite and not negative")]
pub struct RefusedStreamStart(pub f64);

impl TryFrom<f64> for StreamStart {
    type Error = RefusedStreamStart;

    fn try_from(raw: f64) -> Result<Self, RefusedStreamStart> {
        Self::new(SamplePosition::new(raw)).ok_or(RefusedStreamStart(raw))
    }
}

impl From<StreamStart> for f64 {
    fn from(placement: StreamStart) -> f64 {
        placement.0.get()
    }
}

/// Why a file could not be read for an open ([`OpenStream::run`](crate::OpenStream::run)).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The file could not be opened or read.
    #[error("the file could not be opened: {0}")]
    Io(std::io::ErrorKind),
    /// The file is not audio this build can decode.
    #[error("the file is not audio this build can decode")]
    Format,
}

/// Why a read file did not become a stream ([`DiskStreamer::adopt`](crate::DiskStreamer::adopt)).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdoptError<F: RingFloat> {
    /// The file's channels cannot be placed into the voice's width: a channel
    /// no speaker names (a `Speaker::Unknown` in its topology), or a
    /// narrowing with an unordered end (a 5.1 file into 5 channels).
    #[error("the file cannot be placed: {0}")]
    Placement(#[source] PlaceError),
    /// The streamer thread is gone, so nothing would feed the stream.
    #[error("the streamer thread is gone")]
    StreamerGone,
    /// The streamer's command queue is full. Nothing was lost: the open comes
    /// back to adopt again once the streamer has taken what it holds.
    #[error("the streamer's queue is full: adopt the open again")]
    Busy(Box<OpenedStream<F>>),
}

/// What building a stream makes besides its record: everything the audio side's
/// [`StreamReader`] is built from but the registered control reader, which
/// [`serve`] registers.
pub(crate) struct Opened<F: RingFloat> {
    pub(crate) id: StreamId,
    pub(crate) ring: SharedReader<F>,
    pub(crate) pos: PosReader<F>,
    pub(crate) cells: Arc<Cells>,
    /// What the stream's handle reads of it ([`StreamHandle::status`]).
    pub(crate) status: Arc<StatusCell>,
    /// What the stream plays, immutable.
    pub(crate) file: Arc<StreamFile<F>>,
    /// The ring's control cell, which the handle reads the applied loop from.
    pub(crate) control: Publisher<RingControl<F>>,
    /// The loop and direction the control side asks for, which the streamer
    /// reads: the handle holds the publishing ends.
    pub(crate) looping: ControlCell<Option<Loop>>,
    pub(crate) direction: ControlCell<PlayDirection>,
    pub(crate) place: Place<F>,
    pub(crate) width: ChannelLayout,
    /// What the reader rings when its ring wants the streamer.
    pub(crate) bell: Arc<tutti_rt::Bell>,
}

impl<F: RingFloat> std::fmt::Debug for Opened<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The stream and its reader, from what building a stream made.
///
/// Registers the reader's control cell before it returns (that allocates, so
/// it is here and not on the audio thread).
pub(super) fn serve<F: RingFloat>(
    opened: Opened<F>,
    port: StreamerPort<F>,
) -> (StreamHandle<F>, StreamReader<F>) {
    (
        StreamHandle::new(
            opened.id,
            port,
            Arc::clone(&opened.status),
            Arc::clone(&opened.cells),
            Arc::clone(&opened.file),
            opened.control.clone(),
            opened.looping.clone(),
            opened.direction.clone(),
        ),
        StreamReader::new(opened),
    )
}

/// The audio side's one reader of a stream: the ring's position reader and
/// its control cell, with what places the file into a voice.
///
/// By value and not [`Clone`]: it is moved into the voice
/// ([`DiskVoice::from_reader`](crate::DiskVoice::from_reader)), and a
/// stream has no other.
///
/// ```compile_fail
/// # use tutti_sampler::StreamReader;
/// # fn two(reader: StreamReader<f32>) {
/// let second = reader.clone(); // a stream has one reader
/// # }
/// ```
///
/// Its compiling twin only borrows it:
///
/// ```
/// # use tutti_sampler::StreamReader;
/// # fn one(reader: StreamReader<f32>) {
/// let _again = &reader;
/// # }
/// ```
pub struct StreamReader<F: RingFloat> {
    id: StreamId,
    pub(crate) ring: SharedReader<F>,
    /// The position reader and the control's reader, registered together on
    /// the control side ([`serve`]): registering allocates.
    pub(crate) reader: RingReader<F>,
    pub(crate) cells: Arc<Cells>,
    /// What a fork of the voice plays, and whether its stream is still served.
    pub(crate) file: Arc<StreamFile<F>>,
    pub(crate) status: Arc<StatusCell>,
    pub(crate) place: Place<F>,
    pub(crate) width: ChannelLayout,
}

impl<F: RingFloat> std::fmt::Debug for StreamReader<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamReader")
            .field("id", &self.id)
            .field("width", &self.width)
            .finish_non_exhaustive()
    }
}

impl<F: RingFloat> StreamReader<F> {
    fn new(opened: Opened<F>) -> Self {
        Self {
            id: opened.id,
            reader: RingReader::new(opened.pos, opened.ring.control_reader())
                .ringing(opened.bell, opened.ring.frames()),
            ring: opened.ring,
            cells: opened.cells,
            file: opened.file,
            status: opened.status,
            place: opened.place,
            width: opened.width,
        }
    }

    /// Claim `reads` (up to two ranges, empty for an unused one) of the ring
    /// and lend its control, for one block that plays at `play`: the
    /// [`Block`] reads the ring through both. Audio thread; wait-free.
    #[inline]
    pub fn block(&mut self, play: Samples, reads: [Range<Samples>; 2]) -> Block<'_, F> {
        self.reader.block(play, reads)
    }

    /// The stream this reads: the ring of its own open, never another's.
    pub fn id(&self) -> StreamId {
        self.id
    }

    /// The file's own sample rate, from its header.
    pub fn file_rate(&self) -> SampleRate {
        self.file.file_rate()
    }

    /// The file the stream plays: what a fork of the voice reads.
    pub fn file(&self) -> &StreamFile<F> {
        &self.file
    }

    /// The width of the voice this reader feeds.
    pub fn channels(&self) -> ChannelLayout {
        self.width
    }
}

#[cfg(test)]
mod tests;
