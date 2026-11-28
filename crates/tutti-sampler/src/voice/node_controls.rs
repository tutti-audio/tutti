//! [`VoiceNodeControls`]: what a host steers a standalone
//! [`VoiceNode`](super::node::VoiceNode) through, the `Controls` of its
//! `IntoNode`.
//!
//! Each cell is published by the control side and lent by the node once per
//! block, latest wins, so a value set twice between two blocks plays the second
//! and the audio thread never allocates or frees one. Every cell is also one of
//! the node's published entries, so the session's sweep reclaims it.

use std::sync::Arc;

use crate::SamplerFloat;
use crate::streamer::{ControlCell, StreamHandle, StreamStatus};

use super::loop_span::Loop;
use super::memory_source::VoiceWindow;
use super::types::PlayDirection;
use tutti_graph::ParamSet;
use tutti_rt::{AtomicCell, Mutex, Param, Publisher};
use tutti_units::{Amplitude, PlaybackRate, UnitParam};

use super::command::GainError;

/// Whether a streamed voice's node holds its stream's reader.
///
/// A config's stream has one reader. A node built while another holds it waits
/// ([`Waiting`](Self::Waiting)), renders silence, and takes the reader between
/// blocks once the holder is gone ([`Holding`](Self::Holding)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StreamHold {
    /// The node renders silence until the reader comes to it.
    Waiting,
    /// The node has the reader and plays the stream.
    Holding,
}
tutti_rt::atomic_word!(StreamHold { Waiting, Holding });

/// What a free-running voice's steering publishes: the play state.
///
/// [`FreeRunning`] writes it (control thread); the node lends it once per block
/// and applies what differs from what it last applied; the fork source reads
/// [`Publisher::current`] when a fork is taken. Latest-wins loses nothing a
/// queue would have delivered: applying each field is a plain overwrite (no
/// seek, no re-arm), so applying every intermediate value in one block ends in
/// the state applying the last does.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Steering {
    /// Whether the voice has been stopped.
    pub(crate) stopped: bool,
    /// How many times the voice has been played: each one starts a
    /// free-running memory voice's cursor, which a reset leaves stopped. A
    /// wrapping count, compared only for equality.
    pub(crate) plays: u32,
}

pub(crate) type SteeringCell = Publisher<Steering>;

/// The play state of a free-running voice: [`play`](Self::play) and
/// [`stop`](Self::stop) it on its own cursor.
///
/// A voice placed on the transport has none
/// ([`VoiceNodeControls::steering`] is `None`): its window says when it sounds,
/// so playing or stopping it by hand is unwritable.
///
/// A free-running voice reads forwards, or backwards when built reversed, on its
/// own cursor, loops as built, and is never stretched.
///
/// Written by this handle alone, under a lock so two clones racing never lose
/// each other's field; read by the node (once per block) and by its fork source.
#[derive(Clone, Debug)]
pub struct FreeRunning {
    cell: SteeringCell,
    /// The latest steering, which the setters edit and publish.
    latest: Arc<Mutex<Steering>>,
}

impl FreeRunning {
    pub(crate) fn new(cell: SteeringCell) -> Self {
        Self {
            latest: Arc::new(Mutex::new(*cell.current())),
            cell,
        }
    }

    /// The steering in force, read on the control side: the mirror the setters
    /// edit, under its lock, which every read of the steering shares.
    pub(crate) fn current(&self) -> Steering {
        *self.latest.lock()
    }

    /// The cell the node reads, which is the node's published entry.
    pub(crate) fn cell(&self) -> &SteeringCell {
        &self.cell
    }

    /// Edits the steering under the lock and publishes the result. Control
    /// thread: publishing allocates the new value and frees the one it
    /// replaces, here, so the node never does either.
    fn steer(&self, edit: impl FnOnce(&mut Steering)) {
        let mut latest = self.latest.lock();
        edit(&mut latest);
        self.cell.publish(*latest);
    }

    /// Starts the voice, or resumes one [`stop`](Self::stop)ped: it sounds from
    /// the node's next block, from where its cursor is (a reset rewinds it).
    pub fn play(&self) {
        self.steer(|s| {
            s.stopped = false;
            s.plays = s.plays.wrapping_add(1);
        });
    }

    /// Stops the voice: it is silent from the node's next block and reads
    /// nothing, until [`play`](Self::play).
    pub fn stop(&self) {
        self.steer(|s| s.stopped = true);
    }

    /// Whether the voice is playing (not [`stop`](Self::stop)ped).
    #[must_use]
    pub fn is_playing(&self) -> bool {
        !self.current().stopped
    }
}

