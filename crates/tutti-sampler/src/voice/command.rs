//! The control plane of the voice nodes: the handles a host steers them
//! through, and the commands a pool's handle queues for its audio thread.
//!
//! A [`VoicePoolHandle`] queues a command per call, drained inside the audio
//! callback, so every command is built control-side, allocation included: the
//! handle is where the vocoder gets constructed for a voice that needs
//! one, precisely so the callback never has to. A [`VoiceNodeHandle`] steers
//! its one voice and owns the disk [`StreamHandle`] that feeds it.

use std::collections::HashMap;
use std::sync::Arc;

use crate::streamer::{ControlCell, StreamHandle, StreamId};

use super::disk_voice::DiskVoice;
use super::loop_span::Loop;
use super::memory_source::{MemorySource, VoiceWindow};
use super::pool::{DrainCounters, PoolDrops, RetireRoom, Retired};
use super::slot::{stretch_values_want_filter, stretch_wanted, window_covers};
use super::stretched::{self, Vocoders};
use super::types::{PlayDirection, Playback, SlotId, Voice, VoiceSource};
use crate::SamplerFloat;
use tutti_buffers::ChannelLayout;
use tutti_buffers::Wave;
use tutti_dsp::PhaseVocoder;
use tutti_graph::ParamSet;
use tutti_music::LoopRange;
use tutti_rt::Param;
use tutti_rt::{Mutex, Publisher, PushError, RingReader, RingWriter};
use tutti_timeline::LoopReader;
use tutti_units::{Amplitude, Beat, Cents, PlaybackRate, StretchFactor};

/// Voice slots a pool reserves, and the most its `AddVoice` drain will hold.
///
/// The drain runs in the audio callback, so `voices.push` must not reallocate
/// there: an add for a new slot once the pool is full is refused, handed back
/// through the retirement ring and counted
/// ([`PoolDrops::voices_refused`]).
/// 64 covers any realistic per-track voice count.
pub(super) const MAX_RESIDENT_VOICES: usize = 64;

/// Commands the pool's ring holds before a send starts failing with
/// [`SendError::Full`]. The drain empties it every block, so a full queue means
/// the control thread outran a whole audio block.
pub(super) const COMMAND_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// Commands sent from ECS → audio thread.
// ---------------------------------------------------------------------------

/// One control-plane message from a [`VoicePoolHandle`] to its pool's audio
/// thread.
///
/// Drained inside the audio callback, so every variant is built and
/// allocated control-side: that is why the two stretch-carrying variants
/// have the vocoder handed to them rather than constructing one. Each is one
/// of the handle's methods.
pub(crate) enum VoiceCommand<F: SamplerFloat> {
    /// Add a voice by handing the reader a fully-formed [`Voice`]: a
    /// [`VoiceSource`] (in-memory `MemorySource` or
    /// streaming `DiskVoice`) plus its
    /// [`Playback`] control-intent record. The drain
    /// builds the slot and applies the full `Playback` per-tier by reusing the
    /// same cold-path appliers the update commands use (`VoiceSource::apply_*` +
    /// `apply_loop`) — no allocation or I/O on the hot path, since the source is
    /// built entirely on the ECS/streamer side before the send.
    ///
    /// One add command for both tiers; the handle's two adds
    /// ([`VoicePoolHandle::add`], which takes a memory source, and
    /// [`VoicePoolHandle::add_streaming`], which takes the stream beside the
    /// disk voice) build it, so a streaming voice never arrives without the
    /// stream that steers its loop and direction.
    AddVoice {
        /// Identifies the slot for every later command aimed at this voice. The
        /// handle sends an add only for an id holding no voice; a drain that
        /// finds the id resident replaces that slot (a pool driven by hand).
        id: SlotId,
        /// Boxed: a [`Voice`] carries a whole `MemorySource`/`DiskVoice`,
        /// far larger than the other command variants — boxing keeps the bounded
        /// command ring's per-slot footprint small. Cold path (drained off the
        /// per-sample loop), so the indirection costs nothing audible. The box
        /// itself moves into the pool's slot — the drain never unboxes it, so
        /// it frees nothing — and leaves through the retirement ring with
        /// the slot.
        voice: Box<Voice<F>>,
        /// A vocoder built by the SENDER, on the control thread, when
        /// `voice.play` asks for stretching.
        ///
        /// The drain runs inside `process`, so building this there would
        /// allocate in the audio callback (an FFT plan plus the
        /// rings of each channel). The handle fills it in
        /// before the command is queued; the drain only moves it into the slot.
        ///
        /// `None` when the voice does not stretch, which is the common case and
        /// costs nothing.
        ///
        /// Boxed, as `UpdateStretch`'s is, so the command stays small: the
        /// slot keeps the same box, and moving a box is a pointer move that
        /// frees nothing.
        stretch: Option<Box<Vocoders<F>>>,
    },
    /// Retire the slot with this id, vocoder and all.
    ///
    /// The drain does not drop the slot in the callback — it hands it to the
    /// control thread through the retirement ring, because freeing a
    /// [`PhaseVocoder`] frees its rings. See
    /// [`VoicePoolHandle::collect_retired`].
    Remove(SlotId),
    /// Point an in-memory slot at a different wave, keeping its placement,
    /// gain and loop.
    ///
    /// Ignored, and counted
    /// ([`PoolDrops::wave_swaps_ignored`]),
    /// for a streaming slot: swapping a disk source
    /// means re-registering the streamer stream on a different file, which is a
    /// control-thread operation issued as a fresh [`AddVoice`](Self::AddVoice).
    ///
    /// The wave the drain lets go of (the slot's old one, or this one when no
    /// in-memory slot takes it) is never dropped in the callback: it leaves
    /// through the retirement ring, since it may be the last reference to
    /// a decoded buffer. See [`VoicePoolHandle::collect_retired`].
    ReplaceWave {
        /// The slot to re-point.
        id: SlotId,
        /// The new wave. Shared rather than copied — several voices may read one.
        wave: Arc<Wave<F>>,
    },
    /// Move the voice's timeline window: a [`VoiceWindow`], so not a scalar
    /// param.
    UpdatePlacement {
        /// The slot to move.
        id: SlotId,
        /// Where the voice starts on the timeline and how long it plays.
        window: VoiceWindow,
    },
    /// Set the voice's playback gain.
    UpdateGain {
        /// The slot to set.
        id: SlotId,
        /// Linear [`Amplitude`], not [`Db`](tutti_units::Db) — the conversion is
        /// the host's.
        gain: Amplitude,
    },
    /// Set the voice's varispeed. Both tiers carry speed in-unit: in-memory
    /// stores it on the `MemorySource`, streaming writes the stream's cells,
    /// which the streamer also reads to size its fills, so one command reaches
    /// both.
    UpdateSpeed {
        /// The slot to set.
        id: SlotId,
        /// [`PlaybackRate`] relative to the source's own rate; `1.0` is unity.
        /// Pitch moves with it — this is varispeed, not time-stretch.
        speed: PlaybackRate,
    },
    /// Set or clear the voice's loop region.
    ///
    /// Routed by tier: in-memory primes the range on the `MemorySource` in
    /// the drain; streaming is the stream's, which owns the loop-start
    /// fadein head the reader cannot reach: [`VoicePoolHandle::send`] queues
    /// the command and publishes the loop to the voice's [`StreamHandle`] on the
    /// calling thread. A streaming voice the handle holds no stream for (added
    /// without one) is not set: the loss is counted ([`PoolDrops`]) and the
    /// intent is not recorded — a `Playback` claiming a loop the stream never
    /// took would be replayed as real on the next insert. The drain records a streaming
    /// slot's loop only when the handle named that slot's own stream
    /// ([`PoolDrops::loops_not_forwarded`] otherwise).
    UpdateLoop {
        /// The slot to set.
        id: SlotId,
        /// The loop to play; `None` clears it.
        setting: Option<Loop>,
    },
    /// Play the voice forwards or backwards.
    ///
    /// One command reaches both tiers, which is why it is not folded into
    /// [`UpdateSpeed`](Self::UpdateSpeed): in-memory drives a reversed index in
    /// the hot read, streaming is the stream's direction
    /// ([`StreamHandle::direction_cell`]), which [`VoicePoolHandle::send`] publishes
    /// to the voice's stream beside queueing the record.
    UpdateReverse {
        /// The slot to set.
        id: SlotId,
        /// [`PlayDirection::Forward`] or [`PlayDirection::Reverse`].
        direction: PlayDirection,
    },
    /// Give the voice a spare vocoder (`Some`), or take its spare away (`None`): the
    /// vocoder a voice under the transport's loop primes for a wrap with
    /// ([`Vocoders`]). Built by the handle, control-side, and only moved into the slot; what
    /// the slot does not keep (the arrival when it already has a spare or no vocoder, the
    /// spare a `None` takes out) leaves through the retirement ring.
    SetSpare {
        /// The slot to set.
        id: SlotId,
        /// The spare to adopt, or `None` to retire the slot's.
        spare: Option<Box<PhaseVocoder<F>>>,
    },
    /// Set the voice's time-stretch factor and pitch shift — the two controls
    /// that change duration and pitch independently, unlike
    /// [`UpdateSpeed`](Self::UpdateSpeed).
    ///
    /// Pool-only: a [`VoiceNode`](super::node::VoiceNode) has no command to
    /// take it, so a node's pitch is fixed at construction and a host that
    /// wants to change it respawns the voice.
    UpdateStretch {
        /// The slot to set.
        id: SlotId,
        /// Duration multiplier ([`StretchFactor`]); `1.0` is unity. Pitch is
        /// unchanged.
        stretch_factor: StretchFactor,
        /// Pitch shift in [`Cents`]; `0.0` is unity. Duration is unchanged.
        pitch_cents: Cents,
        /// A pre-built vocoder for the case where the slot has none yet, same
        /// contract as [`AddVoice::stretch`](Self::AddVoice::stretch):
        /// [`VoicePoolHandle::send`] fills it in on the control thread, and the
        /// drain only moves it into the slot.
        ///
        /// This field is why turning stretch ON mid-flight works at all. A voice
        /// spawned at unity/zero gets `stretch: None` from `AddVoice` (correctly
        /// — it did not need one), so without a vocoder arriving here,
        /// `PlaybackSlot::set_stretch` would flip the gate fields on a slot that
        /// has nothing to flip and the voice would read dry forever. It cannot
        /// be built in the drain: that is the audio thread, and construction
        /// allocates an FFT plan plus per-channel rings.
        ///
        /// `None` when the slot already holds a processor (the update then only writes
        /// `voice.play`) or when the update turns stretching off.
        ///
        /// Boxed, as [`AddVoice::stretch`](Self::AddVoice::stretch) is, and
        /// the slot keeps the same box: moving it frees nothing.
        stretch: Option<Box<Vocoders<F>>>,
    },
}

