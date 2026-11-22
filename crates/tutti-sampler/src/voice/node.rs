//! [`VoiceNode`] — a single [`Voice`] as its own graph node.
//!
//! Zero inputs, N outputs, and typed controls ([`VoiceNodeHandle`], what its
//! `IntoNode` hands back): its gain, a scalar cell the node reads once per
//! block, its window, a published cell it lends once per block, and its loop,
//! speed and play state, published values it reads once per block. For
//! resynth, preview and a single timeline clip, where a whole pool would be
//! ceremony. It shares `PlaybackSlot` with
//! [`VoicePool`](super::pool::VoicePool), so the per-voice read is the same
//! code in both — a fix in one is a fix in both.

use std::sync::Arc;

use crate::lanes::{BlockScratch, UNPREPARED_FRAMES};
use crate::streamer::{ControlCell, StreamHandle};
use tutti_buffers::MAX_CHANNELS;

use super::clock::{BlockClock, Clock};
use super::command::{GainError, Steering, SteeringCell, VoiceNodeHandle, ask};
use super::disk_voice::DiskVoice;
use super::disk_voice::VoiceHealth;
use super::loop_span::Loop;
use super::memory_source::VoiceWindow;
use super::slot::{PlaybackSlot, stretch_wanted};
use super::stretched;
use super::types::{PlayDirection, Playback, Reach, SlotId, Voice, VoiceSource};
use crate::{PlaceError, SamplerFloat};
use tutti_buffers::ChannelLayout;
use tutti_graph::{
    Cx, ForkCause, ForkSource, Forked, IntoNode, Io, Node, NodeParts, ParamSet, Prepare,
    RenderMode, Shape, Status,
};
use tutti_rt::{Param, Publisher, Reader};
use tutti_timeline::TempoReader;
use tutti_units::{Amplitude, PlaybackRate, Tail, UnitParam};

