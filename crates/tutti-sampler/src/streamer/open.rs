//! Opening a stream: the request ([`Placement`]), the reply ([`PendingOpen`],
//! [`Open`], [`OpenError`]) and what the reply carries: the control side's
//! [`Stream`] and the audio side's one [`StreamReader`].
//!
//! The ring's width comes from the file's header, so the ring is built on the
//! streamer's thread after the probe, and a file that cannot seek is decoded
//! whole before it can be. The reader therefore cannot be the return value of
//! `open`; it is the **reply** of one open: a cell the streamer writes once
//! and [`PendingOpen::poll`] reads once, consuming the `PendingOpen`. There is nothing
//! to look a reader up by, so a reader cannot be taken twice (`poll` takes
//! `self`), cannot be taken before the stream exists (`Pending` carries the
//! `PendingOpen` back), and cannot be another open's (the reply is this open's).

use std::ops::Range;
use std::sync::Arc;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tutti_buffers::ChannelLayout;
use tutti_rt::{PosReader, Publisher};
use tutti_units::{SamplePosition, SampleRate, Samples};

use super::bell::Bell;
use super::block::{Block, RingReader};
use super::cells::Cells;
use super::command::StreamId;
use super::control::StreamFile;
use super::control_cell::ControlCell;
use super::loops::RingControl;
use super::oneshot::{Receiver, Unanswered};
use super::prefetch::SharedReader;
use super::status::StatusCell;
use super::stream::{Stream, StreamerPort};
use crate::voice::Direction;
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
pub struct Placement(SamplePosition);

impl Placement {
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

/// A position [`Placement`] refuses, carrying the refused raw.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0} is not a position in a file: it must be finite and not negative")]
pub struct RefusedPlacement(pub f64);

impl TryFrom<f64> for Placement {
    type Error = RefusedPlacement;

    fn try_from(raw: f64) -> Result<Self, RefusedPlacement> {
        Self::new(SamplePosition::new(raw)).ok_or(RefusedPlacement(raw))
    }
}

impl From<Placement> for f64 {
    fn from(placement: Placement) -> f64 {
        placement.0.get()
    }
}

/// Why an open failed.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The file could not be opened or read.
    #[error("the file could not be opened: {0}")]
    Io(std::io::ErrorKind),
    /// The file is not audio this build can decode.
    #[error("the file is not audio this build can decode")]
    Format,
    /// The file's channels cannot be placed into the voice's width: a channel
    /// no speaker names (a `Speaker::Unknown` in its topology), or a
    /// narrowing with an unordered end (a 5.1 file into 5 channels).
    #[error("the file cannot be placed: {0}")]
    Placement(#[source] PlaceError),
    /// The streamer thread is gone, so the open was never answered.
    #[error("the streamer thread is gone")]
    StreamerGone,
}

/// What the streamer answers an open with: everything the audio side's
/// [`StreamReader`] is built from but the registered control reader, which the
/// control thread registers ([`PendingOpen::poll`]).
pub(crate) struct Opened<F: RingFloat> {
    pub(crate) id: StreamId,
    pub(crate) ring: SharedReader<F>,
    pub(crate) pos: PosReader<F>,
    pub(crate) cells: Arc<Cells>,
    /// What the stream's handle reads of it ([`Stream::status`]).
    pub(crate) status: Arc<StatusCell>,
    /// What the stream plays, immutable.
    pub(crate) file: Arc<StreamFile<F>>,
    /// The ring's control cell, which the handle reads the applied loop from.
    pub(crate) control: Publisher<RingControl<F>>,
    /// The loop and direction the control side asks for, which the streamer
    /// reads: the handle holds the publishing ends.
    pub(crate) looping: ControlCell<Option<Loop>>,
    pub(crate) direction: ControlCell<Direction>,
    pub(crate) place: Place<F>,
    pub(crate) width: ChannelLayout,
    /// What the reader rings when its ring wants the streamer.
    pub(crate) bell: Arc<Bell>,
}

impl<F: RingFloat> std::fmt::Debug for Opened<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// An open the streamer has not answered yet.
///
/// [`poll`](Self::poll) consumes it: the stream and its reader are the reply,
/// and a reply is received once.
///
/// ```compile_fail
/// # use tutti_sampler::PendingOpen;
/// # fn twice(opening: PendingOpen<f32>) {
/// let _ = opening.poll();
/// let _ = opening.poll(); // `poll` took `opening` by value
/// # }
/// ```
///
/// Its compiling twin polls the `PendingOpen` it is handed back instead:
///
/// ```
/// # use tutti_sampler::{Open, PendingOpen};
/// # fn again(opening: PendingOpen<f32>) {
/// if let Open::Pending(opening) = opening.poll() {
///     let _ = opening.poll();
/// }
/// # }
/// ```
///
/// Dropping a `PendingOpen` abandons the open: the streamer closes the stream it
/// opened.
#[derive(Debug)]
#[must_use = "an open nobody polls is abandoned"]
pub struct PendingOpen<F: RingFloat> {
    id: StreamId,
    reply: Receiver<Result<Opened<F>, OpenError>>,
    /// The streamer's command queue, which the stream it answers with sends
    /// on.
    port: StreamerPort<F>,
}

impl<F: RingFloat> PendingOpen<F> {
    pub(crate) fn new(
        id: StreamId,
        reply: Receiver<Result<Opened<F>, OpenError>>,
        port: StreamerPort<F>,
    ) -> Self {
        Self { id, reply, port }
    }