// Hand-rolled: `ReplaceWave` carries a non-`Debug` `Arc<Wave>` (summarize it by
// frame count); `AddVoice`'s `Box<Voice>` is `Debug`, forwarded as-is.
impl<F: SamplerFloat> VoiceCommand<F> {
    /// Whether the drain may hand one value to the retirement channel for
    /// this command: an add (refused at the voice cap), a remove, a wave
    /// replacement, or a stretch update that carries a vocoder. No command
    /// retires more than one.
    pub(super) fn may_retire(&self) -> bool {
        match self {
            Self::AddVoice { .. } | Self::Remove(_) | Self::ReplaceWave { .. } => true,
            Self::UpdateStretch { stretch, .. } => stretch.is_some(),
            Self::SetSpare { .. } => true,
            _ => false,
        }
    }
}

impl<F: SamplerFloat> std::fmt::Debug for VoiceCommand<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddVoice { id, voice, stretch } => f
                .debug_struct("AddVoice")
                .field("id", id)
                .field("voice", voice)
                .field("stretch_prebuilt", &stretch.is_some())
                .finish(),
            Self::Remove(id) => f.debug_tuple("Remove").field(id).finish(),
            Self::ReplaceWave { id, wave } => f
                .debug_struct("ReplaceWave")
                .field("id", id)
                .field("wave_frames", &wave.len())
                .finish(),
            Self::UpdatePlacement { id, window } => f
                .debug_struct("UpdatePlacement")
                .field("id", id)
                .field("window", window)
                .finish(),
            Self::UpdateGain { id, gain } => f
                .debug_struct("UpdateGain")
                .field("id", id)
                .field("gain", gain)
                .finish(),
            Self::UpdateSpeed { id, speed } => f
                .debug_struct("UpdateSpeed")
                .field("id", id)
                .field("speed", speed)
                .finish(),
            Self::UpdateLoop { id, setting } => f
                .debug_struct("UpdateLoop")
                .field("id", id)
                .field("setting", setting)
                .finish(),
            Self::UpdateReverse { id, direction } => f
                .debug_struct("UpdateReverse")
                .field("id", id)
                .field("direction", direction)
                .finish(),
            Self::SetSpare { id, spare } => f
                .debug_struct("SetSpare")
                .field("id", id)
                .field("spare", &spare.is_some())
                .finish(),
            Self::UpdateStretch {
                id,
                stretch_factor,
                pitch_cents,
                stretch,
            } => f
                .debug_struct("UpdateStretch")
                .field("id", id)
                .field("stretch_factor", stretch_factor)
                .field("pitch_cents", pitch_cents)
                .field("stretch_prebuilt", &stretch.is_some())
                .finish(),
        }
    }
}

/// A [`VoiceCommand`] as the handle queues it for the drain.
///
/// `forwarded` is the stream the handle sets a loop or direction command's
/// setting on (it holds that stream, and sets the setting on it beside
/// queueing the command): `None` for every other command, and for a loop or
/// direction it holds no stream for. The drain records a streaming slot's loop
/// or direction only when this names that slot's stream, so whether a setting
/// reached a stream is decided from the command itself, never from what the
/// handle believed the slot held. Only [`VoicePoolHandle::send`] builds one.
pub(crate) struct Queued<F: SamplerFloat> {
    pub(crate) cmd: VoiceCommand<F>,
    pub(crate) forwarded: Option<StreamId>,
    /// The stamp of the handle's add ([`VoicePoolHandle::stamp`]) when `cmd`
    /// is one, which a refusal hands back; 0 for every other command.
    pub(crate) generation: u64,
}

// ---------------------------------------------------------------------------
// Handle — held by ECS systems, sends commands to the audio-thread unit.
// ---------------------------------------------------------------------------

/// A voice on the transport cannot be played or stopped by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum PlayError {
    /// The voice is placed: its window says when it sounds, and
    /// [`play`](VoiceNodeHandle::play) and [`stop`](VoiceNodeHandle::stop)
    /// drive a free-running voice.
    #[error("the voice is placed on the transport; its window says when it sounds")]
    Placed,
}

/// Publishes `value` to `cell` when it differs from the one in it: the
/// request a stream's streamer reads.
pub(super) fn ask<T: Copy + PartialEq>(cell: &ControlCell<T>, value: T) {
    if *cell.current() != value {
        cell.publish(value);
    }
}

