//! What binds a meter to audio: the context it is built with and the view each
//! chunk is pushed as, both read from the chunk the job decoded.

use tutti_buffers::{Fold, FoldError, Interleaved, InterleavedMut, Mono, StereoPlanes};
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate};

use super::AnalyzeError;

/// One chunk of interleaved audio at the audio's own width, with the scratch
/// the views that are not interleaved are made in. Unnameable outside the
/// crate: the traits that take it are implemented here alone.
#[derive(Debug)]
pub struct Chunk {
    data: Vec<f32>,
    layout: ChannelLayout,
    to_mono: Result<Fold, FoldError>,
    mono: Vec<f32>,
    left: Vec<f32>,
    right: Vec<f32>,
}

impl Chunk {
    /// An empty chunk of audio placed as `topology` says.
    pub(crate) fn new(topology: &ChannelTopology) -> Self {
        Self {
            data: Vec::new(),
            layout: topology.width(),
            to_mono: Fold::between(topology, &ChannelTopology::assumed(ChannelLayout::MONO)),
            mono: Vec::new(),
            left: Vec::new(),
            right: Vec::new(),
        }
    }

    /// The interleaved buffer to fill with `frames` frames.
    pub(crate) fn fill(&mut self, frames: usize) -> &mut [f32] {
        self.data.clear();
        self.data
            .resize(frames * usize::from(self.layout.count()), 0.0);
        &mut self.data
    }

    /// Keeps the first `frames` frames of what was filled.
    pub(crate) fn truncate(&mut self, frames: usize) {
        self.data
            .truncate(frames * usize::from(self.layout.count()));
    }

    /// Whether the chunk holds no frame.
    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn frames(&self) -> usize {
        self.data.len() / usize::from(self.layout.count())
    }
}

/// Builds a [`Measure::Context`](tutti_measure::Measure::Context) from the
/// audio an analysis runs over: the caller never states the rate or the width
/// of what it analyzes.
///
/// Implemented for the contexts the analyses take: a [`SampleRate`], a
/// [`ChannelLayout`], both as a pair, and `()`.
pub trait AudioContext {
    /// The context for audio at `rate` and `layout`.
    fn from_audio(rate: SampleRate, layout: ChannelLayout) -> Self;
}

impl AudioContext for SampleRate {
    fn from_audio(rate: SampleRate, _: ChannelLayout) -> Self {
        rate
    }
}

impl AudioContext for ChannelLayout {
    fn from_audio(_: SampleRate, layout: ChannelLayout) -> Self {
        layout
    }
}

impl AudioContext for (SampleRate, ChannelLayout) {
    fn from_audio(rate: SampleRate, layout: ChannelLayout) -> Self {
        (rate, layout)
    }
}

impl AudioContext for () {
    fn from_audio(_: SampleRate, _: ChannelLayout) -> Self {}
}

/// Makes the view a meter takes ([`Meter::Input`](tutti_measure::Meter::Input))
/// from one decoded chunk.
///
/// [`Interleaved`] is the chunk as it is. [`Mono`] is the chunk folded to one
/// channel by the audio's own topology (the audio itself when it is mono).
/// [`StereoPlanes`] is a stereo chunk's two planes; audio of another width is
/// refused with [`AnalyzeError::Width`].
pub trait AudioInput<'a>: Sized {
    /// The view of `chunk`.
    ///
    /// # Errors
    ///
    /// The audio cannot be viewed this way.
    fn view(chunk: &'a mut Chunk) -> Result<Self, AnalyzeError>;
}

impl<'a> AudioInput<'a> for Interleaved<'a> {
    fn view(chunk: &'a mut Chunk) -> Result<Self, AnalyzeError> {
        Ok(Interleaved::new(&chunk.data, chunk.layout)?)
    }
}

impl<'a> AudioInput<'a> for Mono<'a> {
    fn view(chunk: &'a mut Chunk) -> Result<Self, AnalyzeError> {
        if chunk.layout == ChannelLayout::MONO {
            return Ok(Mono::new(&chunk.data));
        }
        let fold = chunk.to_mono.as_ref().map_err(|error| *error)?;
        chunk.mono.clear();
        chunk.mono.resize(chunk.frames(), 0.0);
        fold.fold(
            Interleaved::new(&chunk.data, chunk.layout)?,
            InterleavedMut::new(&mut chunk.mono, ChannelLayout::MONO)?,
        )?;
        Ok(Mono::new(&chunk.mono))
    }
}

impl<'a> AudioInput<'a> for StereoPlanes<'a> {
    fn view(chunk: &'a mut Chunk) -> Result<Self, AnalyzeError> {
        if chunk.layout != ChannelLayout::STEREO {
            return Err(AnalyzeError::Width {
                needs: ChannelLayout::STEREO,
                found: chunk.layout,
            });
        }
        chunk.left.clear();
        chunk.right.clear();
        for &[left, right] in chunk.data.as_chunks::<2>().0 {
            chunk.left.push(left);
            chunk.right.push(right);
        }
        Ok(StereoPlanes::new(&chunk.left, &chunk.right)?)
    }
}