    /// The stream this open is for.
    pub fn id(&self) -> StreamId {
        self.id
    }

    /// Whether the streamer has answered. Never blocks; control thread.
    ///
    /// `Ready` registers the reader's control cell before it returns (that
    /// allocates, so it is here and not on the audio thread).
    pub fn poll(self) -> Open<F> {
        match self.reply.try_recv() {
            Ok(Ok(opened)) => self.ready(opened),
            Ok(Err(error)) => Open::Failed(error),
            Err(Unanswered::Empty) => Open::Pending(self),
            // The streamer dropped the reply unanswered: its future is gone.
            Err(Unanswered::Disconnected) => Open::Failed(OpenError::StreamerGone),
        }
    }

    fn ready(self, opened: Opened<F>) -> Open<F> {
        Self::ready_with(self.port, opened)
    }

    fn ready_with(port: StreamerPort<F>, opened: Opened<F>) -> Open<F> {
        Open::Ready(
            Stream::new(
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
}

/// Awaiting an open resolves with the stream and its reader once the streamer
/// answers, and wakes the task when it does: the open as a future, where
/// [`poll`](PendingOpen::poll) is for a caller that has other work between looks.
///
/// # Errors
///
/// The open's own failure, as the streamer answered it; [`OpenError::StreamerGone`]
/// when the streamer's future ends first.
impl<F: RingFloat> Future for PendingOpen<F> {
    type Output = Result<(Stream<F>, StreamReader<F>), OpenError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.reply.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(Ok(opened))) => {
                // `ready` takes the port by value: a spent `PendingOpen` is not
                // polled again.
                let port = this.port.clone();
                let Open::Ready(stream, reader) = Self::ready_with(port, opened) else {
                    unreachable!("`ready_with` answers `Ready`")
                };
                Poll::Ready(Ok((stream, reader)))
            }
            Poll::Ready(Ok(Err(error))) => Poll::Ready(Err(error)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(OpenError::StreamerGone)),
        }
    }
}

/// An open's answer.
#[derive(Debug)]
#[must_use = "a dropped `Pending` abandons the open; a dropped `Stream` closes it"]
pub enum Open<F: RingFloat> {
    /// Not answered yet: poll the `PendingOpen` again.
    Pending(PendingOpen<F>),
    /// The stream is open: the handle is the control thread's, and the reader
    /// is the voice's, by value. Dropping the [`Stream`] ends the stream.
    Ready(Stream<F>, StreamReader<F>),
    /// The stream did not open, and will not.
    Failed(OpenError),
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
    /// the control side ([`PendingOpen::poll`]): registering allocates.
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