/// A command never reached the pool, and the work it carried is gone.
///
/// Modelled on `tutti_timeline::TimedError`: the two failures are told apart
/// because a caller acts on them differently.
///
/// A dropped add is a note that never sounds; a dropped remove is a voice
/// that never stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum SendError {
    /// The 64-slot command queue was full. Transient: the audio thread drains
    /// it every block, so a caller can back off and retry.
    #[error("voice command queue full; the command was dropped")]
    Full,
    /// The [`VoicePool`](super::pool::VoicePool) has been dropped, so nothing
    /// will ever drain the queue again.
    ///
    /// Permanent: every subsequent call fails the same way, so a caller that
    /// sees this should stop rather than retry. Reporting it is what makes a
    /// dead pool distinguishable from a working one.
    #[error("voice pool is gone; the command was dropped")]
    Disconnected,
    /// The retirement channel has no room left for what the command might
    /// retire: the audio thread hands removed slots, surplus vocoders and
    /// replaced waves back through a channel of
    /// 64 values, and a value that finds it full could only be
    /// freed in the callback. Call
    /// [`collect_retired`](VoicePoolHandle::collect_retired) and send again.
    /// Transient. The command was dropped.
    #[error(
        "the retirement channel has no room: collect retired voices first; the command was dropped"
    )]
    RetirementFull,
    /// The voice is free-running (a memory voice with no window) and a
    /// stretch or pitch shift was asked of it: a time-stretcher reads the
    /// transport's clock, which a free-running voice does not have. Place the
    /// voice ([`set_window`](VoicePoolHandle::set_window)) first. The
    /// command was dropped.
    #[error("the voice is free-running and cannot be stretched; the command was dropped")]
    FreeRunning,
    /// An add named a [`SlotId`] that already holds a voice. Nothing changed:
    /// the voice there keeps playing and its stream, if any, keeps feeding
    /// it. [`remove`](VoicePoolHandle::remove) it first to put another there.
    #[error("slot {} already holds a voice; the add was dropped", .0.0)]
    SlotInUse(SlotId),
    /// A command named a [`SlotId`] that holds no voice: it was never added,
    /// was removed, or its add was refused at the pool's voice cap
    /// ([`PoolDrops::voices_refused`](super::pool::PoolDrops::voices_refused)).
    /// The command was dropped.
    #[error("slot {} holds no voice; the command was dropped", .0.0)]
    NoSuchSlot(SlotId),
    /// [`replace_wave`](VoicePoolHandle::replace_wave) named a slot holding a
    /// streaming voice: its source is a stream on a file, so changing it is a
    /// new [`add_streaming`](VoicePoolHandle::add_streaming) on a stream
    /// opened for the other file. The command was dropped.
    #[error("slot {} holds a streaming voice, whose wave cannot be replaced; the command was dropped", .0.0)]
    DiskVoice(SlotId),
    /// The command carried a gain that is not finite, which the voice would
    /// play as silence. The command was dropped.
    #[error(transparent)]
    Gain(#[from] GainError),
}

/// A gain the sampler refuses: a voice's gain is a finite multiplier, and a
/// NaN or an infinity is a mistake, not a mute (silence is
/// [`Amplitude::SILENT`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum GainError {
    /// The gain is NaN or infinite.
    #[error("the gain is not finite")]
    NotFinite,
}

impl GainError {
    pub(crate) fn check(gain: Amplitude) -> Result<(), Self> {
        if gain.get().is_finite() {
            Ok(())
        } else {
            Err(Self::NotFinite)
        }
    }
}

impl SendError {
    /// Whether this failure is permanent: no later call can succeed either.
    ///
    /// The distinction worth acting on: `Full` means "try again", `Disconnected`
    /// means "stop trying".
    pub fn is_disconnected(self) -> bool {
        matches!(self, Self::Disconnected)
    }
}

/// The controls of a standalone [`VoiceNode`](super::node::VoiceNode): what
/// its `IntoNode` hands back. Its gain, a scalar cell the node reads once per
/// block, its window, loop, speed and play state, published values the node
/// reads once per block, and, for a voice that streams, the disk [`StreamHandle`]
/// that feeds it.
///
/// # An owned handle, and why it is owned
///
/// A host keeps this where the node's control thread is: it is the node's
/// cross-thread control surface (the audio thread reads what it publishes),
/// and it is `Clone` so a UI and an automation thread can both hold one. It
/// owns the disk [`StreamHandle`] because a stream ends when its handle drops, and a
/// stream held apart from the voice is one a caller can drop by accident and
/// silence the voice: here the stream lives exactly as long as the last
/// clone of the handle, which the session's graph keeps for as long as the
/// node is in it. A stream is control-thread only (its drop sends on a
/// bounded queue), so it is never the node's: the node reads the ring, this
/// steers it.
///
/// # Why a separate handle rather than reusing [`VoicePoolHandle`]
///
/// A pool addresses its voices by [`SlotId`] and hands retired slots back for
/// the control thread to free. A `VoiceNode` holds exactly one voice for its
/// whole life: there is no id to address and nothing is ever retired, so both of
/// those fields would be dead weight that every call site has to supply a
/// meaningless value for. The controls it takes are also a strict subset: an
/// add or a remove has no meaning for a node that *is* its voice.
///
/// So its methods name the voice implicitly, and there is no command queue at
/// all: a window, a loop and a speed are values only the latest of which
/// matters, so each is published rather than sent, and nothing a caller does
/// here can hand the node a payload (a boxed `Voice`, a wave, a stretch
/// vocoder) to drop on the audio thread.
///
/// ```compile_fail
/// # use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceSource};
/// # use tutti_buffers::{Mono, Wave};
/// # use std::sync::Arc;
/// # let wave = Arc::new(Wave::from_mono(tutti_units::SampleRate::SR_48K, Mono::new(&[0.0; 64])));
/// # let voice = Voice {
/// #     source: VoiceSource::Memory(MemorySource::new(wave.clone()).expect("a placeable wave")),
/// #     play: Playback::default(),
/// # };
/// let (_node, handle) = VoiceNode::new(voice).expect("a finite gain and a nonempty width").with_handle();
/// // A standalone voice node has no door for a pool's wave swap.
/// let _ = handle.replace_wave(tutti_sampler::SlotId(0), wave);
/// ```
///
/// The same setup with a control it does take compiles:
///
/// ```
/// # use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceSource, VoiceWindow};
/// # use tutti_buffers::{Mono, Wave};
/// # use std::sync::Arc;
/// # let wave = Arc::new(Wave::from_mono(tutti_units::SampleRate::SR_48K, Mono::new(&[0.0; 64])));
/// # let voice = Voice {
/// #     source: VoiceSource::Memory(MemorySource::new(wave.clone()).expect("a placeable wave")),
/// #     play: Playback::default(),
/// # };
/// let (_node, handle) = VoiceNode::new(voice).expect("a finite gain and a nonempty width").with_handle();
/// handle.window_cell().publish(Some(VoiceWindow::from(tutti_units::Beat::new(1.0))));
/// ```
#[derive(Clone, Debug)]
pub struct VoiceNodeHandle<F: SamplerFloat> {
    /// What the handle publishes besides the gain, shared with the node
    /// (which reads it once per block) and with its fork source (which reads
    /// it at fork time).
    pub(crate) steering: SteeringCell,
    /// The voice's window: `Some` places it on the transport, `None` leaves it
    /// free-running. The cell the node reads once per block.
    pub(crate) window: Publisher<Option<VoiceWindow>>,
    /// The voice's loop. A streamed voice's is its [`StreamHandle`]'s cell, which the
    /// streamer reads too; a memory voice's is read by the node alone.
    pub(crate) looping: ControlCell<Option<Loop>>,
    /// The voice's direction, held as the loop is.
    pub(crate) direction: ControlCell<PlayDirection>,
    /// The voice's varispeed: the cell the node reads once per block, on
    /// either tier (a streamed voice's node writes it into the stream's
    /// playback cells).
    pub(crate) speed: Publisher<PlaybackRate>,
    /// The latest steering, which the setters edit under the lock and
    /// publish: two handles racing never lose each other's field.
    latest: Arc<Mutex<Steering>>,
    /// The voice's gain: the cell the node reads once per block.
    pub(crate) gain: Param<Amplitude>,
    /// The gain by address ([`UnitParam::Volume`](tutti_units::UnitParam)),
    /// for a host that sets params by address.
    pub(crate) params: ParamSet,
    /// The stream that feeds a disk voice, whose loop and direction cells the
    /// handle's are.
    stream: Option<Arc<Mutex<StreamHandle<F>>>>,
}

/// What a [`VoiceNode`](super::node::VoiceNode)'s handle publishes besides
/// the gain, the window, the loop, the direction and the speed, latest wins:
/// the play state.
///
/// The handle publishes (control thread); the node lends the cell once per
/// block, and applies what differs from what it last applied; the fork
/// source reads [`Publisher::current`] when a fork is taken, so a clip moved
/// after the insert exports where it was moved to.
///
/// Latest-wins loses nothing a queue would have delivered: applying each
/// field is a plain overwrite (no seek, no re-arm), so applying every
/// intermediate value in one block ends in the same state as applying the
/// last, and nothing else rides with it to be ordered against.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Steering {
    /// Whether the handle has stopped the voice.
    pub(crate) stopped: bool,
    /// How many times the handle has played the voice: each one starts a
    /// free-running memory voice's cursor, which a reset leaves stopped. A
    /// wrapping count, compared only for equality.
    pub(crate) plays: u32,
}