/// How a voice is driven, fixed for the node's life: a free-running voice has
/// a steering and no window, a placed voice a window and no steering. No cell
/// turns one into the other, so a window never goes from none to some and a
/// placed voice is never steered by hand.
#[derive(Clone, Debug)]
pub(crate) enum Driving {
    /// On its own cursor: written through [`FreeRunning`].
    Free(FreeRunning),
    /// On the transport: the window the session or a host moves.
    Placed(Publisher<VoiceWindow>),
}

/// The cells a node reads, shared by its controls and its fork source.
#[derive(Clone, Debug)]
pub(crate) struct Cells {
    pub(crate) driving: Driving,
    /// The voice's loop. A streamed voice's is its [`StreamHandle`]'s cell; a
    /// memory voice's is the node's alone.
    pub(crate) looping: ControlCell<Option<Loop>>,
    /// The voice's direction, held as the loop is.
    pub(crate) direction: ControlCell<PlayDirection>,
    /// The voice's varispeed: the node writes it into a streamed voice's
    /// playback cells.
    pub(crate) speed: Publisher<PlaybackRate>,
}

/// The controls of a standalone [`VoiceNode`](super::node::VoiceNode): what its
/// `IntoNode` hands back. Its gain, a scalar cell the node reads once per block,
/// and five published cells it lends once per block: steering, window, loop,
/// direction and speed. For a voice that streams, it owns the disk
/// [`StreamHandle`] that feeds it.
///
/// A voice is **free-running or placed for the node's life**.
/// [`steering`](Self::steering) is `Some` only for a free voice and
/// [`window_cell`](Self::window_cell) only for a placed one, so playing a placed
/// voice by hand, or freeing it by publishing no window, cannot be written. A
/// placed voice's window can still be moved.
///
/// # Who writes and who reads each cell
///
/// - **gain**: a host through [`set_gain`](Self::set_gain) or
///   [`params`](Self::params); the node reads it once per block (or a param
///   lane's value when one drives it).
/// - **steering**: [`FreeRunning`]; the node and its fork source read.
/// - **window**: the session (`window()` on the key) or a host; the node and
///   its fork source read.
/// - **loop and direction**: the session or a host. A streamed voice's are its
///   stream's cells, so the streamer reads them too; a memory voice's, the node
///   alone. The node never writes them: publishing allocates.
/// - **speed**: the session or a host; the node reads it once per block and, on
///   a streamed voice, writes it into the stream's playback cells.
///
/// Every cell's retired values are freed by the session's sweep, which reaches
/// the cells as the node's published entries (a stream's loop and direction
/// also through the stream's own reclaimer).
///
/// A host keeps this where the node's control thread is. It is `Clone` so a UI
/// and an automation thread can both hold one, and it owns the stream because a
/// stream ends when its handle drops: the stream lives exactly as long as the
/// last clone, which the session's graph keeps for as long as the node is in
/// it. A stream is control-thread only (its drop sends on a bounded queue), so
/// it is never the node's.
///
/// There is no command queue: nothing a caller does here hands the node a
/// payload (a boxed `Voice`, a wave, a vocoder) to drop on the audio thread.
///
/// ```compile_fail
/// # use tutti_sampler::{MemorySourceConfig, VoiceNode, VoiceNodeConfig};
/// # use tutti_buffers::{ChannelLayout, Mono, Wave};
/// # use tutti_graph::IntoNode;
/// # use std::sync::Arc;
/// # let wave = Arc::new(Wave::from_mono(tutti_units::SampleRate::SR_48K, Mono::new(&[0.0; 64])));
/// # let source = MemorySourceConfig::new(wave.clone(), ChannelLayout::STEREO).expect("a placeable wave");
/// let config = VoiceNodeConfig::free(source).expect("a finite gain and a free source");
/// let parts = IntoNode::<f32>::into_parts(VoiceNode::from(config));
/// // A standalone voice node has no door for a pool's wave swap.
/// let _ = parts.controls.replace_wave(tutti_sampler::SlotId(0), wave);
/// ```
#[derive(Clone, Debug)]
pub struct VoiceNodeControls<F: SamplerFloat> {
    pub(crate) cells: Cells,
    /// The voice's gain: the cell the node reads once per block.
    pub(crate) gain: Param<Amplitude>,
    /// The gain by address ([`UnitParam::Volume`]), for a host that sets params
    /// by address.
    pub(crate) params: ParamSet,
    /// The stream that feeds a disk voice, whose loop and direction cells the
    /// controls' are.
    stream: Option<Arc<Mutex<StreamHandle<F>>>>,
    /// Whether the node holds the stream's reader yet; the node's own writes.
    hold: Option<Arc<AtomicCell<StreamHold>>>,
}

