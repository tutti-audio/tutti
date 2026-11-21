//! [`Clip`]: one sample placed on the timeline, built in a line and added to a
//! graph as a node, resident in memory or streamed from disk.

use std::sync::Arc;

use crate::SamplerFloat;
use tutti_buffers::ChannelLayout;
use tutti_buffers::Wave;
use tutti_graph::{HasParam, IntoNode, NodeParts};
use tutti_timeline::TempoReader;
use tutti_units::{Amplitude, Beat, PlaybackRate};

use crate::streamer::{StreamHandle, StreamReader};
use crate::{
    DiskVoice, GainError, Loop, MemorySource, PlaceError, Playback, Voice, VoiceNode,
    VoiceNodeHandle, VoiceSource, VoiceWindow, voice_width,
};

/// A sample placed on the timeline: where it starts, how loud, and whether it
/// loops. It is a graph node as it stands, so a user never assembles a
/// [`Voice`], a [`VoiceSource`] or a
/// [`VoiceWindow`]:
///
/// ```ignore
/// let kick = session.graph().add(
///     Clip::new(wave)?.at(Beat::new(4.0)).gain(Amplitude::new(0.8))?,
/// );
/// ```
///
/// It plays on the transport, from `at` (beat 0 until set) to the end of the
/// sample. A node over a clip reads the session's tempo map when the session's graph adds it
/// (`tutti-session`'s `SessionNode`).
///
/// # Two tiers, one clip
///
/// A clip is **resident** ([`new`](Self::new): the decoded wave, in memory)
/// or **streamed** ([`streaming`](Self::streaming): a stream the disk
/// streamer reads ahead, for a file too long to hold). Every builder method
/// reads the same on both, and both play what the file holds at the same
/// frames. The sampler never picks the tier: the caller does, by the
/// constructor it calls (the session's `load_audio` handle says which a file
/// was loaded as: a wave, or a header to stream).
///
/// A resident clip's width is the wave's own (placed by the wave's
/// [`topology`](Wave::topology), as every [`MemorySource`] places it; at most
/// the sampler's widest), until [`channels`](Self::channels) places it at
/// another. A streamed clip's is its stream's, which its open chose.
///
/// A clip is a [`VoiceNode`]'s: its controls are the node's
/// [`VoiceNodeHandle`], which also owns a streamed clip's stream, so the clip
/// sounds for as long as the node is in its graph.
///
/// `Clip` is a thin builder over [`VoiceNode`], which stays the doorway for
/// what a clip does not say (a pool of voices: [`VoicePool`](crate::VoicePool)).
#[derive(Debug)]
#[must_use = "a clip is a node: add it to a graph"]
pub struct Clip<F: SamplerFloat> {
    tier: Tier<F>,
    window: VoiceWindow,
    gain: Amplitude,
    speed: PlaybackRate,
    loop_: Option<Loop>,
    tempo: Option<TempoReader>,
}

/// What a clip plays.
#[derive(Debug)]
enum Tier<F: SamplerFloat> {
    /// The decoded wave, placed.
    Resident {
        wave: Arc<Wave<F>>,
        source: Box<MemorySource<F>>,
    },
    /// A stream and the reader the streamer feeds.
    Streamed {
        reader: Box<StreamReader<F>>,
        stream: StreamHandle<F>,
    },
}

impl<F: SamplerFloat> Clip<F> {
    fn over(tier: Tier<F>) -> Self {
        Self {
            tier,
            window: VoiceWindow::default(),
            gain: Amplitude::new(1.0),
            speed: PlaybackRate::UNITY,
            loop_: None,
            tempo: None,
        }
    }

    /// A resident clip of `wave`, at beat 0, at unit gain, playing once.
    ///
    /// The wave is held at the clip's own float width. Another width is the
    /// caller's to state, with `Wave::<f64>::from(&wave)` to widen or
    /// `wave.map(|x| x as f32)` to narrow:
    ///
    /// ```compile_fail
    /// use std::sync::Arc;
    /// use tutti_buffers::Wave;
    /// use tutti_sampler::Clip;
    /// use tutti_units::SampleRate;
    ///
    /// let wide = Arc::new(Wave::<f64>::new(1, SampleRate::SR_48K));
    /// let _clip = Clip::<f32>::new(wide);
    /// ```
    ///
    /// # Errors
    ///
    /// [`PlaceError`] for a wave with a channel no speaker names (a second
    /// LFE, a wide or high speaker): refused here, never played with a
    /// channel in the wrong place.
    pub fn new(wave: impl Into<Arc<Wave<F>>>) -> Result<Self, PlaceError> {
        let wave = wave.into();
        let width = voice_width(ChannelLayout::clamped(wave.channels()))?;
        Ok(Self::over(Tier::Resident {
            source: Box::new(MemorySource::with_channels(Arc::clone(&wave), width)?),
            wave,
        }))
    }