pub(crate) type SteeringCell = Publisher<Steering>;

impl<F: SamplerFloat> VoiceNodeHandle<F> {
    pub(crate) fn new(
        steering: SteeringCell,
        window: Publisher<Option<VoiceWindow>>,
        looping: ControlCell<Option<Loop>>,
        direction: ControlCell<PlayDirection>,
        speed: Publisher<PlaybackRate>,
        gain: Param<Amplitude>,
        params: ParamSet,
        stream: Option<StreamHandle<F>>,
    ) -> Self {
        Self {
            latest: Arc::new(Mutex::new(*steering.current())),
            steering,
            window,
            looping,
            direction,
            speed,
            gain,
            params,
            stream: stream.map(|s| Arc::new(Mutex::new(s))),
        }
    }

    /// Edits the steering under the lock and publishes the result. Control
    /// thread: publishing allocates the new value and frees the one it
    /// replaces, here, so the node never does either.
    fn steer(&self, edit: impl FnOnce(&mut Steering)) {
        let mut latest = self.latest.lock();
        edit(&mut latest);
        self.steering.publish(*latest);
    }

    /// The cell holding the voice's window: `Some` places the voice on the
    /// transport, `None` leaves it free-running.
    ///
    /// A window is a [`Beat`] (`f64`) plus an optional
    /// duration: two values, and a precision the transport cannot afford to
    /// lose, so it is no scalar param (an `f32` beat position loses whole
    /// frames past ~2²⁴). The node lends the cell once per block, so a window
    /// published twice between two blocks plays the second. A fork of the node
    /// taken after a publish starts from it. A streamed voice is always
    /// placed: `None` leaves it at the window it has. Stretch applies only to
    /// a placed voice, so a stretched clip given `None` plays dry.
    ///
    /// The session writes it (`sampler.window()`); a host driving a node by
    /// hand publishes into it.
    pub fn window_cell(&self) -> &Publisher<Option<VoiceWindow>> {
        &self.window
    }

    /// Whether the voice plays its window of the transport, as published.
    pub fn is_placed(&self) -> bool {
        self.stream.is_some() || self.window.current().is_some()
    }

    /// The cell holding the voice's loop: `None` plays once.
    ///
    /// The node lends it once per block and applies a change to a resident
    /// source. A streamed voice's is its [`StreamHandle`]'s cell
    /// ([`StreamHandle::looping_cell`]), which the streamer reads and applies to the
    /// ring's layout, which the node reads. The session writes it
    /// (`sampler.looping()`); a host driving a node by hand publishes into it.
    pub fn looping_cell(&self) -> &ControlCell<Option<Loop>> {
        &self.looping
    }

    /// The cell holding the voice's direction, held as the
    /// [loop](Self::looping_cell) is.
    pub fn direction_cell(&self) -> &ControlCell<PlayDirection> {
        &self.direction
    }

    /// What the streamer reports of a disk voice's stream (whether it is
    /// still opening, live or ended, and the fault that ended it); `None` for
    /// a memory voice, which has no stream.
    #[must_use]
    pub fn status(&self) -> Option<crate::StreamStatus> {
        self.stream.as_ref().map(|s| s.lock().status())
    }

    /// The cell holding the voice's varispeed (pitch follows the rate), on
    /// either tier.
    ///
    /// The node lends it once per block and applies a change, so a speed
    /// published twice between two blocks plays the second. A fork of the node
    /// taken after a publish starts from it. The session writes it
    /// (`sampler.speed()`); a host driving a node by hand publishes into it.
    pub fn speed_cell(&self) -> &Publisher<PlaybackRate> {
        &self.speed
    }

    /// Starts a free-running voice, or resumes one [`stop`](Self::stop)ped:
    /// the voice sounds from the node's next block, from where its cursor is
    /// (a reset rewinds it).
    ///
    /// Placed means the published window ([`is_placed`](Self::is_placed)),
    /// checked on the control thread before the steer: a window published
    /// between the two is not seen by this call.
    ///
    /// A free-running voice has limits: it reads forwards, or backwards when
    /// built reversed, on its own cursor, loops as built, and is never
    /// stretched (the stretch setters refuse it,
    /// [`SendError::FreeRunning`]).
    ///
    /// # Errors
    ///
    /// [`PlayError::Placed`] for a voice on the transport: its window says
    /// when it sounds.
    pub fn play(&self) -> Result<(), PlayError> {
        self.drive(false)
    }

    /// Stops a free-running voice: it is silent from the node's next block and
    /// reads nothing, until [`play`](Self::play).
    ///
    /// # Errors
    ///
    /// [`PlayError::Placed`] for a voice on the transport.
    pub fn stop(&self) -> Result<(), PlayError> {
        self.drive(true)
    }

    fn drive(&self, stopped: bool) -> Result<(), PlayError> {
        if self.is_placed() {
            return Err(PlayError::Placed);
        }
        self.steer(|s| {
            s.stopped = stopped;
            if !stopped {
                s.plays = s.plays.wrapping_add(1);
            }
        });
        Ok(())
    }

    /// Whether the voice is playing (not [`stop`](Self::stop)ped).
    pub fn is_playing(&self) -> bool {
        !self.latest.lock().stopped
    }

    /// Sets the voice's gain, from the node's next block on. A write to a
    /// shared cell: never lost, never queued.
    ///
    /// # Errors
    ///
    /// [`GainError::NotFinite`] for a NaN or infinite gain; the voice keeps
    /// the gain it had.
    pub fn set_gain(&self, gain: Amplitude) -> Result<(), GainError> {
        GainError::check(gain)?;
        self.params.set(tutti_units::UnitParam::Volume, gain.get());
        Ok(())
    }

    /// The voice's gain, as last set.
    pub fn gain(&self) -> Amplitude {
        self.gain.load()
    }

    /// The node's params by address: its gain, as
    /// [`UnitParam::Volume`](tutti_units::UnitParam::Volume).
    pub fn params(&self) -> &ParamSet {
        &self.params
    }
}

/// The control-thread end of a [`VoicePool`](super::pool::VoicePool)'s command
/// ring, and the collection point for what the audio thread retires.
///
/// Held by the host (an ECS system, typically) and cloned freely — its several
/// control threads share the one command writer behind a lock. It mirrors the
/// reader's width, and shares the rate the reader was last prepared at, so
/// `send` can build a vocoder that matches, on this thread. It holds the [`StreamHandle`] of every
/// streaming voice added through [`add_streaming`](Self::add_streaming), so
/// `send` publishes a streaming voice's loop and direction to its stream itself:
/// the pool's drain has no stream to publish them to, and a stream cannot live
/// in a voice (its drop sends on a bounded queue, which the audio thread must
/// not do). A stream ends when its voice is removed.
///
/// # Ids the drain refused
///
/// An add the drain refuses at the voice cap
/// ([`PoolDrops::voices_refused`]) counts as live until
/// [`collect_retired`](Self::collect_retired) takes the refused voice back:
/// then its id holds no voice, its stream (if any) ends, and a command for it
/// is [`SendError::NoSuchSlot`]. A host that never collects keeps the id (and
/// the stream) held. Each add is stamped and the refusal carries its stamp, so
/// the collect frees the id only when the entry is still that add's: an id
/// removed and added again since keeps its new voice.
///
/// # Ids the handle knows
///
/// The handle knows the ids of the voices it added, less the ones it removed
/// and the ones the pool refused. A voice never ends on its own in the pool (a
/// one-shot stays resident, silent), so a finished voice is live until
/// removed. The set is shared by every clone.
#[derive(Clone, Debug)]
pub struct VoicePoolHandle<F: SamplerFloat> {
    /// Pushes into the pool's bounded command ring. A handle is `Clone` and
    /// several control threads send, so the one writer sits behind a lock no
    /// audio thread ever takes.
    pub(crate) tx: Arc<Mutex<RingWriter<Queued<F>>>>,
    /// Slots the audio thread has removed and handed back to be freed here.
    ///
    /// See [`VoicePool`](super::pool::VoicePool)'s `retired`. Draining this is what actually moves the
    /// deallocation off the callback; [`collect_retired`](Self::collect_retired)
    /// is the call that does it.
    pub(crate) retired: Arc<Mutex<RingReader<Retired<F>>>>,
    /// The reader's output width, copied at construction (it is fixed for the
    /// reader's lifetime). Lets [`send`](Self::send) build a vocoder at
    /// the right width on the CONTROL thread — see
    /// [`VoiceCommand::AddVoice::stretch`].
    pub(crate) channels: ChannelLayout,
    /// What the pool's drain dropped or refused; read by
    /// [`drops`](Self::drops).
    pub(crate) counters: Arc<DrainCounters>,
    /// The retirement slots reserved of `retired`; this handle's ring only.
    pub(crate) room: Arc<RetireRoom>,
    /// The stream of every streaming voice this handle (or a clone) added with
    /// [`add_streaming`](Self::add_streaming), by slot; a slot holding a memory
    /// voice, or a streaming one added without a stream, has no entry.
    ///
    /// It decides only where `send` sets a loop or a direction, never whether
    /// the drain records one: a voice inserted with
    /// [`insert_voice`](super::pool::VoicePool::insert_voice) after the handle
    /// was made is not in it, and the drain then refuses that slot's loops
    /// ([`PoolDrops::loops_not_forwarded`]).
    pub(crate) streams: SlotStreams<F>,
    /// The slots the handle added as free-running memory voices and has not
    /// placed since: a stretch is refused for them.
    pub(crate) free: Arc<Mutex<std::collections::HashSet<SlotId>>>,
    /// The slots holding a voice, by tier: every add the queue took, less
    /// every remove it took, less the adds the pool refused at its voice
    /// cap (they come back through [`collect_retired`](Self::collect_retired)).
    /// A voice never ends on its own in the pool (a one-shot stays resident,
    /// silent, until removed), so the control side alone keeps this exact.
    /// Locked before every other table, and across an add's or a remove's
    /// queueing, so two threads adding one id cannot both win.
    pub(crate) live: Arc<Mutex<HashMap<SlotId, Live>>>,
    /// The last stamp [`stamp`](Self::stamp) gave an add; shared by every clone.
    pub(crate) stamps: Arc<std::sync::atomic::AtomicU64>,
    /// The transport's loop as the control side knows it, which decides which voices
    /// carry a spare vocoder (see [`follow_loop`](Self::follow_loop)).
    pub(crate) loops: Arc<LoopFeed>,
}