impl<F: SamplerFloat> VoiceNodeControls<F> {
    pub(crate) fn new(
        cells: Cells,
        gain: Param<Amplitude>,
        params: ParamSet,
        stream: Option<Arc<Mutex<StreamHandle<F>>>>,
        hold: Option<Arc<AtomicCell<StreamHold>>>,
    ) -> Self {
        Self {
            cells,
            gain,
            params,
            stream,
            hold,
        }
    }

    /// The play state of a free-running voice; `None` for a placed one, whose
    /// window says when it sounds.
    #[must_use]
    pub fn steering(&self) -> Option<&FreeRunning> {
        match &self.cells.driving {
            Driving::Free(steering) => Some(steering),
            Driving::Placed(_) => None,
        }
    }

    /// The cell holding a placed voice's window; `None` for a free-running
    /// voice.
    ///
    /// A window is a [`Beat`](tutti_units::Beat) (`f64`) plus an optional
    /// duration: two values, and a precision the transport cannot afford to
    /// lose, so it is no scalar param (an `f32` beat position loses whole
    /// frames past ~2²⁴). The node lends the cell once per block, so a window
    /// published twice between two blocks plays the second. A fork of the node
    /// taken after a publish starts from it. Stretch applies only to a placed
    /// voice.
    ///
    /// The session writes it (`window()` on the key); a host driving a node by
    /// hand publishes into it.
    #[must_use]
    pub fn window_cell(&self) -> Option<&Publisher<VoiceWindow>> {
        match &self.cells.driving {
            Driving::Free(_) => None,
            Driving::Placed(window) => Some(window),
        }
    }

    /// The cell holding the voice's loop: `None` plays once.
    ///
    /// The node lends it once per block and applies a change to a resident
    /// source. A streamed voice's is its [`StreamHandle`]'s cell
    /// ([`StreamHandle::looping_cell`]), which the streamer reads and applies to
    /// the ring's layout, which the node reads. The session writes it
    /// (`looping()` on the key); a host driving a node by hand publishes into it.
    #[must_use]
    pub fn looping_cell(&self) -> &ControlCell<Option<Loop>> {
        &self.cells.looping
    }

    /// The cell holding the voice's direction, held as the
    /// [loop](Self::looping_cell) is.
    #[must_use]
    pub fn direction_cell(&self) -> &ControlCell<PlayDirection> {
        &self.cells.direction
    }

    /// The cell holding the voice's varispeed (pitch follows the rate), on
    /// either tier.
    ///
    /// The node lends it once per block and applies a change, so a speed
    /// published twice between two blocks plays the second. A fork of the node
    /// taken after a publish starts from it. The session writes it
    /// (`speed()` on the key); a host driving a node by hand publishes into it.
    #[must_use]
    pub fn speed_cell(&self) -> &Publisher<PlaybackRate> {
        &self.cells.speed
    }

    /// What the streamer reports of a disk voice's stream (whether it is still
    /// opening, live or ended, and the fault that ended it); `None` for a memory
    /// voice, which has no stream.
    #[must_use]
    pub fn status(&self) -> Option<StreamStatus> {
        self.stream.as_ref().map(|s| s.lock().status())
    }

    /// Whether a streamed voice's node holds its stream's reader: `Waiting` while
    /// another node of the same config holds it, `Holding` once this one has it.
    /// `None` for a voice with no stream, like [`status`](Self::status).
    #[must_use]
    pub fn stream_hold(&self) -> Option<StreamHold> {
        self.hold.as_ref().map(|hold| hold.load())
    }

    /// Sets the voice's gain, from the node's next block on. A write to a
    /// shared cell: never lost, never queued.
    ///
    /// # Errors
    ///
    /// [`GainError::NotFinite`] for a NaN or infinite gain; the voice keeps the
    /// gain it had.
    pub fn set_gain(&self, gain: Amplitude) -> Result<(), GainError> {
        GainError::check(gain)?;
        self.params.set(UnitParam::Volume, gain.get());
        Ok(())
    }

    /// The voice's gain, as last set.
    #[must_use]
    pub fn gain(&self) -> Amplitude {
        self.gain.load()
    }

    /// The node's params by address: its gain, as [`UnitParam::Volume`].
    #[must_use]
    pub fn params(&self) -> &ParamSet {
        &self.params
    }
}
