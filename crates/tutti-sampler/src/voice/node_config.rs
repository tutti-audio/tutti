//! [`VoiceNodeConfig`]: what a [`VoiceNode`] is built from.

use std::marker::PhantomData;
use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_graph::{HostValues, Initials, NodeConfig, Shared};
use tutti_rt::{AtomicCell, Mutex};
use tutti_units::{Amplitude, Cents, PlaybackRate, StretchFactor, UnitAddr, UnitParam};

use super::command::GainError;
use super::command::ask;
use super::disk_voice::DiskVoice;
use super::home::Home;
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
    pub trait Drive {}
}

/// How a [`VoiceNodeConfig`] drives its voice, fixed by the type: [`Free`],
/// [`Placed`] or [`Streamed`].
pub trait Drive: sealed::Drive + Send + Sync + 'static {}

/// A voice on its own cursor, played and stopped through its steering.
#[derive(Clone, Copy, Debug)]
pub struct Free;
/// A resident voice on the transport, at a window.
#[derive(Clone, Copy, Debug)]
pub struct Placed;
/// A disk voice, on the transport at the window it was built at.
#[derive(Clone, Copy, Debug)]
pub struct Streamed;

impl sealed::Drive for Free {}
impl sealed::Drive for Placed {}
impl sealed::Drive for Streamed {}
impl Drive for Free {}
impl Drive for Placed {}
impl Drive for Streamed {}

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
pub struct VoiceNodeConfig<F: SamplerFloat, D: Drive = Free> {
    pub(crate) form: Form<F>,
    pub(crate) channels: ChannelLayout,
    pub(crate) play: Playback,
    pub(crate) steering: Steering,
    drive: PhantomData<fn() -> D>,
}

impl<F: SamplerFloat, D: Drive> Clone for VoiceNodeConfig<F, D> {
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

impl<F: SamplerFloat, D: Drive> VoiceNodeConfig<F, D> {
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
impl<F: SamplerFloat, D: Drive> std::fmt::Debug for VoiceNodeConfig<F, D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceNodeConfig")
            .field("drive", &std::any::type_name::<D>())
            .field("channels", &self.channels)
            .field("play", &self.play)
            .field("steering", &self.steering)
            .finish_non_exhaustive()
    }
}

impl<F: SamplerFloat, D: Drive> PartialEq for VoiceNodeConfig<F, D> {
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

impl<F: SamplerFloat, D: Drive> NodeConfig<F> for VoiceNodeConfig<F, D> {
    type Node = VoiceNode<F>;

    fn build(&self, _: &HostValues) -> VoiceNode<F> {
        VoiceNode::from(self.clone())
    }

    fn same_structure(&self, other: &Self) -> bool {
        let mut with_other_values = self.clone();
        with_other_values.play.gain = other.play.gain;
        with_other_values.play.speed = other.play.speed;
        with_other_values.play.loop_ = other.play.loop_;
        with_other_values.play.direction = other.play.direction;
        with_other_values.steering = other.steering;
        if let (Form::Memory(mine), Form::Memory(theirs)) = (&self.form, &other.form) {
            with_other_values.form = Form::Memory(mine.clone().with_window_of(theirs));
        }
        with_other_values == *other
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

/// The node a config describes. The values start at the config's: a resident
/// voice takes `play`'s gain, speed and loop over its source's, and a disk voice's
/// loop and direction are first requested of its stream. A streamed config's
/// stream moves into the node's controls.
impl<F: SamplerFloat, D: Drive> From<VoiceNodeConfig<F, D>> for VoiceNode<F> {
    fn from(config: VoiceNodeConfig<F, D>) -> Self {
        let VoiceNodeConfig {
            form,
            channels,
            play,
            steering,
            ..
        } = config;
        match form {
            Form::Memory(source) => {
                let mut voice = Voice {
                    source: VoiceSource::Memory(MemorySource::from(source)),
                    play,
                };
                voice.source.apply_gain(play.gain);
                apply_speed(&mut voice, play.speed);
                apply_loop(&mut voice, play.loop_);
                Self::of_voice(voice, channels, steering)
            }
            Form::Streamed { home, stream } => {
                let (disk, seat) = Home::seat(home.arc());
                {
                    let stream = stream.lock();
                    ask(stream.looping_cell(), play.loop_);
                    ask(stream.direction_cell(), play.direction);
                }
                let mut disk = disk;
                disk.store_gain(play.gain);
                disk.set_speed(play.speed);
                let mut node = Self::of_voice(
                    Voice {
                        source: VoiceSource::Disk(disk),
                        play,
                    },
                    channels,
                    steering,
                );
                node.stream = Some(Arc::clone(stream.arc()));
                node.hold = Some(Arc::new(AtomicCell::new(if seat.holds() == Some(true) {
                    StreamHold::Holding
                } else {
                    StreamHold::Waiting
                })));
                node.seat = seat;
                node
            }
        }
    }
}