/// A live slot: the tier of its voice, and the stamp of the add that made it,
/// which tells a refusal of that add from a refusal of an earlier one at the
/// same id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Live {
    pub(crate) tier: Tier,
    pub(crate) generation: u64,
    /// Where the voice sits on the timeline, as the handle last sent it: what decides
    /// whether a transport loop's start is under it.
    pub(crate) window: Option<VoiceWindow>,
    /// Whether the voice has been asked to stretch, so the pool holds a vocoder for it
    /// (one stays for the voice's life).
    pub(crate) stretching: bool,
    /// Whether the handle has sent the voice a spare vocoder it has not taken back.
    pub(crate) spare: bool,
}

impl Live {
    /// A voice of `tier` added by the add stamped `generation`, with no spare yet.
    pub(crate) fn added(
        tier: Tier,
        generation: u64,
        window: Option<VoiceWindow>,
        stretching: bool,
    ) -> Self {
        Self {
            tier,
            generation,
            window,
            stretching,
            spare: false,
        }
    }

    /// Whether the voice should carry a spare under a loop starting at `start`: a placed
    /// memory voice that stretches, whose window covers the start (a disk voice reads
    /// its wrap through its stream and has no use for one).
    fn wants_spare(&self, start: Option<Beat>) -> bool {
        self.tier == Tier::Memory
            && self.stretching
            && matches!((start, self.window), (Some(s), Some(w)) if window_covers(w, s))
    }
}

/// What the control side knows of the transport's loop: the start of the loop while
/// looping is armed, and where to read it from.
///
/// Shared by a pool and its handles. The loop's start is what decides which voices carry
/// a spare vocoder, and a handle writes it ([`VoicePoolHandle::follow_loop`]) or
/// reads it from the transport the pool is bound to ([`VoicePool::loops`](crate::VoicePool::loops)) on its
/// reclaim tick. The audio thread never reads this: it reads the loop from its block's
/// `Env`, and a spare arrives through the command ring.
#[derive(Default)]
pub(crate) struct LoopFeed {
    start: Mutex<Option<Beat>>,
    reader: Mutex<Option<LoopReader>>,
}

impl std::fmt::Debug for LoopFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopFeed")
            .field("start", &*self.start.lock())
            .field("bound", &self.reader.lock().is_some())
            .finish()
    }
}

impl LoopFeed {
    /// Reads the loop from `reader` from now on.
    pub(crate) fn bind(&self, reader: LoopReader) {
        *self.reader.lock() = Some(reader);
    }

    /// The loop's start as last written or read.
    fn start(&self) -> Option<Beat> {
        *self.start.lock()
    }

    fn set(&self, start: Option<Beat>) {
        *self.start.lock() = start;
    }

    /// Reads the bound transport's loop, if one is bound.
    fn read(&self) -> Option<Option<Beat>> {
        let reader = self.reader.lock().clone()?;
        Some(reader.read().active().map(|range| range.start()))
    }
}

/// Which tier holds a live slot's voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// An in-memory voice.
    Memory,
    /// A voice that streams from disk.
    Disk,
}

/// [`VoicePoolHandle::streams`]: the table is locked for a lookup or for
/// queueing a command beside changing it; each stream has a lock of its own,
/// held across the publish, so two commands for one slot reach its stream and
/// the pool in one order. Control threads only.
pub(crate) type SlotStreams<F> = Arc<Mutex<HashMap<SlotId, Arc<Mutex<StreamHandle<F>>>>>>;

/// A streaming add that did not reach the pool: why, and the stream, which the
/// handle takes only once the add is queued. Its loop and direction are as they
/// were before the call.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct StreamingAddError<F: SamplerFloat> {
    /// Why the add was refused, with the command (the voice) it carried.
    pub error: SendError,
    /// The stream that was to steer the voice, handed back.
    pub stream: StreamHandle<F>,
}

impl<F: SamplerFloat> VoicePoolHandle<F> {
    /// Frees everything the audio thread has retired since the last call — removed
    /// slots, surplus vocoders and replaced waves alike — and frees the ids of
    /// adds the pool refused at its voice cap ([`PoolDrops::voices_refused`]):
    /// those ids hold no voice, so a command for one is
    /// [`SendError::NoSuchSlot`] from here on.
    ///
    /// Call this periodically from the control thread — once a frame is ample.
    /// Skipping it is safe, but the retirement channel then fills, and a
    /// command that might retire a value is refused with
    /// [`SendError::RetirementFull`] until it is called.
    ///
    /// Returns how many values were freed, which is what a test can assert on.
    pub fn collect_retired(&self) -> usize {
        let mut freed = 0;
        let mut refused = Vec::new();
        for value in self.retired.lock().drain() {
            if let Retired::Refused { id, generation, .. } = &value {
                refused.push((*id, *generation));
            }
            freed += 1;
        }
        self.room.release(freed);
        // An add the pool refused at its voice cap holds no slot: its id is
        // free again and its stream, if any, ends (a send, outside the locks).
        let ended: Vec<_> = {
            let mut live = self.live();
            let mut streams = self.streams();
            refused
                .iter()
                .filter_map(|(id, generation)| {
                    // Only the add that was refused: the id may have been
                    // removed and added again since, and that voice stays.
                    if live.get(id).map(|l| l.generation) != Some(*generation) {
                        return None;
                    }
                    live.remove(id);
                    self.free.lock().remove(id);
                    streams.remove(id)
                })
                .collect()
        };
        drop(ended);
        freed
    }

    /// What the pool's drain has dropped, refused, or freed in the callback
    /// since the pool was built. Poll it as often as convenient: the counts
    /// only grow.
    pub fn drops(&self) -> PoolDrops {
        self.counters.snapshot()
    }