    /// A streamed clip over `stream` and the `reader` it opened with (a
    /// [`DiskStreamer::adopt`](crate::DiskStreamer::adopt) or
    /// `Session::open_stream`), at beat 0, at unit gain, playing once. The
    /// clip owns the stream: it ends when the clip's node leaves the graph.
    pub fn streaming(reader: StreamReader<F>, stream: StreamHandle<F>) -> Self {
        Self::over(Tier::Streamed {
            reader: Box::new(reader),
            stream,
        })
    }

    /// Whether the clip streams from disk (`false`: it is resident).
    #[must_use]
    pub fn is_streamed(&self) -> bool {
        matches!(self.tier, Tier::Streamed { .. })
    }

    /// Places a resident clip at `channels` wide, through the wave's topology
    /// (a mono wave into stereo plays on the centre of both, a stereo wave
    /// into mono folds). Gain, speed and loop set so far are kept.
    ///
    /// # Errors
    ///
    /// [`PlaceError`] when the wave has no place in that width (a channel no
    /// speaker names), or [`PlaceError::Streamed`] for a streamed clip, whose
    /// width is its stream's (`DiskStreamer::open_stream_into` chooses it).
    pub fn channels(mut self, channels: impl Into<ChannelLayout>) -> Result<Self, PlaceError> {
        let Tier::Resident { wave, source } = &mut self.tier else {
            return Err(PlaceError::Streamed);
        };
        **source = MemorySource::with_channels(Arc::clone(wave), channels)?;
        Ok(self)
    }

    /// Starts the clip at `start` on the timeline. A clip's node is placed
    /// from the start; its window can be published away
    /// ([`VoiceNodeHandle::window_cell`]), which frees it to run on its own
    /// cursor, forwards or backwards as built, unstretched.
    pub fn at(mut self, start: Beat) -> Self {
        self.window.start = start;
        self
    }

    /// The clip's level, linear (unity is `Amplitude::new(1.0)`).
    ///
    /// # Errors
    ///
    /// [`GainError::NotFinite`] for a NaN or infinite gain.
    pub fn gain(mut self, gain: Amplitude) -> Result<Self, GainError> {
        GainError::check(gain)?;
        self.gain = gain;
        Ok(self)
    }

    /// The clip's varispeed (pitch follows the rate).
    pub fn speed(mut self, speed: PlaybackRate) -> Self {
        self.speed = speed;
        self
    }

    /// Loops the clip over `looped`; a clip plays once without it.
    pub fn looped(mut self, looped: Loop) -> Self {
        self.loop_ = Some(looped);
        self
    }

    /// This clip reading the project's tempo map through `tempo`, as
    /// [`VoiceNode::tempo`].
    pub fn tempo(mut self, tempo: TempoReader) -> Self {
        self.tempo = Some(tempo);
        self
    }

    /// The node this clip is, placed and set.
    fn into_node(self) -> VoiceNode<F> {
        let Self {
            tier,
            window,
            gain,
            speed,
            loop_,
            tempo,
        } = self;
        let play = Playback {
            gain,
            speed,
            loop_,
            ..Playback::default()
        };
        let node = match tier {
            Tier::Resident { mut source, .. } => {
                source.set_window(window);
                source.store_gain(gain);
                source.set_speed(speed);
                source.set_loop(loop_);
                let width = source.channels();
                VoiceNode::with_channels(
                    Voice {
                        source: VoiceSource::Memory(*source),
                        play,
                    },
                    width,
                )
            }
            Tier::Streamed { reader, stream } => {
                VoiceNode::streaming(DiskVoice::from_reader(*reader, window), stream, play)
            }
        }
        // The clip's gain was checked when set, and its width when it was made.
        .expect("a clip's gain and width are checked at the call that sets them");
        match tempo {
            Some(tempo) => node.tempo(tempo),
            None => node,
        }
    }
}

/// A clip has the params of the node it builds.
impl<F: SamplerFloat, const P: u16> HasParam<P> for Clip<F>
where
    VoiceNode<F>: HasParam<P>,
{
    type Unit = <VoiceNode<F> as HasParam<P>>::Unit;
}

impl<F: SamplerFloat> IntoNode<F> for Clip<F> {
    type Controls = VoiceNodeHandle<F>;

    fn into_parts(self) -> NodeParts<VoiceNodeHandle<F>, F> {
        self.into_node().into_parts()
    }
}
