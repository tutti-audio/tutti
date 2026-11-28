//! [`VoiceNodeConfig`]: what a [`VoiceNode`] is built from.

use std::marker::PhantomData;
use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_graph::{ForkCause, HostValues, Initials, NodeConfig, RenderMode, Shared};
use tutti_rt::{AtomicCell, Mutex};
use tutti_units::{Amplitude, Cents, PlaybackRate, StretchFactor, UnitAddr, UnitParam};

use super::command::GainError;
use super::command::ask;
use super::disk_voice::DiskVoice;
use super::home::{Home, Seat};
use super::loop_span::Loop;
use super::memory_source::MemorySource;
use super::memory_source::{MemorySourceConfig, VoiceWindow};
use super::node::{
    DirectionKind, LoopingKind, SpeedKind, SteeringKind, VoiceNode, VoiceNodeError, WindowKind,
    apply_loop, apply_speed,
};
use super::node_controls::{Steering, StreamHold};
use super::slot::stretch_wanted;
use super::types::{PlayDirection, Playback, Voice, VoiceSource};
use crate::SamplerFloat;
use crate::streamer::StreamHandle;

mod sealed {
    pub trait VoiceDrive {}
}

/// How a [`VoiceNodeConfig`] drives its voice, fixed by the type: [`Free`],
/// [`Placed`] or [`Streamed`].
pub trait VoiceDrive: sealed::VoiceDrive + Send + Sync + 'static {}

/// A voice on its own cursor, played and stopped through its steering.
#[derive(Clone, Copy, Debug)]
pub struct Free;
/// A resident voice on the transport, at a window.
#[derive(Clone, Copy, Debug)]
pub struct Placed;
/// A disk voice, on the transport at the window it was built at.
#[derive(Clone, Copy, Debug)]
pub struct Streamed;

impl sealed::VoiceDrive for Free {}
impl sealed::VoiceDrive for Placed {}
impl sealed::VoiceDrive for Streamed {}
impl VoiceDrive for Free {}
impl VoiceDrive for Placed {}
impl VoiceDrive for Streamed {}

/// What the voice reads.
#[derive(Clone)]
pub(crate) enum Form<F: SamplerFloat> {
    /// A resident wave.
    Memory(MemorySourceConfig<F>),
    /// A stream, read through its voice. The voice has one ring reader, which
    /// the config's [`Home`] keeps: a node takes it at build, or waits for it, and hands it back
    /// when it drops. A stream is control-thread only (its drop sends on a
    /// bounded queue), so a node hands it to its controls.
    Streamed {
        home: Shared<Mutex<Home<F>>>,
        stream: Shared<Mutex<StreamHandle<F>>>,
    },
}

/// What a [`VoiceNode`] is built from: a voice. Its gain, speed and loop are its
/// source config's (a streamed voice's gain is a constructor argument), and
/// each lives in the config's own fields from then on. `VoiceNode::from` builds
/// it, and cannot fail: the constructor has checked the gain and the width.
///
/// The type says how the voice is driven, for the node's life.
/// [`free`](Self::free) is a [`Free`] voice on its own cursor, with a
/// [`stopped`](Self::stopped) option and no window; [`placed`](Self::placed) is
/// a [`Placed`] voice on the transport at its source's window, with a
/// [`window`](Self::window) option and no steering; [`streaming`](Self::streaming)
/// is a [`Streamed`] voice, placed at its voice's window. An option another
/// drive owns is not written.
///
/// The gain is the node's param ([`UnitParam::Volume`]), and the steering,
/// window, loop, direction and speed are its published values: a change of
/// any is a write to the running node. The wave (by identity), the placement,
/// the stream (by identity), the width, the stretch and the pitch are structure:
/// a change builds a new node.
///
/// ```
/// use std::sync::Arc;
/// use tutti_buffers::ChannelLayout;
/// use tutti_sampler::{MemorySourceConfig, VoiceNode, VoiceNodeConfig, Wave};
/// use tutti_units::{PlaybackRate, SampleRate};
///
/// let wave = Arc::new(Wave::<f32>::new(2, SampleRate::SR_48K));
/// let source = MemorySourceConfig::new(wave, ChannelLayout::STEREO).expect("a stereo wave places");
/// let config = VoiceNodeConfig::free(source)
///     .expect("a finite gain, and a source on its own cursor")
///     .speed(PlaybackRate::of(2.0));
/// let node = VoiceNode::from(config);
/// ```
pub struct VoiceNodeConfig<F: SamplerFloat, D: VoiceDrive = Free> {
    form: Form<F>,
    channels: ChannelLayout,
    play: Playback,
    steering: Steering,
    drive: PhantomData<fn() -> D>,
}