    /// Adds an in-memory voice: `source` played as `play` says (its gain,
    /// speed, direction, loop, stretch and pitch; the loop in `play` is the
    /// voice's, replacing any the source holds). The id must hold no voice.
    ///
    /// A voice that streams from disk is added with
    /// [`add_streaming`](Self::add_streaming), which takes the [`StreamHandle`]
    /// that steers its loop and direction: this takes only what needs no
    /// stream, so a disk voice cannot be added without one.
    ///
    /// ```compile_fail
    /// # use tutti_sampler::{DiskVoice, Playback, SlotId, VoicePool};
    /// # fn add(voice: DiskVoice) {
    /// let (_pool, handle) = VoicePool::<f32>::new().with_handle();
    /// // A disk voice has no door here: its loop and direction are its stream's.
    /// let _ = handle.add(SlotId(0), voice, Playback::default());
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// [`SendError::SlotInUse`] when `id` holds a voice, and
    /// [`SendError::Gain`] for a gain in `play` that is not finite. Otherwise
    /// [`SendError`]: the add did not reach the pool, and the voice is
    /// dropped with it. A refused add changes nothing.
    #[must_use = "a dropped add is a note that never sounds"]
    pub fn add(
        &self,
        id: SlotId,
        source: MemorySource<F>,
        play: Playback,
    ) -> Result<(), SendError> {
        GainError::check(play.gain)?;
        let free = !source.is_placed();
        if free && stretch_wanted(&play) {
            return Err(SendError::FreeRunning);
        }
        let (window, stretching) = (source.window(), stretch_wanted(&play));
        let mut live = self.live();
        if live.contains_key(&id) {
            return Err(SendError::SlotInUse(id));
        }
        let generation = self.stamp();
        self.send_noting_free(
            id,
            free,
            generation,
            VoiceCommand::AddVoice {
                id,
                voice: Box::new(Voice {
                    source: VoiceSource::Memory(source),
                    play,
                }),
                stretch: None,
            },
        )?;
        live.insert(
            id,
            Live::added(Tier::Memory, generation, window, stretching),
        );
        self.reconcile_spares(&mut live);
        Ok(())
    }

    /// Removes the voice at `id` (and ends its stream, if it streams). The
    /// slot is handed back to be freed by
    /// [`collect_retired`](Self::collect_retired), and the id is free for an
    /// [`add`](Self::add) again: the pool takes the two in the order sent.
    ///
    /// # Errors
    ///
    /// [`SendError::NoSuchSlot`] for an id holding no voice. Otherwise
    /// [`SendError`]: a dropped remove is a voice that never stops.
    #[must_use = "a dropped remove is a voice that never stops"]
    pub fn remove(&self, id: SlotId) -> Result<(), SendError> {
        let mut live = self.live();
        if !live.contains_key(&id) {
            return Err(SendError::NoSuchSlot(id));
        }
        self.send(VoiceCommand::Remove(id))?;
        self.free.lock().remove(&id);
        live.remove(&id);
        Ok(())
    }

    /// Points the in-memory voice at `id` at a different wave, keeping its
    /// window, gain and loop. Changing a disk source is a new
    /// [`add_streaming`](Self::add_streaming) on a stream opened for the other
    /// file, so a streaming voice is refused.
    ///
    /// The wave the pool lets go of is freed by
    /// [`collect_retired`](Self::collect_retired), never in the callback.
    ///
    /// # Errors
    ///
    /// [`SendError::DiskVoice`] for a streaming voice; otherwise as
    /// [`set_window`](Self::set_window). (A swap that reaches a streaming
    /// slot anyway, through a pool the handle did not add it to, is counted:
    /// [`PoolDrops::wave_swaps_ignored`].)
    #[must_use = "a dropped command changes nothing"]
    pub fn replace_wave(&self, id: SlotId, wave: Arc<Wave<F>>) -> Result<(), SendError> {
        if self.tier(id)? == Tier::Disk {
            return Err(SendError::DiskVoice(id));
        }
        self.send(VoiceCommand::ReplaceWave { id, wave })
    }

    /// Moves the voice at `id` on the timeline to `window`.
    ///
    /// # Errors
    ///
    /// [`SendError::NoSuchSlot`] for an id holding no voice. Otherwise
    /// [`SendError`]: the command did not reach the pool.
    #[must_use = "a dropped command changes nothing"]
    pub fn set_window(&self, id: SlotId, window: VoiceWindow) -> Result<(), SendError> {
        self.tier(id)?;
        self.send(VoiceCommand::UpdatePlacement { id, window })?;
        self.free.lock().remove(&id);
        let mut live = self.live();
        if let Some(entry) = live.get_mut(&id) {
            entry.window = Some(window);
        }
        self.reconcile_spares(&mut live);
        Ok(())
    }