/// Why a [`VoiceNode`] cannot be built over a voice.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum VoiceNodeError {
    /// The voice's [`Playback`] has a gain that is not finite.
    #[error(transparent)]
    Gain(#[from] GainError),
    /// The node's width is none, or past what the sampler reads.
    #[error(transparent)]
    Width(#[from] PlaceError),
}

/// One [`Voice`] as a graph node in its own right: zero inputs, `channels`
/// outputs.
///
/// Where [`VoicePool`](super::pool::VoicePool) holds a *list* of voices and sums
/// them, this holds exactly ONE and plays it — a degenerate single-voice
/// mixdown. Its block read is the same per-voice read the pool does for one
/// slot, shared through the slot itself, so there is no duplication and no
/// per-sample dynamic dispatch.
///
/// # Controls
///
/// Inserted into a graph (`IntoNode`), it hands back a [`VoiceNodeHandle`]:
///
/// - gain — a `Param<Amplitude>` cell, also addressable as
///   [`UnitParam::Volume`] through the handle's [`ParamSet`]. The node reads
///   it once per block into the voice's `Playback` record and its source.
/// - window: `Some` places the voice on the transport, `None` leaves it
///   free-running ([`VoiceNodeHandle::window_cell`]); a `Publisher` cell the
///   node lends once per block, latest wins, allocation-free.
/// - loop, speed and play state: not scalars, so the handle publishes
///   them to a `Publisher` cell the node reads once per block, latest wins,
///   allocation-free. A disk voice's loop is its [`StreamHandle`]'s, which the
///   handle owns ([`VoiceNode::streaming`]).
///
/// # Forks
///
/// The node's fork source keeps a copy of the voice as it was inserted, and a
/// fork is that copy made to share nothing with the live node
/// (`Voice::fork_copy`: its own gain cell; a disk voice reading its file
/// itself), moved to the window, loop and speed the handle last published and
/// set to the gain last set, with a vocoder of its own. It reads its render's
/// transport from its block's `Env`. A disk voice's node
/// forks only for an offline render.
///
/// A stretching voice's seat (a seek, play from a position the transport has not stood
/// at for 2 049 frames; a loop wrap is primed for, by a second vocoder the voice carries) primes the vocoder over blocks and is silent for
/// 4 097 frames (85 ms at 48 kHz) before it sounds on the timeline; a clip placed ahead
/// of the playhead, or at a position the transport has stood at, primes before it
/// plays and is not silent.
pub struct VoiceNode<F: SamplerFloat> {
    /// The single voice plus its resident vocoder and the per-sample
    /// read, shared with the pool's slots.
    pub(crate) slot: PlaybackSlot<F>,
    /// Output width — the node's audio outputs, fixed at construction.
    /// Declared rather than inferred from the voice, so an edge wired against
    /// it cannot be re-arityed under a live graph.
    pub(crate) channels: ChannelLayout,
    /// A reader of the steering the handle publishes, lent once per block,
    /// registered on the `Publisher` the handle and the fork source share. Empty until a handle sets a field, so a node driven
    /// by hand keeps what it was built with.
    steering: Reader<Steering>,
    /// The steering last applied, so a block applies a field once, when it
    /// changes, rather than every block.
    applied: Steering,
    /// A reader of the window the handle publishes, lent once per block.
    window: Reader<Option<VoiceWindow>>,
    /// The window last applied.
    applied_window: Option<VoiceWindow>,
    /// A reader of the loop the handle publishes, lent once per block.
    looping: Reader<Option<Loop>>,
    /// The loop last applied.
    applied_loop: Option<Loop>,
    /// A reader of the direction the handle publishes, lent once per block.
    direction: Reader<PlayDirection>,
    /// The direction last applied.
    applied_direction: PlayDirection,
    /// A reader of the speed the handle publishes, lent once per block.
    speed: Reader<PlaybackRate>,
    /// The speed last applied.
    applied_speed: PlaybackRate,
    /// The disk stream of a streaming node, until [`with_handle`](Self::with_handle)
    /// hands it to the handle: a stream is control-thread only (its drop
    /// sends on a bounded queue), so the node never keeps one it plays with.
    stream: Option<StreamHandle<F>>,
    /// The gain the handle writes; read once per block.
    gain: Param<Amplitude>,
    /// The transport as the voice reads it, kept across blocks so a jump is
    /// flushed where it happens.
    clock: Clock,
    /// The project's tempo map, when the node reads one: lent once per block
    /// to its clock ([`tempo`](Self::tempo)).
    tempo: Option<TempoReader>,
    /// The block read's lanes (`PlaybackSlot::render_into`), built here and
    /// sized from `Prepare::max_block` in `prepare`, on the control thread.
    scratch: BlockScratch<F>,
    /// Whether a handle still reaches the node; a fork's is frozen. It
    /// decides how exactly the node states its ring-out
    /// ([`ring_out`](Self::ring_out)).
    reach: Reach,
}

impl<F: SamplerFloat> VoiceNode<F> {
    /// Wraps a single [`Voice`] as a standalone stereo graph node. Builds the
    /// resident stretch processor once (like a mixer slot), off any hot path.
    ///
    /// # Errors
    ///
    /// [`VoiceNodeError::Gain`] for a `voice.play` gain that is not finite.
    pub fn new(voice: Voice<F>) -> Result<Self, VoiceNodeError> {
        Self::with_channels(voice, ChannelLayout::STEREO)
    }

    /// Wraps a single [`Voice`] as a `channels`-wide graph node.
    ///
    /// Builds the vocoder here when `voice.play` asks for one. This
    /// is a control-thread constructor, so the allocation is free; the
    /// audio-thread drain in [`VoicePool`](super::pool::VoicePool) cannot do the
    /// same and takes a pre-built vocoder from the sender instead.
    ///
    /// Skipping that step is how a standalone stretched voice reads DRY forever
    /// — no stretch, no pitch shift, and no error — because
    /// `PlaybackSlot::with_channels` always leaves the field `None` and nothing
    /// else on this path fills it in.
    ///
    /// # Errors
    ///
    /// [`VoiceNodeError::Gain`] for a `voice.play` gain that is not finite, and
    /// [`VoiceNodeError::Width`] for a width of no channels or past the
    /// sampler's ceiling ([`voice_width`](crate::voice_width)).
    pub fn with_channels(
        voice: Voice<F>,
        channels: impl Into<ChannelLayout>,
    ) -> Result<Self, VoiceNodeError> {
        let channels = crate::voice_width(channels.into())?;
        GainError::check(voice.play.gain)?;
        let stretch = stretch_wanted(&voice.play).then(|| stretched::vocoder::<F>(channels, true));
        let gain = Param::new(voice.play.gain);
        let window = voice.source.window();
        let play = voice.play;
        let mut slot = PlaybackSlot::with_channels(SlotId(0), Box::new(voice), channels);
        slot.stretch = stretch;
        Ok(Self {
            slot,
            channels,
            steering: SteeringCell::default().reader(),
            applied: Steering::default(),
            window: Publisher::new(window).reader(),
            applied_window: window,
            looping: ControlCell::new(play.loop_).reader(),
            applied_loop: play.loop_,
            direction: ControlCell::new(play.direction).reader(),
            applied_direction: play.direction,
            speed: Publisher::new(play.speed).reader(),
            applied_speed: play.speed,
            stream: None,
            gain,
            clock: Clock::new(),
            tempo: None,
            scratch: BlockScratch::new(UNPREPARED_FRAMES, channels.count() as usize),
            reach: Reach::Live,
        })
    }

    /// A node over a streaming voice, and the [`StreamHandle`] that feeds it: the
    /// handle ([`with_handle`](Self::with_handle), or the graph's insert)
    /// takes the stream, so the voice sounds for as long as the handle lives,
    /// and its loop and direction are the stream's cells, through the handle.
    /// `play`'s loop and direction are published to the stream first, as
    /// [`VoicePoolHandle::add_streaming`](super::VoicePoolHandle::add_streaming)
    /// publishes them. `voice` is the one built from the stream's own reader.
    ///
    /// A node never handed to a handle keeps the stream until it is dropped:
    /// built by hand, drop it on the control thread.
    ///
    /// # Errors
    ///
    /// [`VoiceNodeError::Gain`] for a `play` gain that is not finite; the
    /// stream is dropped with the error.
    pub fn streaming(
        mut voice: DiskVoice<F>,
        stream: StreamHandle<F>,
        play: Playback,
    ) -> Result<Self, VoiceNodeError> {
        GainError::check(play.gain)?;
        ask(stream.looping_cell(), play.loop_);
        ask(stream.direction_cell(), play.direction);
        voice.store_gain(play.gain);
        voice.set_speed(play.speed);
        let width = ChannelLayout::from_count(voice.width() as u16);
        let mut node = Self::with_channels(
            Voice {
                source: VoiceSource::Disk(voice),
                play,
            },
            width,
        )?;
        node.stream = Some(stream);
        Ok(node)
    }

    /// This node reading the project's tempo map through `tempo`
    /// ([`TransportSettings::tempo_reader`](tutti_timeline::TransportSettings::tempo_reader)).
    /// Over a map whose tempo changes, how long a placed voice has played is
    /// the map's seconds between its start and the playhead, not the beats
    /// over the tempo of the moment; see
    /// [`VoicePool::tempo`](super::VoicePool::tempo). It matters
    /// only when the voice is placed on the transport. A fork reads the same map.
    #[must_use]
    pub fn tempo(mut self, tempo: TempoReader) -> Self {
        self.tempo = Some(tempo);
        self
    }

    /// This node with a fresh steering cell, and the handle that drives
    /// it: what [`into_parts`](IntoNode::into_parts) hands the graph and the
    /// caller. For a host (or a test) that calls the node by hand. What was
    /// set through a handle taken before no longer reaches the node (its
    /// gain still does: that cell is the node's for life). A streaming node's
    /// stream moves into the handle.
    pub fn with_handle(mut self) -> (Self, VoiceNodeHandle<F>) {
        let steering = SteeringCell::default();
        self.steering = steering.reader();
        let window = Publisher::new(self.slot.voice.source.window());
        self.window = window.reader();
        self.applied_window = self.slot.voice.source.window();
        // A streamed voice's loop and direction are its stream's cells, which
        // the streamer reads; a memory voice's are the node's alone.
        let play = self.slot.voice.play;
        let (looping, direction) = match &self.stream {
            Some(stream) => (
                stream.looping_cell().clone(),
                stream.direction_cell().clone(),
            ),
            None => (
                ControlCell::new(play.loop_),
                ControlCell::new(play.direction),
            ),
        };
        self.looping = looping.reader();
        self.applied_loop = *looping.current();
        self.direction = direction.reader();
        self.applied_direction = *direction.current();
        let speed = Publisher::new(play.speed);
        self.speed = speed.reader();
        self.applied_speed = play.speed;
        let params = self.param_set();
        let handle = VoiceNodeHandle::new(
            steering,
            window,
            looping,
            direction,
            speed,
            self.gain.handle(),
            params,
            self.stream.take(),
        );
        (self, handle)
    }

    /// The node's params by address: its gain, as [`UnitParam::Volume`].
    /// The cells the node reads, so a set taken before the node is inserted
    /// addresses the inserted node (it is the set the handle carries).
    pub fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// Apply the published steering, and the gain. Runs at the top of each
    /// block.
    ///
    /// Allocation- and free-free by construction: one `Publisher` read (a
    /// borrow that cannot free, dropped before the block's read), a `Copy`
    /// value, and a field write when one changed. The pool's drain has to be
    /// more careful because its commands carry owned payloads (a `Voice`, a
    /// wave, a vocoder), which is why it retires what it lets go of.
    /// A node takes no such payload: there is nothing for it to drop.
    ///
    /// `driven` is the block's gain from the node's param port when a lane
    /// drives it (its last frame); `None` reads the cell.
    #[inline]
    fn drain_commands(&mut self, driven: Option<F>) {
        let published = *self.steering.lend();
        if published != self.applied {
            apply_steering(&mut self.slot.voice, &self.applied, &published);
            self.applied = published;
        }
        let window = *self.window.lend();
        if window != self.applied_window {
            self.slot.voice.source.apply_window(window);
            self.applied_window = window;
        }
        let looping = *self.looping.lend();
        if looping != self.applied_loop {
            apply_loop(&mut self.slot.voice, looping);
            self.applied_loop = looping;
        }
        let direction = *self.direction.lend();
        if direction != self.applied_direction {
            self.slot.voice.play.direction = direction;
            self.applied_direction = direction;
        }
        let speed = *self.speed.lend();
        if speed != self.applied_speed {
            apply_speed(&mut self.slot.voice, speed);
            self.applied_speed = speed;
        }
        // Both halves of the gain: `Playback` is what the memory tier's read
        // scales by, the source's own cell what the disk tier's does.
        // A cell holding a NaN or an infinity (a host can write anything into
        // it) is the gain the voice already has.
        let gain = driven.map_or_else(
            || self.gain.load(),
            |v| Amplitude::new(Into::<f64>::into(v).max(0.0)),
        );
        let gain = if gain.get().is_finite() {
            gain
        } else {
            self.slot.voice.play.gain
        };
        if gain != self.slot.voice.play.gain {
            self.slot.voice.play.gain = gain;
            self.slot.voice.source.apply_gain(gain);
        }
    }

    /// Output width — the node's audio outputs.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// The wrapped voice (immutable view).
    pub fn voice(&self) -> &Voice<F> {
        &self.slot.voice
    }

    /// A block longer than the scratch, in pieces as long as it: only a
    /// node driven by hand without a `prepare` (its `UNPREPARED_FRAMES`) or
    /// past the `max_block` it was prepared for reaches this.
    #[cold]
    #[inline(never)]
    fn render_in_pieces(
        &mut self,
        clock: &BlockClock<'_>,
        frames: usize,
        n: usize,
        out: &mut [&mut [F]],
    ) {
        let piece = self.scratch.frames();
        let mut from = 0;
        while from < frames {
            let to = (from + piece).min(frames);
            self.slot
                .render_into(clock, from..to, n, &mut self.scratch, out);
            from = to;
        }
    }

    /// The ring-out the node declares ([`Shape::tail`]): its voice's
    /// ([`Voice::ring_out`]), promised as a ceiling while a handle can still
    /// steer the node and exactly in a fork, which none reaches.
    fn ring_out(&self) -> Tail {
        self.slot.voice.ring_out(self.reach)
    }

    /// What the node declares to the cost model: `Moderate` for a plain
    /// read, `Heavy` with a vocoder.
    ///
    /// Measured: a plain memory voice 13 ns
    /// per frame and channel, a stretched one 288 (the phase vocoder),
    /// against `SvfFilterNode`'s 7.2 and `Heavy`'s 256. A node's vocoder is
    /// built with it or never (`UpdateStretch` reaches only a pool's
    /// slots), so every read of the shape (at insert and at each
    /// re-prepare) gives the same class. A disk voice reads through the
    /// same kernel as a memory one.
    fn cost(&self) -> tutti_graph::Cost {
        if self.slot.stretch.is_some() {
            tutti_graph::Cost::Heavy
        } else {
            tutti_graph::Cost::Moderate
        }
    }

    /// Its scratch's frames and width.
    #[cfg(test)]
    pub(crate) fn scratch_size(&self) -> (usize, usize) {
        (self.scratch.frames(), self.scratch.width())
    }
}

// Hand-rolled: wraps a non-`Debug` `PlaybackSlot`. Print the wrapped `Voice`
// (which is `Debug`) and leave the resident stretch DSP out.
impl<F: SamplerFloat> std::fmt::Debug for VoiceNode<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceNode")
            .field("voice", &self.slot.voice)
            .finish_non_exhaustive()
    }
}