impl<F: SamplerFloat, D: VoiceDrive> Clone for VoiceNodeConfig<F, D> {
    fn clone(&self) -> Self {
        Self {
            form: self.form.clone(),
            channels: self.channels,
            play: self.play,
            steering: self.steering,
            drive: PhantomData,
        }
    }
}

impl<F: SamplerFloat, D: VoiceDrive> VoiceNodeConfig<F, D> {
    fn of(form: Form<F>, channels: ChannelLayout, play: Playback) -> Self {
        Self {
            form,
            channels,
            play,
            steering: Steering::default(),
            drive: PhantomData,
        }
    }

    /// This config with its stretch and pitch at their defaults, and the
    /// stretch and pitch it asked for when they ask for a vocoder: what a
    /// pool's row keeps apart.
    pub(crate) fn split_stretch(mut self) -> (Self, Option<(StretchFactor, Cents)>) {
        let stretch = stretch_wanted(&self.play).then_some((self.play.stretch, self.play.pitch));
        self.play.stretch = StretchFactor::UNITY;
        self.play.pitch = Cents::new(0.0);
        (self, stretch)
    }

    /// This config with `factor` and `pitch` as its stretch and pitch: a pool's row
    /// keeps them apart from the config's.
    pub(crate) fn stretched_by(mut self, factor: StretchFactor, pitch: Cents) -> Self {
        self.play.stretch = factor;
        self.play.pitch = pitch;
        self
    }

    /// What the config asks of a voice that is a value: its gain, speed, loop,
    /// direction, stretch and pitch; the window of a placed resident voice (a
    /// streamed voice's is its stream's); and the play state of a free-running one.
    pub(crate) fn asks(&self) -> (Playback, Option<VoiceWindow>, Steering) {
        let window = match &self.form {
            Form::Memory(source) => source.window_value(),
            Form::Streamed { .. } => None,
        };
        (self.play, window, self.steering)
    }

    /// A streamed config's stream, whose cells the controls sweep.
    pub(crate) fn stream(&self) -> Option<Arc<Mutex<StreamHandle<F>>>> {
        match &self.form {
            Form::Streamed { stream, .. } => Some(Arc::clone(stream.arc())),
            Form::Memory(_) => None,
        }
    }

    /// Writes a streamed config's loop and direction to its stream's cells; nothing
    /// for a resident one.
    pub(crate) fn steer_stream(&self) {
        if let Form::Streamed { stream, .. } = &self.form {
            steer_stream(&stream.lock(), self.play.loop_, self.play.direction);
        }
    }

    /// This config at varispeed `speed`.
    #[must_use]
    pub fn speed(mut self, speed: PlaybackRate) -> Self {
        self.play.speed = speed;
        self
    }

    /// This config looping over `looped`.
    #[must_use]
    pub fn looped(mut self, looped: Loop) -> Self {
        self.play.loop_ = Some(looped);
        self
    }

    /// This config reading in `direction`.
    #[must_use]
    pub fn direction(mut self, direction: PlayDirection) -> Self {
        self.play.direction = direction;
        self
    }
}