    /// Sets the voice's gain, linear.
    ///
    /// # Errors
    ///
    /// [`SendError::Gain`] for a NaN or infinite gain; otherwise as
    /// [`set_window`](Self::set_window).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_gain(&self, id: SlotId, gain: Amplitude) -> Result<(), SendError> {
        GainError::check(gain)?;
        self.tier(id)?;
        self.send(VoiceCommand::UpdateGain { id, gain })
    }

    /// Sets the voice's varispeed (pitch follows the rate), on either tier.
    ///
    /// # Errors
    ///
    /// As [`set_window`](Self::set_window).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_speed(&self, id: SlotId, speed: PlaybackRate) -> Result<(), SendError> {
        self.tier(id)?;
        self.send(VoiceCommand::UpdateSpeed { id, speed })
    }

    /// Sets the voice's loop, or clears it with `None`; on either tier.
    ///
    /// A streaming voice's loop is published to its [`StreamHandle`]'s loop cell,
    /// which this handle holds ([`add_streaming`](Self::add_streaming)) and
    /// the streamer reads: the queue is tried first, so a full queue refuses
    /// the call before the stream hears of it and `Err` leaves both as they
    /// were.
    ///
    /// # Errors
    ///
    /// As [`set_window`](Self::set_window).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_loop(&self, id: SlotId, setting: Option<Loop>) -> Result<(), SendError> {
        self.tier(id)?;
        self.send_loop(id, setting)
    }

    /// Plays the voice forwards or backwards. A streaming voice's direction
    /// is its stream's, published as [`set_loop`](Self::set_loop) publishes a
    /// loop.
    ///
    /// # Errors
    ///
    /// As [`set_window`](Self::set_window).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_direction(&self, id: SlotId, direction: PlayDirection) -> Result<(), SendError> {
        self.tier(id)?;
        self.send_reverse(id, direction)
    }

    /// Sets the voice's time-stretch factor and pitch shift: the two controls
    /// that change duration and pitch independently, unlike
    /// [`set_speed`](Self::set_speed). The vocoder a voice needs is
    /// built here, on the calling thread.
    ///
    /// # Errors
    ///
    /// As [`set_window`](Self::set_window).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_stretch(
        &self,
        id: SlotId,
        factor: StretchFactor,
        pitch: Cents,
    ) -> Result<(), SendError> {
        self.tier(id)?;
        if stretch_values_want_filter(factor, pitch) && self.free.lock().contains(&id) {
            return Err(SendError::FreeRunning);
        }
        self.send(VoiceCommand::UpdateStretch {
            id,
            stretch_factor: factor,
            pitch_cents: pitch,
            stretch: None,
        })?;
        if stretch_values_want_filter(factor, pitch) {
            let mut live = self.live();
            if let Some(entry) = live.get_mut(&id) {
                entry.stretching = true;
            }
            self.reconcile_spares(&mut live);
        }
        Ok(())
    }

    /// Tells the handle where the transport's loop starts (`None`: not looping), so
    /// that only the stretched voices whose window covers the start carry a spare
    /// vocoder, the one a voice primes for a loop wrap with.
    ///
    /// A pool bound to a transport ([`VoicePool::loops`](crate::VoicePool::loops)) reads the loop itself on its
    /// reclaim tick; this is for a pool no transport is bound to. The handle builds a
    /// spare for each voice the loop newly covers and sends it through the command ring,
    /// and takes the spare of each voice it no longer covers back through the
    /// retirement ring ([`collect_retired`](Self::collect_retired) frees it). A voice
    /// the ring has no room for is retried on the next call or tick; until its spare
    /// arrives, a wrap it meets flushes as a seek does.
    ///
    /// Control thread.
    pub fn follow_loop(&self, region: Option<LoopRange>) {
        self.loops.set(region.map(|range| range.start()));
        self.reconcile_spares(&mut self.live());
    }

    /// The voices holding a spare the handle sent them.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn spares_sent(&self) -> usize {
        self.live().values().filter(|l| l.spare).count()
    }

    /// Sends each voice the spare its window and the loop ask for, and takes the spare of
    /// each that no longer wants one; a voice the ring has no room for stays as it was.
    /// At most one spare per resident voice, so at most `MAX_RESIDENT_VOICES`, which is
    /// what the retirement ring is sized for. Control thread, `live` locked.
    fn reconcile_spares(&self, live: &mut HashMap<SlotId, Live>) {
        let start = self.loops.start();
        for (&id, entry) in live.iter_mut() {
            let wants = entry.wants_spare(start);
            if wants == entry.spare {
                continue;
            }
            // Nothing is built or queued while the ring or the retirement room is full:
            // the refusal is counted and the next tick retries.
            if self.writer().is_full() || !self.room.has_room(MAX_RESIDENT_VOICES) {
                DrainCounters::bump(&self.counters.spares_deferred);
                continue;
            }
            let spare = wants.then(|| stretched::spare_vocoder::<F>(self.channels));
            if self
                .queue(VoiceCommand::SetSpare { id, spare }, None)
                .is_ok()
            {
                entry.spare = wants;
            } else {
                DrainCounters::bump(&self.counters.spares_deferred);
            }
        }
    }

    /// Queues a command, doing any allocation it implies here, on the calling
    /// (control) thread.
    ///
    /// This is the one chokepoint every command passes through, which makes it the
    /// right place to keep the audio thread clean: `drain_commands` runs from
    /// `process`, so anything expensive left for the drain is an allocation
    /// in the callback. That means materialising the vocoder for an
    /// `AddVoice` or an `UpdateStretch` that needs one.
    ///
    /// `Ok` means only that the command was *queued*: the pool applies it on the
    /// audio thread, and that outcome is not available synchronously.
    pub(crate) fn send(&self, cmd: VoiceCommand<F>) -> Result<(), SendError> {
        self.send_stamped(cmd, 0)
    }

    /// [`send`](Self::send) for a command that is an add stamped `generation`.
    fn send_stamped(&self, cmd: VoiceCommand<F>, generation: u64) -> Result<(), SendError> {
        match self.prepare(cmd) {
            VoiceCommand::AddVoice { id, voice, stretch } => {
                let displaced = {
                    let mut streams = self.streams();
                    self.queue_with(
                        VoiceCommand::AddVoice { id, voice, stretch },
                        None,
                        generation,
                    )?;
                    streams.remove(&id)
                };
                // The slot this replaced held a stream of its own, if any: it
                // ends (a send), outside the table's lock.
                drop(displaced);
                Ok(())
            }
            VoiceCommand::Remove(id) => {
                let displaced = {
                    let mut streams = self.streams();
                    self.queue(VoiceCommand::Remove(id), None)?;
                    streams.remove(&id)
                };
                drop(displaced);
                Ok(())
            }
            VoiceCommand::UpdateLoop { id, setting } => self.send_loop(id, setting),
            VoiceCommand::UpdateReverse { id, direction } => self.send_reverse(id, direction),
            cmd => self.queue(cmd, None),
        }
    }

    /// Adds a streaming voice, and takes the [`StreamHandle`] that feeds it: from now
    /// on the voice's loop and direction are set on that stream, and the stream
    /// ends when the voice is removed ([`remove`](Self::remove)). `voice` is the one built from the stream's own
    /// reader.
    ///
    /// `play`'s loop and direction are published to the stream first, if they
    /// differ from what it has, and the add is queued only after. A stretch
    /// vocoder the voice needs is built here.
    ///
    /// Control thread.
    ///
    /// # Errors
    ///
    /// A boxed [`StreamingAddError`] when the add did not reach the pool: the
    /// [`SendError`] (full, disconnected, or [`SlotInUse`](SendError::SlotInUse)), and the stream handed back with
    /// its loop and direction as they were before the call.
    pub fn add_streaming(
        &self,
        id: SlotId,
        voice: DiskVoice<F>,
        stream: StreamHandle<F>,
        play: Playback,
    ) -> Result<(), Box<StreamingAddError<F>>> {
        let mut live = self.live();
        if let Err(error) = GainError::check(play.gain) {
            let error = SendError::Gain(error);
            return Err(Box::new(StreamingAddError { error, stream }));
        }
        if live.contains_key(&id) {
            let error = SendError::SlotInUse(id);
            return Err(Box::new(StreamingAddError { error, stream }));
        }
        let VoiceCommand::AddVoice { id, voice, stretch } = self.prepare(VoiceCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Disk(voice),
                play,
            }),
            stretch: None,
        }) else {
            unreachable!("`prepare` returns the command kind it was given")
        };
        let generation = self.stamp();
        self.add_stream_voice(id, voice, stream, stretch, generation)?;
        live.insert(id, Live::added(Tier::Disk, generation, None, false));
        Ok(())
    }

    /// The queueing half of [`add_streaming`](Self::add_streaming): `voice`
    /// is a streaming voice, its vocoder already built.
    fn add_stream_voice(
        &self,
        id: SlotId,
        voice: Box<Voice<F>>,
        stream: StreamHandle<F>,
        stretch: Option<Box<Vocoders<F>>>,
        generation: u64,
    ) -> Result<(), Box<StreamingAddError<F>>> {
        if self.writer().is_full() {
            let error = SendError::Full;
            return Err(Box::new(StreamingAddError { error, stream }));
        }
        // What the stream had, to put back if the add cannot be queued.
        let (was_loop, was_direction) = (stream.loop_(), stream.direction());
        ask(stream.looping_cell(), voice.play.loop_);
        ask(stream.direction_cell(), voice.play.direction);
        let queued = {
            let mut streams = self.streams();
            let cmd = VoiceCommand::AddVoice { id, voice, stretch };
            match self.queue_with(cmd, None, generation) {
                Ok(()) => Ok(streams.insert(id, Arc::new(Mutex::new(stream)))),
                Err(error) => Err((error, stream)),
            }
        };
        match queued {
            // The slot this replaced held a stream of its own, if any: it ends
            // (a send), outside the table's lock.
            Ok(displaced) => {
                drop(displaced);
                Ok(())
            }
            // Another thread filled the queue meanwhile: the stream goes back
            // as it came.
            Err((error, stream)) => {
                ask(stream.looping_cell(), was_loop);
                ask(stream.direction_cell(), was_direction);
                Err(Box::new(StreamingAddError { error, stream }))
            }
        }
    }

    /// Queues the loop command setting `setting` on slot `id`, and publishes
    /// it to the slot's stream when the handle holds one. The queue first: a
    /// full one refuses the command with nothing changed.
    fn send_loop(&self, id: SlotId, setting: Option<Loop>) -> Result<(), SendError> {
        let cmd = VoiceCommand::UpdateLoop { id, setting };
        let Some(entry) = self.stream_of(id) else {
            // An in-memory (or unknown) slot: nothing to set it on.
            return self.queue(cmd, None);
        };
        let stream = entry.lock();
        self.queue(cmd, Some(stream.id()))?;
        ask(stream.looping_cell(), setting);
        Ok(())
    }

    /// [`send_loop`](Self::send_loop) for the direction.
    fn send_reverse(&self, id: SlotId, direction: PlayDirection) -> Result<(), SendError> {
        let cmd = VoiceCommand::UpdateReverse { id, direction };
        let Some(entry) = self.stream_of(id) else {
            // An in-memory slot reverses its own index; a streaming slot
            // without a stream here is counted by the drain.
            return self.queue(cmd, None);
        };
        let stream = entry.lock();
        self.queue(cmd, Some(stream.id()))?;
        ask(stream.direction_cell(), direction);
        Ok(())
    }

    /// The stream the handle holds for slot `id`, if any.
    pub(crate) fn stream_of(&self, id: SlotId) -> Option<Arc<Mutex<StreamHandle<F>>>> {
        self.streams().get(&id).cloned()
    }

    /// Queues `cmd`, an add at `id`, and notes whether the slot is
    /// free-running.
    fn send_noting_free(
        &self,
        id: SlotId,
        free: bool,
        generation: u64,
        cmd: VoiceCommand<F>,
    ) -> Result<(), SendError> {
        self.send_stamped(cmd, generation)?;
        let mut set = self.free.lock();
        if free {
            set.insert(id);
        } else {
            set.remove(&id);
        }
        Ok(())
    }

    /// A stamp no earlier add of this handle (or a clone) has had.
    fn stamp(&self) -> u64 {
        self.stamps
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// The live-slot table, locked.
    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<SlotId, Live>> {
        self.live.lock()
    }

    /// The tier holding slot `id`'s voice, or [`SendError::NoSuchSlot`].
    fn tier(&self, id: SlotId) -> Result<Tier, SendError> {
        self.live()
            .get(&id)
            .map(|l| l.tier)
            .ok_or(SendError::NoSuchSlot(id))
    }

    /// The command ring's writer, locked.
    fn writer(&self) -> std::sync::MutexGuard<'_, RingWriter<Queued<F>>> {
        self.tx.lock()
    }

    /// The stream table, locked.
    fn streams(&self) -> std::sync::MutexGuard<'_, HashMap<SlotId, Arc<Mutex<StreamHandle<F>>>>> {
        self.streams.lock()
    }

    /// Queues `cmd` for the drain, with the stream its loop was set on.
    fn queue(&self, cmd: VoiceCommand<F>, forwarded: Option<StreamId>) -> Result<(), SendError> {
        self.queue_with(cmd, forwarded, 0)
    }

    /// [`queue`](Self::queue) for an add stamped `generation`.
    fn queue_with(
        &self,
        cmd: VoiceCommand<F>,
        forwarded: Option<StreamId>,
        generation: u64,
    ) -> Result<(), SendError> {
        let mut writer = self.writer();
        if writer.is_full() {
            return Err(SendError::Full);
        }
        // One retirement slot per command that might retire a value, taken
        // before the command is visible to the drain and given back by the
        // drain when the command retires nothing, or by `collect_retired`
        // when the value is taken. The drain's push therefore always fits.
        let reserves = cmd.may_retire();
        let is_spare = matches!(cmd, VoiceCommand::SetSpare { .. });
        if reserves && !self.room.reserve(MAX_RESIDENT_VOICES) {
            return Err(SendError::RetirementFull);
        }
        if let Err(r) = writer.push(Queued {
            cmd,
            forwarded,
            generation,
        }) {
            if reserves {
                self.room.release(1);
            }
            return Err(match r.error {
                PushError::Disconnected => SendError::Disconnected,
                PushError::Full => SendError::Full,
            });
        }
        // A spare changes no voice: a pool whose handle has only sent spares still holds
        // the voices it was built with, which lets a fork copy it.
        if !is_spare {
            self.counters.note_queued();
        }
        Ok(())
    }

    /// Move control-thread work out of the drain. Runs on the caller's thread.
    fn prepare(&self, cmd: VoiceCommand<F>) -> VoiceCommand<F> {
        match cmd {
            VoiceCommand::AddVoice {
                id,
                voice,
                stretch: None,
            } if stretch_wanted(&voice.play) => {
                let vocoder = stretched::vocoder::<F>(self.channels, false);
                VoiceCommand::AddVoice {
                    id,
                    voice,
                    stretch: Some(vocoder),
                }
            }
            // Same materialisation for an update that turns stretching ON.
            // Whether the slot already has a processor is audio-thread state and
            // unreadable from here, so build unconditionally when the new values
            // ask for stretch and let the drain retire a redundant one. Paying an
            // occasional wasted control-thread allocation is the right trade
            // against the alternatives: querying the slot needs a round-trip, and
            // building in the drain allocates in the callback.
            VoiceCommand::UpdateStretch {
                id,
                stretch_factor,
                pitch_cents,
                stretch: None,
            } if stretch_values_want_filter(stretch_factor, pitch_cents) => {
                let vocoder = stretched::vocoder::<F>(self.channels, false);
                VoiceCommand::UpdateStretch {
                    id,
                    stretch_factor,
                    pitch_cents,
                    stretch: Some(vocoder),
                }
            }
            other => other,
        }
    }
}

