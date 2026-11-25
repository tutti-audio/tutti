//! [`StreamHandle`], the control side of an open stream, and the port its close
//! travels by.
//!
//! A stream is addressed by its [`StreamId`], which every command carries; the
//! streamer ignores an id it no longer holds, so a command can never reach
//! another stream. What a stream is asked to play (its loop and direction) is
//! two [`ControlCell`]s the streamer reads; the only command a stream sends is
//! the close its drop sends.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::StreamerSender;
use super::cells::Cells;
use super::command::{StreamId, StreamerCommand};
use super::control::StreamFile;
use super::control_cell::ControlCell;
use super::loops::RingControl;
use super::status::{StatusCell, StreamStatus};
use crate::Loop;
use crate::voice::PlayDirection;
use tutti_rt::Publisher;
use tutti_rt::RingFloat;

/// The streamer's queue is closed: its thread has exited (or its streamer was
/// dropped), so a command was discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PortClosed;

/// The way onto one streamer's command queue, cloned into every [`StreamHandle`].
///
/// Private to the crate: a stream's drop sends its close through it.
/// Control threads only: a send waits while the bounded queue is full and
/// fails once the streamer has gone (every receiver dropped: the thread's on
/// its exit, a built streamer's on its drop).
#[derive(Clone, Debug)]
pub(crate) struct StreamerPort<F: RingFloat> {
    tx: StreamerSender<F>,
}

impl<F: RingFloat> StreamerPort<F> {
    pub(crate) fn new(tx: impl Into<StreamerSender<F>>) -> Self {
        Self { tx: tx.into() }
    }

    /// Queues `cmd` for the streamer. `Ok` means it was *queued*: the streamer
    /// applies it on its own thread, and that outcome is not available
    /// synchronously.
    pub(crate) fn send(&self, cmd: StreamerCommand<F>) -> Result<(), PortClosed> {
        self.tx.send(cmd).map_err(|_| PortClosed)
    }
}

/// The control side of an open stream: a handle that ends the stream when it
/// is dropped.
///
/// What a stream's [`StreamReader`](crate::StreamReader) plays is the ring the
/// streamer keeps filled; this is how the control thread steers and ends it.
/// Dropping the `StreamHandle` sends the streamer a close, and the streamer frees the
/// stream's region in the step that handles it. A voice built from the reader
/// outlives that only as long as the ring it holds (the last holder of the
/// ring frees it), so a host keeps the `StreamHandle` for as long as the voice
/// plays. A `StreamHandle` is control-thread only: it is never a part of a voice,
/// so its drop is never on the audio thread.
///
/// Dropping waits while the streamer's queue is full, as every control
/// command does.
///
/// ```compile_fail
/// # use tutti_sampler::StreamHandle;
/// # fn two(stream: StreamHandle<f32>) {
/// let second = stream.clone(); // a stream has one handle
/// # }
/// ```
///
/// Its compiling twin only borrows it:
///
/// ```
/// # use tutti_sampler::StreamHandle;
/// # fn one(stream: StreamHandle<f32>) {
/// let _again = &stream;
/// # }
/// ```
pub struct StreamHandle<F: RingFloat> {
    id: StreamId,
    port: StreamerPort<F>,
    /// What the streamer publishes of the stream ([`status`](Self::status)).
    status: Arc<StatusCell>,
    /// The cells the voice counts its underruns in; this handle is the one
    /// reader of the count.
    cells: Arc<Cells>,
    /// Underruns read out of `cells` so far.
    underruns: AtomicU64,
    /// What the stream plays, immutable ([`file`](Self::file)).
    file: Arc<StreamFile<F>>,
    /// The ring's published control: the loop as applied is read from it
    /// ([`StreamStatus::loop_`]).
    control: Publisher<RingControl<F>>,
    /// The loop and direction asked for, which the streamer reads and applies
    /// on its own thread: the cells the stream's node steers. Shared with the
    /// sweep [`reclaimer`](Self::reclaimer) hands out, which ends with the handle.
    asked: Arc<Asked>,
}