impl<F: SamplerFloat> TryFrom<Voice<F>> for VoiceNode<F> {
    type Error = VoiceNodeError;

    fn try_from(voice: Voice<F>) -> Result<Self, VoiceNodeError> {
        Self::new(voice)
    }
}

// The param a `VoiceNode` lets the graph modulate: its gain, as
// `UnitParam::Volume`. Block-rate in the voice (read once per block, as its
// cell is), so a driven port is read the same way: its value on the block's
// last frame.
tutti_graph::declare_params!([F: SamplerFloat] VoiceNode<F>, n {
    Volume => Amplitude = &n.gain,
});

impl<F: SamplerFloat> Node<F> for VoiceNode<F> {
    /// No inputs, [`channels`](Self::channels) outputs; a source, never
    /// skipped, with the ring-out of `ring_out`.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.channels)
            .tail(self.ring_out())
            .cost(self.cost())
            .param_addrs(self.declared_ports().as_slice())
    }

    /// The rate the voice's source runs at, and the
    /// block scratch sized from `max_block` (a re-prepare at another size
    /// resizes it).
    fn prepare(&mut self, p: &Prepare) {
        self.scratch
            .resize(p.max_block().get(), self.channels.count() as usize);
        let sample_rate = p.sample_rate();
        self.slot
            .voice
            .source
            .set_render(sample_rate, p.render_mode());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        self.drain_commands(
            self.declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
                .last_frame(),
        );
        // Lent for the block and given back after it: the render borrows the
        // whole node.
        let mut reader = self.tempo.take();
        let map = reader.as_mut().map(TempoReader::lend);
        let clock = self.clock.observe_with(cx.env, map.as_deref());
        let frames = io.frames();
        let (_, mut outs) = io.split();
        // Every output this node declares, not only the `n` the read writes:
        // a node wider than `MAX_CHANNELS` leaves the rest silent
        // rather than holding whatever the buffer held.
        for ch in outs.iter_mut() {
            ch.fill(F::ZERO);
        }
        let n = (self.channels.count() as usize)
            .min(outs.len())
            .min(MAX_CHANNELS);
        let mut refs: [&mut [F]; MAX_CHANNELS] = std::array::from_fn(|_| Default::default());
        for (slot, ch) in refs.iter_mut().zip(outs.iter_mut()) {
            *slot = ch;
        }
        if self.applied.stopped {
            // Silent, and nothing read. A disk voice's ring read by position
            // is dropped, so the block it resumes in reads from the playhead
            // as a jump does; a free-running memory voice keeps its cursor.
            if let VoiceSource::Disk(disk) = &mut self.slot.voice.source {
                disk.flush();
            }
            drop(map);
            self.tempo = reader;
            return Status::Modified;
        }
        // One piece: in a graph, `prepare` sized the scratch for every block.
        if frames <= self.scratch.frames() {
            self.slot
                .render_into(&clock, 0..frames, n, &mut self.scratch, &mut refs[..n]);
        } else {
            self.render_in_pieces(&clock, frames, n, &mut refs[..n]);
        }
        drop(map);
        self.tempo = reader;
        Status::Modified
    }

    /// The voice's buffered audio flushed (one definition shared with a
    /// jump's flush, so a jump can never flush less than a reset does), the
    /// transport forgotten.
    fn reset(&mut self) {
        self.slot.flush_playhead_state();
        self.clock.reset();
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

/// Applies a loop: a memory voice takes it, and a disk voice's is its stream's,
/// which the streamer applies to the ring (the voice's record follows it).
fn apply_loop<F: SamplerFloat>(voice: &mut Voice<F>, setting: Option<Loop>) {
    if let VoiceSource::Memory(source) = &mut voice.source {
        source.set_loop(setting);
    }
    voice.play.loop_ = setting;
}

/// Applies what `now` publishes over what `was` already had applied, a field
/// at a time and only the ones that changed (a loop set again would restart
/// the free-running cursor's lap). A disk voice's loop is its stream's, so
/// only a memory voice takes one here.
fn apply_steering<F: SamplerFloat>(voice: &mut Voice<F>, was: &Steering, now: &Steering) {
    if now.plays != was.plays
        && let VoiceSource::Memory(source) = &voice.source
    {
        source.play();
    }
}

/// Applies a speed: the source's, and the record the voice keeps of it.
fn apply_speed<F: SamplerFloat>(voice: &mut Voice<F>, speed: PlaybackRate) {
    voice.source.apply_speed(speed);
    voice.play.speed = speed;
}

/// A voice node's fork source: see "Forks" on [`VoiceNode`].
struct VoiceNodeFork<F: SamplerFloat> {
    /// The voice as inserted. Shares with the live node what a `Voice`
    /// clone shares (a memory voice's gain cell, a disk voice's stream
    /// record and control cell, never its ring reader); a fork copies it
    /// with `Voice::fork_copy`, which shares nothing.
    template: Voice<F>,
    channels: ChannelLayout,
    /// The live node's steering cell, read with `current` when a fork is
    /// taken (an owning read on the control side, never a lend).
    steering: SteeringCell,
    /// The live node's window, loop, direction and speed cells, read the same way.
    window: Publisher<Option<VoiceWindow>>,
    looping: ControlCell<Option<Loop>>,
    direction: ControlCell<PlayDirection>,
    speed: Publisher<PlaybackRate>,
    /// The node's params: a fork starts from the gain cell, which holds the
    /// gain last set (a modulation lane sends its composite to the
    /// port and never writes the cell).
    params: ParamSet,
    /// The tempo map the live node reads, which a fork reads too.
    tempo: Option<TempoReader>,
}

impl<F: SamplerFloat> ForkSource<F> for VoiceNodeFork<F> {
    fn fork(&self, mode: RenderMode) -> Result<Forked<F>, ForkCause> {
        let mut voice = self.template.fork_copy(mode)?;
        apply_steering(&mut voice, &Steering::default(), &self.steering.current());
        // The window is the source's alone: `Playback` has no window field
        // (two copies kept in sync by hand is how a fork reaches the wrong
        // one), so there is exactly one write.
        voice.source.apply_window(*self.window.current());
        apply_loop(&mut voice, *self.looping.current());
        voice.play.direction = *self.direction.current();
        apply_speed(&mut voice, *self.speed.current());
        let gain = self
            .params
            .get(UnitParam::Volume)
            .map_or(voice.play.gain, Amplitude::new);
        voice.play.gain = gain;
        voice.source.apply_gain(gain);
        let health = voice.fault().map(VoiceHealth);
        // A driven gain that is not finite refuses the fork, as the node's own
        // constructor does.
        let mut node = VoiceNode::with_channels(voice, self.channels).map_err(ForkCause::new)?;
        node.tempo = self.tempo.clone();
        node.reach = Reach::Frozen;
        let forked = Forked::new(Box::new(node));
        Ok(match health {
            Some(h) => forked.health(Arc::new(h)),
            None => forked,
        })
    }
}

impl<F: SamplerFloat> IntoNode<F> for VoiceNode<F> {
    type Controls = VoiceNodeHandle<F>;

    fn into_parts(self) -> NodeParts<VoiceNodeHandle<F>, F> {
        let (node, handle) = self.with_handle();
        let fork = VoiceNodeFork {
            template: node.slot.voice.as_ref().clone(),
            channels: node.channels,
            steering: handle.steering.clone(),
            window: handle.window.clone(),
            looping: handle.looping.clone(),
            direction: handle.direction.clone(),
            speed: handle.speed.clone(),
            params: handle.params.clone(),
            tempo: node.tempo.clone(),
        };
        let params = handle.params().clone();
        NodeParts::new(
            Box::new(node),
            handle.clone(),
            Some(Box::new(fork)),
            Some(Box::new(handle)),
        )
        .params(params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemorySource, Playback, VoiceSource, VoiceWindow};
    use tutti_buffers::Mono;
    use tutti_units::{Beat, SampleRate};

    /// A window the node held across the next publish is freed by the next
    /// `reclaim`, with no further publish.
    #[test]
    fn reclaim_frees_a_window_a_block_held_across_the_next() {
        let wave = Arc::new(tutti_buffers::Wave::from_mono(
            SampleRate::SR_48K,
            Mono::new(&[0.0; 64]),
        ));
        let voice = Voice {
            source: VoiceSource::Memory(MemorySource::new(wave).expect("a placeable wave")),
            play: Playback::default(),
        };
        let (mut node, handle) = VoiceNode::new(voice)
            .expect("a finite gain and a nonempty width")
            .with_handle();
        {
            // A block in progress across the move.
            let _held = node.window.lend();
            handle
                .window_cell()
                .publish(Some(VoiceWindow::from(Beat::new(1.0))));
        }
        assert_eq!(handle.window.retired_len(), 1, "pinned until a scan");
        assert_eq!(tutti_rt::Reclaim::reclaim(&handle), 0);
        assert_eq!(handle.window.retired_len(), 0, "freed by the reclaim");
    }

    /// A speed the node held across the next publish is freed by the next
    /// `reclaim`, with no further publish.
    #[test]
    fn reclaim_frees_a_speed_a_block_held_across_the_next() {
        let wave = Arc::new(tutti_buffers::Wave::from_mono(
            SampleRate::SR_48K,
            Mono::new(&[0.0; 64]),
        ));
        let voice = Voice {
            source: VoiceSource::Memory(MemorySource::new(wave).expect("a placeable wave")),
            play: Playback::default(),
        };
        let (mut node, handle) = VoiceNode::new(voice)
            .expect("a finite gain and a nonempty width")
            .with_handle();
        {
            // A block in progress across the move.
            let _held = node.speed.lend();
            handle.speed_cell().publish(PlaybackRate::of(2.0));
        }
        assert_eq!(handle.speed.retired_len(), 1, "pinned until a scan");
        assert_eq!(tutti_rt::Reclaim::reclaim(&handle), 0);
        assert_eq!(handle.speed.retired_len(), 0, "freed by the reclaim");
    }

    /// **A node's ring-out follows what can still steer it.** A placed voice
    /// is the programme and has none, whoever steers it; a free-running voice
    /// that a handle can loop, slow or stretch promises no ceiling; and its
    /// fork, which nothing steers, declares its wave's length at the rate it
    /// reads, at the stretch it plays.
    #[test]
    fn a_nodes_ring_out_is_the_ceiling_its_steering_allows() {
        let wave = Arc::new(tutti_buffers::Wave::from_mono(
            SampleRate::SR_48K,
            Mono::new(&[0.0; 4_800]),
        ));
        let prepare = Prepare::new(
            SampleRate::SR_48K,
            tutti_buffers::MaxBlock::of(tutti_units::Samples::new(64)),
        );
        let free = |play: Playback| {
            let voice = Voice {
                source: VoiceSource::Memory(
                    MemorySource::new(Arc::clone(&wave)).expect("a placeable wave"),
                ),
                play,
            };
            let NodeParts {
                node: mut live,
                fork,
                ..
            } = VoiceNode::new(voice)
                .expect("a finite gain and a nonempty width")
                .into_parts();
            live.prepare(&prepare);
            let mut fork = fork
                .expect("forks")
                .fork(RenderMode::Offline)
                .expect("forks")
                .node;
            fork.prepare(&prepare);
            (live.shape().tail, fork.shape().tail)
        };
        let (live, fork) = free(Playback::default());
        assert_eq!(live, Tail::Unbounded);
        assert_eq!(fork, Tail::Finite(tutti_units::Samples::new(4_800)));

        let stretched = Playback {
            stretch: tutti_units::StretchFactor::new(2.0),
            ..Playback::default()
        };
        assert_eq!(
            free(stretched).1,
            Tail::Finite(tutti_units::Samples::new(9_600)),
            "a stretch of two plays twice as long"
        );

        let placed = Voice {
            source: VoiceSource::Memory(
                MemorySource::placed(Arc::clone(&wave), VoiceWindow::from(Beat::new(0.0)))
                    .expect("a placeable wave"),
            ),
            play: Playback::default(),
        };
        assert_eq!(
            VoiceNode::new(placed)
                .expect("a finite gain and a nonempty width")
                .shape()
                .tail,
            Tail::None
        );
    }

    /// A standalone disk voice declares no latency, stretching or not: it reads its history
    /// from the ring before its seat, so it sounds on time.
    #[test]
    fn a_standalone_disk_voice_declares_no_latency() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("tone.wav");
        tutti_fixtures::signal::write_wav_f32(&path, SampleRate::SR_48K, 1, &vec![0.25f32; 4_800]);
        let mut streamer =
            crate::DiskStreamer::<_, f32>::manual(SampleRate::SR_48K, Default::default())
                .expect("streamer builds");
        let mut latency = |play: Playback| {
            let opening = streamer.open_stream(&path, crate::StreamStart::HEAD);
            let (stream, reader) =
                crate::testing::wait_for_open(&mut streamer, opening).expect("opens");
            let voice = crate::DiskVoice::from_reader(reader, crate::VoiceWindow::default());
            VoiceNode::streaming(voice, stream, play)
                .expect("a finite gain and a nonempty width")
                .shape()
                .latency
        };
        for play in [
            Playback {
                stretch: tutti_units::StretchFactor::new(1.5),
                ..Playback::default()
            },
            Playback::default(),
        ] {
            assert_eq!(latency(play).samples(), tutti_units::Samples::new(0));
        }
    }

    /// **A streaming node's handle owns its stream.** With every other holder
    /// gone (the caller moved the stream into the node; nothing keeps it
    /// apart), the stream stays served for as long as the handle lives, and
    /// ends with the last clone of it.
    #[test]
    fn a_streaming_nodes_handle_owns_its_stream() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("tone.wav");
        let samples = vec![0.25f32; 4_800];
        tutti_fixtures::signal::write_wav_f32(&path, SampleRate::SR_48K, 1, &samples);
        let mut streamer =
            crate::DiskStreamer::<_, f32>::manual(SampleRate::SR_48K, Default::default())
                .expect("streamer builds");
        let opening = streamer.open_stream(&path, crate::StreamStart::HEAD);
        let (stream, reader) =
            crate::testing::wait_for_open(&mut streamer, opening).expect("opens");
        let served = Arc::clone(&reader.status);
        let voice = crate::DiskVoice::from_reader(reader, crate::VoiceWindow::default());

        let (node, handle) = VoiceNode::streaming(voice, stream, Playback::default())
            .expect("a finite gain and a nonempty width")
            .with_handle();
        let second = handle.clone();
        let _ = streamer.step_until_idle(1_000);
        assert!(!served.closed(), "the handle holds the stream");

        drop(handle);
        let _ = streamer.step_until_idle(1_000);
        assert!(!served.closed(), "a clone of the handle still holds it");
        assert!(
            second.status().is_some(),
            "a streaming handle reports its stream"
        );

        drop(second);
        let _ = streamer.step_until_idle(1_000);
        assert!(served.closed(), "the last clone's drop ended the stream");
        drop(node);
    }
}