/// Frees what the control side retired and no block holds, without publishing
/// (the placement windows a replacement left behind); control thread, from the owner's tick.
impl<F: SamplerFloat> tutti_rt::Reclaim for VoiceNodeHandle<F> {
    fn reclaim(&self) -> usize {
        self.steering.reclaim()
            + self.window.reclaim()
            + self.looping.reclaim()
            + self.direction.reclaim()
            + self.speed.reclaim()
    }
}

/// Frees the slots the pool's audio side retired
/// ([`collect_retired`](VoicePoolHandle::collect_retired)), so the pool's
/// owner's tick keeps the retire ring from filling, and the loops and
/// directions its streams' control side replaced; control thread only.
impl<F: SamplerFloat> tutti_rt::Reclaim for VoicePoolHandle<F> {
    fn reclaim(&self) -> usize {
        self.collect_retired();
        // The transport's loop as it stands now, and any spare a full ring held back.
        if let Some(start) = self.loops.read() {
            self.loops.set(start);
        }
        self.reconcile_spares(&mut self.live());
        let streams: Vec<_> = self.streams().values().cloned().collect();
        streams.iter().map(|s| s.lock().reclaim()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::pool::VoicePool;

    /// A full retirement room refuses a spare on the control side: `SetSpare` reserves a
    /// retirement slot like every command that may retire, so with none left nothing is
    /// queued, the refusal is counted (`PoolDrops::spares_deferred`), and the next tick
    /// sends it once a slot is free. No box reaches the audio thread to be dropped there.
    #[test]
    fn a_full_retirement_room_defers_the_spare_and_the_next_tick_sends_it() {
        let (_pool, handle) = VoicePool::<f32>::new().with_handle();
        let wave = Arc::new(Wave::new(2, tutti_units::SampleRate::SR_48K));
        let source = MemorySource::placed(wave, VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave");
        handle
            .add(
                SlotId(1),
                source,
                Playback {
                    stretch: StretchFactor::new(1.5),
                    ..Default::default()
                },
            )
            .expect("room");
        // Every retirement slot taken, as 64 uncollected retirements would hold them.
        while handle.room.reserve(MAX_RESIDENT_VOICES) {}
        let range = LoopRange::new(Beat::new(0.0), Beat::new(1.0)).expect("a loop");
        handle.follow_loop(Some(range));
        assert_eq!(handle.spares_sent(), 0, "nothing was sent");
        let deferred = handle.drops().spares_deferred;
        assert!(deferred >= 1, "the refusal is counted");
        tutti_rt::Reclaim::reclaim(&handle);
        assert!(
            handle.drops().spares_deferred > deferred,
            "a tick without room defers again"
        );
        assert_eq!(handle.spares_sent(), 0, "still nothing sent");
        handle.room.release(1);
        tutti_rt::Reclaim::reclaim(&handle);
        assert_eq!(handle.spares_sent(), 1, "the next tick with room sends it");
    }

    /// A full queue is reported to the caller, not logged and forgotten.
    ///
    /// A `tracing::warn!` reaches an operator reading logs, never the code that
    /// could back off or retry — and a dropped `AddVoice` is a note that never
    /// sounds.
    #[test]
    fn a_full_queue_is_reported_to_the_caller() {
        // Build the pool but never drain it, so the queue fills.
        let (_pool, handle) = VoicePool::<f32>::new().with_handle();

        for _ in 0..COMMAND_CAPACITY {
            handle
                .send(VoiceCommand::Remove(SlotId(1)))
                .expect("within capacity");
        }

        let err = handle
            .send(VoiceCommand::Remove(SlotId(2)))
            .expect_err("past capacity the command is dropped");
        assert!(
            !err.is_disconnected(),
            "a full queue is transient, not a dead pool"
        );
        assert_eq!(err, SendError::Full);
    }

    /// A dropped pool is reported as permanent. Without it every later send is a
    /// no-op with nothing to notice it by.
    #[test]
    fn a_dropped_pool_is_reported_as_disconnected() {
        let (pool, handle) = VoicePool::<f32>::new().with_handle();
        drop(pool);

        let err = handle
            .send(VoiceCommand::Remove(SlotId(1)))
            .expect_err("nothing will ever drain this queue again");
        assert!(
            err.is_disconnected(),
            "a dead pool must be distinguishable from a merely full one — the \
             caller should stop, not retry"
        );
    }
}