/// What a stream is asked to play: the two cells the streamer reads.
struct Asked {
    looping: ControlCell<Option<Loop>>,
    direction: ControlCell<PlayDirection>,
}

impl tutti_rt::Reclaim for Asked {
    fn reclaim(&self) -> usize {
        self.looping.reclaim() + self.direction.reclaim()
    }
}

impl<F: RingFloat> std::fmt::Debug for StreamHandle<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHandle")
            .field("id", &self.id)
            .finish()
    }
}

impl<F: RingFloat> StreamHandle<F> {
    pub(crate) fn new(
        id: StreamId,
        port: StreamerPort<F>,
        status: Arc<StatusCell>,
        cells: Arc<Cells>,
        file: Arc<StreamFile<F>>,
        control: Publisher<RingControl<F>>,
        looping: ControlCell<Option<Loop>>,
        direction: ControlCell<PlayDirection>,
    ) -> Self {
        Self {
            id,
            port,
            status,
            cells,
            underruns: AtomicU64::new(0),
            file,
            control,
            asked: Arc::new(Asked { looping, direction }),
        }
    }

    /// The stream's status, as it is now: a level read, never blocking.
    ///
    /// [`StreamStatus::phase`] is what the streamer last published: `Opening`
    /// until the ring is primed (wait for it to be anything else before
    /// starting a voice that must not underrun), `Live` while it serves the
    /// stream, `Ended` once the whole file is in the ring,
    /// `Faulted` for a decode fault (the voice reads silence from it), a panic
    /// the streamer contained to this stream, or a streamer that is gone.
    /// [`StreamStatus::underruns`] counts the frames the voice found missing
    /// in the ring, since the stream opened: the audio side counts them
    /// (the voice's underrun report) and this is the one place they are read.
    ///
    /// [`StreamStatus::loop_`] is the loop the streamer applied: what an
    /// export fork of the stream's voice plays, and what a downgrade (a lead-in
    /// that could not be read) shows as, where [`loop_`](Self::loop_) is the
    /// request.
    ///
    /// Control thread; no allocation.
    #[must_use]
    pub fn status(&self) -> StreamStatus {
        let drained = self.cells.take_underruns();
        let underruns = self.underruns.fetch_add(drained, Ordering::Relaxed) + drained;
        StreamStatus {
            phase: self.status.phase(),
            loop_: self.control.current().loop_,
            underruns,
        }
    }

    /// The file the stream plays: its asset and rate, and the wave when the
    /// streamer holds it whole. What a fork of the stream's voice reads.
    #[must_use]
    pub fn file(&self) -> &StreamFile<F> {
        &self.file
    }

    /// The loop asked for: the request, which the streamer applies on its own
    /// thread ([`status`](Self::status)`.loop_` is what it applied). `None`
    /// until one is published to [`looping_cell`](Self::looping_cell).
    #[must_use]
    pub fn loop_(&self) -> Option<Loop> {
        *self.asked.looping.current()
    }

    /// The direction asked for.
    #[must_use]
    pub fn direction(&self) -> PlayDirection {
        *self.asked.direction.current()
    }

    /// The stream this handle steers: the identity every command it sends
    /// carries.
    #[must_use]
    pub fn id(&self) -> StreamId {
        self.id
    }

    /// The cell holding the loop the stream plays (its frames index the file,
    /// not the engine's rate): `None` plays once.
    ///
    /// A publish rings the streamer's bell, and the streamer reads the cell
    /// at the start of its next cycle and applies a change there, even when
    /// it was parked with nothing else to do. A change near
    /// the playhead is heard [`GUARD_FRAMES`](crate::GUARD_FRAMES) past the block being played,
    /// crossfaded over the stream's seek crossfade
    /// (`DiskStreamerConfig::seek_crossfade_frames`) from what the old loop
    /// would have played; one further ahead, exactly where the memory tier
    /// hears it. From there on the stream plays the new loop exactly as the
    /// memory tier does ([`Loop`] has the rule). An export fork taken after
    /// the change reads the new loop from the start of its render.
    ///
    /// The session writes it (`sampler.looping()`); a host driving a stream by
    /// hand publishes into it, from the control thread, and reclaims it
    /// ([`reclaim`](Self::reclaim)).
    #[must_use]
    pub fn looping_cell(&self) -> &ControlCell<Option<Loop>> {
        &self.asked.looping
    }