/// The stretch and pitch options of a memory voice: a streamed voice's are
/// arguments of its constructor, because they are part of its structure.
macro_rules! stretch_options {
    ($drive:ty) => {
        impl<F: SamplerFloat> VoiceNodeConfig<F, $drive> {
            /// This config time-stretched by `stretch`.
            #[must_use]
            pub fn stretch(mut self, stretch: StretchFactor) -> Self {
                self.play.stretch = stretch;
                self
            }

            /// This config pitch-shifted by `pitch`.
            #[must_use]
            pub fn pitch(mut self, pitch: Cents) -> Self {
                self.play.pitch = pitch;
                self
            }
        }
    };
}
stretch_options!(Free);
stretch_options!(Placed);

/// The voice's own values, taken from `source`: its gain (checked), speed and loop.
fn memory_voice<F: SamplerFloat>(
    source: MemorySourceConfig<F>,
) -> Result<(Form<F>, ChannelLayout, Playback), VoiceNodeError> {
    let (gain, speed, loop_) = source.values();
    GainError::check(gain)?;
    let channels = source.channels;
    let play = Playback {
        gain,
        speed,
        loop_,
        ..Playback::default()
    };
    Ok((Form::Memory(source.without_values()), channels, play))
}

impl<F: SamplerFloat> VoiceNodeConfig<F, Free> {
    /// A config over `source`, free-running. The voice's gain, speed and loop are
    /// the source config's.
    ///
    /// # Errors
    ///
    /// [`VoiceNodeError::Gain`] for a source gain that is not finite, and
    /// [`VoiceNodeError::Placed`] for a source on the transport.
    pub fn free(source: MemorySourceConfig<F>) -> Result<Self, VoiceNodeError> {
        if source.window_value().is_some() {
            return Err(VoiceNodeError::Placed);
        }
        let (form, channels, play) = memory_voice(source)?;
        Ok(Self::of(form, channels, play))
    }

    /// This config starting stopped: the voice is silent until it is played.
    #[must_use]
    pub fn stopped(mut self) -> Self {
        self.steering.stopped = true;
        self
    }
}

impl<F: SamplerFloat> VoiceNodeConfig<F, Placed> {
    /// A config over `source`, placed on the transport at its window. The voice's
    /// gain, speed and loop are the source config's.
    ///
    /// # Errors
    ///
    /// [`VoiceNodeError::Gain`] for a source gain that is not finite, and
    /// [`VoiceNodeError::Free`] for a source with no window.
    pub fn placed(source: MemorySourceConfig<F>) -> Result<Self, VoiceNodeError> {
        if source.window_value().is_none() {
            return Err(VoiceNodeError::Free);
        }
        let (form, channels, play) = memory_voice(source)?;
        Ok(Self::of(form, channels, play))
    }

    /// This config at `window` of the timeline.
    #[must_use]
    pub fn window(mut self, window: VoiceWindow) -> Self {
        if let Form::Memory(source) = self.form {
            self.form = Form::Memory(source.window(window));
        }
        self
    }
}

impl<F: SamplerFloat> VoiceNodeConfig<F, Streamed> {
    /// A config over a streaming `voice` and the `stream` that feeds it, at
    /// `gain`: the node's controls keep the stream, so the voice sounds for as
    /// long as they live. The gain is an argument because a stream has no source
    /// config to carry one, and a constructor is where it is checked. The stream
    /// is the config's identity: a clone of the config, with its values moved, is
    /// the same structure, and a config over another stream is another node.
    /// `voice` is the one built from the stream's own reader. `stretch` and
    /// `pitch` are arguments rather than options: they are structure, so a
    /// config over one stream is always the same node.
    ///
    /// # Errors
    ///
    /// [`VoiceNodeError::Gain`] for a `gain` that is not finite, and
    /// [`VoiceNodeError::Width`] for a voice the sampler cannot run
    /// ([`voice_width`](crate::voice_width)).
    pub fn streaming(
        voice: DiskVoice<F>,
        stream: StreamHandle<F>,
        gain: Amplitude,
        stretch: StretchFactor,
        pitch: Cents,
    ) -> Result<Self, VoiceNodeError> {
        GainError::check(gain)?;
        let channels = crate::voice_width(ChannelLayout::from_count(voice.width() as u16))?;
        Ok(Self::of(
            Form::Streamed {
                home: Shared::new(Mutex::new(Home::of(voice))),
                stream: Shared::new(Mutex::new(stream)),
            },
            channels,
            Playback {
                gain,
                stretch,
                pitch,
                ..Playback::default()
            },
        ))
    }
}