    /// The cell holding the direction the stream plays.
    ///
    /// Reverse reads the file mirrored and ignores the loop, as the memory
    /// tier's reverse does. The streamer turns the ring's mapping on its own
    /// thread when it takes the change (a publish wakes it), and the change is heard just past the
    /// block the voice is in, crossfaded (a loop change's rule). The
    /// direction is published with the ring's control: the voice and a fork
    /// read it from there, and nothing polls a flag.
    #[must_use]
    pub fn direction_cell(&self) -> &ControlCell<PlayDirection> {
        &self.asked.direction
    }

    /// Frees the loops and directions the control side replaced and no
    /// streamer cycle holds. Control thread, from the owner's tick: the session
    /// calls it for a stream it opened ([`reclaimer`](Self::reclaimer)); a caller
    /// without a session calls it on its own tick.
    pub fn reclaim(&self) -> usize {
        tutti_rt::Reclaim::reclaim(&*self.asked)
    }

    /// This handle's [`reclaim`](Self::reclaim), held weakly: it upgrades for
    /// as long as the handle lives and not after, so a holder of many keeps
    /// no cell of a stream that ended. The session keeps one per stream it opens.
    #[must_use]
    pub fn reclaimer(&self) -> std::sync::Weak<dyn tutti_rt::Reclaim + Send + Sync> {
        let asked: Arc<dyn tutti_rt::Reclaim + Send + Sync> = self.asked.clone();
        Arc::downgrade(&asked)
    }
}

/// A dropped `StreamHandle` ends its stream. A streamer that is already gone has
/// nothing left to end.
impl<F: RingFloat> Drop for StreamHandle<F> {
    fn drop(&mut self) {
        let _ = self.port.send(StreamerCommand::Close { stream: self.id });
    }
}

#[cfg(test)]
impl<F: RingFloat> StreamHandle<F> {
    /// A handle on no streamer's stream: only its sends are in question (tests).
    pub(crate) fn detached(id: StreamId, port: StreamerPort<F>) -> Self {
        StreamHandle::new(
            id,
            port,
            Arc::new(StatusCell::new()),
            Arc::new(Cells::new()),
            Arc::new(StreamFile::new(
                std::path::PathBuf::new(),
                tutti_units::SampleRate::SR_48K,
                None,
            )),
            Publisher::new(RingControl::new(
                super::loops::RingMap::plain(
                    super::loops::Arrangement::plain(tutti_units::Samples::new(0)),
                    0,
                ),
                None,
            )),
            ControlCell::new(None),
            ControlCell::new(PlayDirection::Forward),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM_1: StreamId = StreamId::of(1);
    const STREAM_2: StreamId = StreamId::of(2);

    /// Every command a stream sends names that stream, and only it: two streams
    /// on one queue each send their own id in the close their drop sends.
    #[test]
    fn a_stream_names_itself_in_the_close_it_sends() {
        let (tx, queue) = std::sync::mpsc::sync_channel::<StreamerCommand<f32>>(16);
        let port = StreamerPort::new(tx);
        let (a, b) = (
            StreamHandle::detached(StreamId::of(1), port.clone()),
            StreamHandle::detached(StreamId::of(2), port),
        );
        drop(a);
        drop(b);
        let sent: Vec<_> = queue.try_iter().collect();
        assert!(
            matches!(
                sent.as_slice(),
                [
                    StreamerCommand::Close { stream: STREAM_1 },
                    StreamerCommand::Close { stream: STREAM_2 },
                ]
            ),
            "{sent:?}"
        );
    }
}