// Hand-written: a memory source's config and a stream are not `Debug`. Prints the
// drive and the values.
impl<F: SamplerFloat, D: VoiceDrive> std::fmt::Debug for VoiceNodeConfig<F, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceNodeConfig")
            .field("drive", &std::any::type_name::<D>())
            .field("channels", &self.channels)
            .field("play", &self.play)
            .field("steering", &self.steering)
            .finish_non_exhaustive()
    }
}

impl<F: SamplerFloat, D: VoiceDrive> PartialEq for VoiceNodeConfig<F, D> {
    fn eq(&self, other: &Self) -> bool {
        let same_form = match (&self.form, &other.form) {
            (Form::Memory(a), Form::Memory(b)) => a == b,
            // The voice is the stream's own reader, so the stream is the identity.
            (Form::Streamed { stream: a, .. }, Form::Streamed { stream: b, .. }) => a == b,
            _ => false,
        };
        same_form
            && self.channels == other.channels
            && self.play == other.play
            && self.steering == other.steering
    }
}

impl<F: SamplerFloat, D: VoiceDrive> NodeConfig<F> for VoiceNodeConfig<F, D> {
    type Node = VoiceNode<F>;

    fn build(&self, _: &HostValues) -> VoiceNode<F> {
        VoiceNode::from(self.clone())
    }

    /// Compared field by field, without a copy: a pool asks it on the audio thread, where
    /// a copy of a folded source's matrix would allocate.
    fn same_structure(&self, other: &Self) -> bool {
        let same_form = match (&self.form, &other.form) {
            (Form::Memory(mine), Form::Memory(theirs)) => mine.same_structure(theirs),
            // The voice is the stream's own reader, so the stream is the identity.
            (Form::Streamed { stream: a, .. }, Form::Streamed { stream: b, .. }) => a == b,
            _ => false,
        };
        same_form
            && self.channels == other.channels
            && self.play.stretch == other.play.stretch
            && self.play.pitch == other.play.pitch
    }

    fn initials(&self) -> Initials {
        let window = match &self.form {
            Form::Memory(source) => source.window_value(),
            Form::Streamed { home, .. } => Some(home.lock().window),
        };
        let initials = Initials::new()
            .param(UnitAddr::first(UnitParam::Volume), self.play.gain.get())
            .published::<LoopingKind>(self.play.loop_)
            .published::<DirectionKind>(self.play.direction)
            .published::<SpeedKind>(self.play.speed);
        match window {
            Some(window) => initials.published::<WindowKind>(window),
            None => initials.published::<SteeringKind>(self.steering),
        }
    }
}

/// What a config builds: its voice at the config's values, and how the voice is
/// held.
pub(crate) struct Built<F: SamplerFloat> {
    pub(crate) voice: Voice<F>,
    pub(crate) channels: ChannelLayout,
    pub(crate) steering: Steering,
    /// The stream and the seat of a streamed config's voice; `None` for a resident one.
    pub(crate) held: Option<Held<F>>,
}

/// A streamed voice's hold on its stream's reader.
pub(crate) struct Held<F: SamplerFloat> {
    pub(crate) seat: Seat<F>,
    pub(crate) stream: Arc<Mutex<StreamHandle<F>>>,
    /// Whether the voice has the reader yet; the audio end writes it when it takes it.
    pub(crate) hold: Arc<AtomicCell<StreamHold>>,
}

/// Requests `looped` and `direction` of `stream`, which its streamer reads.
pub(crate) fn steer_stream<F: SamplerFloat>(
    stream: &StreamHandle<F>,
    looped: Option<Loop>,
    direction: PlayDirection,
) {
    ask(stream.looping_cell(), looped);
    ask(stream.direction_cell(), direction);
}

impl<F: SamplerFloat, D: VoiceDrive> VoiceNodeConfig<F, D> {
    /// The voice this config describes. The values start at the config's: a
    /// resident voice takes `play`'s gain, speed and loop over its source's, and a
    /// disk voice's loop and direction are first requested of its stream. A streamed
    /// config's reader is taken, or waited for ([`Home::seat`]). Control side.
    pub(crate) fn built(self) -> Built<F> {
        let Self {
            form,
            channels,
            play,
            steering,
            ..
        } = self;
        match form {
            Form::Memory(source) => Built {
                voice: memory_voice_at(source, play),
                channels,
                steering,
                held: None,
            },
            Form::Streamed { home, stream } => {
                let (mut disk, seat) = Home::seat(home.arc());
                steer_stream(&stream.lock(), play.loop_, play.direction);
                disk.store_gain(play.gain);
                disk.set_speed(play.speed);
                let hold = Arc::new(AtomicCell::new(if seat.holds() == Some(true) {
                    StreamHold::Holding
                } else {
                    StreamHold::Waiting
                }));
                Built {
                    voice: Voice {
                        source: VoiceSource::Disk(disk),
                        play,
                    },
                    channels,
                    steering,
                    held: Some(Held {
                        seat,
                        stream: Arc::clone(stream.arc()),
                        hold,
                    }),
                }
            }
        }
    }

    /// The voice a fork of a node over this config plays: a copy that shares nothing
    /// with the live one ([`Voice::fork_copy`]), at the config's values.
    pub(crate) fn forked(&self, mode: RenderMode) -> Result<Voice<F>, ForkCause> {
        match &self.form {
            Form::Memory(source) => memory_voice_at(source.clone(), self.play).fork_copy(mode),
            Form::Streamed { home, .. } => {
                let mut voice = Voice {
                    source: VoiceSource::Disk(home.lock().template()),
                    play: self.play,
                }
                .fork_copy(mode)?;
                // The fork's detached cells hold what the live cells hold now, which
                // the audio thread writes at its next block: the config's values
                // are the ones in force (the stretch rate is the slot's, per block).
                voice.source.apply_gain(self.play.gain);
                apply_speed(&mut voice, self.play.speed);
                Ok(voice)
            }
        }
    }
}

/// A resident voice over `source` at `play`'s gain, speed and loop.
fn memory_voice_at<F: SamplerFloat>(source: MemorySourceConfig<F>, play: Playback) -> Voice<F> {
    let mut voice = Voice {
        source: VoiceSource::Memory(MemorySource::from(source)),
        play,
    };
    voice.source.apply_gain(play.gain);
    apply_speed(&mut voice, play.speed);
    apply_loop(&mut voice, play.loop_);
    voice
}

/// The node a config describes: its voice, at the config's values. A streamed config's
/// stream moves into the node's controls.
impl<F: SamplerFloat, D: VoiceDrive> From<VoiceNodeConfig<F, D>> for VoiceNode<F> {
    fn from(config: VoiceNodeConfig<F, D>) -> Self {
        let Built {
            voice,
            channels,
            steering,
            held,
        } = config.built();
        let mut node = Self::of_voice(voice, channels, steering);
        if let Some(Held { seat, stream, hold }) = held {
            node.stream = Some(stream);
            node.hold = Some(hold);
            node.seat = seat;
        }
        node
    }
}
