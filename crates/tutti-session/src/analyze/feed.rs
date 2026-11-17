//! What binds a meter to audio: the context it is built with and the view each
//! chunk is pushed as, both read from the chunk the job reads.

use tutti_buffers::{
    Fold, FoldError, Interleaved, InterleavedMut, Mono, Planes, PlanesMut, StereoPlanes,
    deinterleave, interleave,
};
use tutti_types::{ChannelLayout, ChannelTopology, SampleRate, Samples};

use super::AnalyzeError;

/// How a chunk holds its frames.
#[derive(Debug)]
enum Layout<'w> {
    /// One plane per channel, borrowed from a wave in memory: no copy.
    Planar(Vec<&'w [f32]>),
    /// Interleaved frames the job decoded into its own buffer.
    Interleaved,
}

/// One chunk of audio at the audio's own width, as the source holds it (planes
/// of a wave in memory, interleaved frames of a decoded file), with the
/// scratch a view in the other shape is made in. Opaque: [`AudioInput`] names
/// it and only this crate makes one.
#[derive(Debug)]
pub struct Chunk<'w> {
    held: Layout<'w>,
    layout: ChannelLayout,
    to_mono: Result<Fold, FoldError>,
    /// The decoded frames of a file chunk, interleaved.
    data: Vec<f32>,
    /// Interleaved frames made from planes.
    interleaved: Vec<f32>,
    /// Planes made from interleaved frames.
    planes: Vec<Vec<f32>>,
    mono: Vec<f32>,
}

impl<'w> Chunk<'w> {
    /// An empty chunk of audio placed as `topology` says.
    pub(crate) fn new(topology: &ChannelTopology) -> Self {
        Self {
            held: Layout::Interleaved,
            layout: topology.width(),
            to_mono: Fold::between(topology, &ChannelTopology::assumed(ChannelLayout::MONO)),
            data: Vec::new(),
            interleaved: Vec::new(),
            planes: Vec::new(),
            mono: Vec::new(),
        }
    }

    /// Holds `planes`, one per channel, as the chunk.
    pub(crate) fn hold_planes(&mut self, planes: impl IntoIterator<Item = &'w [f32]>) {
        let mut held = match std::mem::replace(&mut self.held, Layout::Interleaved) {
            Layout::Planar(mut held) => {
                held.clear();
                held
            }
            Layout::Interleaved => Vec::new(),
        };
        held.extend(planes);
        self.held = Layout::Planar(held);
    }

    /// The interleaved buffer to decode `frames` frames into.
    #[cfg(codec)]
    pub(crate) fn fill(&mut self, frames: Samples) -> &mut [f32] {
        self.held = Layout::Interleaved;
        self.data.clear();
        self.data.resize(frames.interleaved_len(self.layout), 0.0);
        &mut self.data
    }

    /// Keeps the first `frames` frames of what was decoded.
    #[cfg(codec)]
    pub(crate) fn truncate(&mut self, frames: Samples) {
        self.data.truncate(frames.interleaved_len(self.layout));
    }

    /// Whether the chunk holds no frame.
    pub(crate) fn is_empty(&self) -> bool {
        self.frames() == Samples::ZERO
    }

    fn frames(&self) -> Samples {
        match &self.held {
            Layout::Planar(planes) => planes.first().map_or(Samples::ZERO, |p| Samples(p.len())),
            Layout::Interleaved => Samples::from_interleaved_len(self.data.len(), self.layout),
        }
    }
}

/// Builds a [`Measure::Context`](tutti_measure::Measure::Context) from the
/// audio an analysis runs over: the caller never states the rate or the width
/// of what it analyzes.
///
/// Implemented for the contexts the analyses take: a [`SampleRate`], a
/// [`ChannelLayout`], both as a pair, and `()`.
///
/// Sealed: the contexts below are the whole set.
pub trait AudioContext: sealed_context::Sealed {}

pub(crate) use sealed_context::Sealed as Bind;

mod sealed_context {
    use tutti_types::{ChannelLayout, SampleRate};

    /// Builds the context from the audio; unnameable outside the crate.
    pub trait Sealed {
        fn from_audio(rate: SampleRate, layout: ChannelLayout) -> Self;
    }
}

impl AudioContext for SampleRate {}

impl sealed_context::Sealed for SampleRate {
    fn from_audio(rate: SampleRate, _: ChannelLayout) -> Self {
        rate
    }
}

impl AudioContext for ChannelLayout {}

impl sealed_context::Sealed for ChannelLayout {
    fn from_audio(_: SampleRate, layout: ChannelLayout) -> Self {
        layout
    }
}

impl AudioContext for (SampleRate, ChannelLayout) {}

impl sealed_context::Sealed for (SampleRate, ChannelLayout) {
    fn from_audio(rate: SampleRate, layout: ChannelLayout) -> Self {
        (rate, layout)
    }
}

impl AudioContext for () {}

impl sealed_context::Sealed for () {
    fn from_audio(_: SampleRate, _: ChannelLayout) -> Self {}
}

/// Makes the view a meter takes ([`Meter::Input`](tutti_measure::Meter::Input))
/// from one chunk.
///
/// [`Interleaved`] is the chunk interleaved. [`Mono`] is the audio itself when
/// it is mono, otherwise the chunk folded to one channel by the audio's own
/// topology. [`StereoPlanes`] is a stereo chunk's two planes; audio of another
/// width is refused with [`AnalyzeError::Width`]. A wave in memory is read
/// through its own planes, and a file's frames are interleaved by nature: each
/// is converted only to the shape a meter asks for.
///
/// Sealed: the three views above are the whole set.
pub trait AudioInput<'a>: sealed::Sealed<'a> {}

pub(crate) use sealed::Sealed as View;

mod sealed {
    use super::{AnalyzeError, Chunk};

    /// Makes the view from a chunk; unnameable outside the crate.
    pub trait Sealed<'a>: Sized {
        fn view<'w: 'a>(chunk: &'a mut Chunk<'w>) -> Result<Self, AnalyzeError>;
    }
}

impl<'a> AudioInput<'a> for Interleaved<'a> {}

impl<'a> sealed::Sealed<'a> for Interleaved<'a> {
    fn view<'w: 'a>(chunk: &'a mut Chunk<'w>) -> Result<Self, AnalyzeError> {
        let layout = chunk.layout;
        match &chunk.held {
            Layout::Interleaved => Ok(Interleaved::new(&chunk.data, layout)?),
            Layout::Planar(planes) => {
                let frames = chunk.frames();
                chunk.interleaved.clear();
                chunk
                    .interleaved
                    .resize(frames.interleaved_len(layout), 0.0);
                interleave(
                    Planes::new(planes)?,
                    InterleavedMut::new(&mut chunk.interleaved, layout)?,
                )?;
                Ok(Interleaved::new(&chunk.interleaved, layout)?)
            }
        }
    }
}

impl<'a> AudioInput<'a> for Mono<'a> {}

impl<'a> sealed::Sealed<'a> for Mono<'a> {
    fn view<'w: 'a>(chunk: &'a mut Chunk<'w>) -> Result<Self, AnalyzeError> {
        let layout = chunk.layout;
        if layout == ChannelLayout::MONO {
            return Ok(match &chunk.held {
                Layout::Interleaved => Mono::new(&chunk.data),
                Layout::Planar(planes) => Mono::new(planes[0]),
            });
        }
        let fold = chunk.to_mono.as_ref().map_err(|error| *error)?;
        chunk.mono.clear();
        chunk.mono.resize(chunk.frames().get(), 0.0);
        let mono = InterleavedMut::new(&mut chunk.mono, ChannelLayout::MONO)?;
        match &chunk.held {
            Layout::Interleaved => fold.fold(Interleaved::new(&chunk.data, layout)?, mono)?,
            Layout::Planar(planes) => {
                fold.fold_planar_to_interleaved(Planes::new(planes)?, mono)?
            }
        }
        Ok(Mono::new(&chunk.mono))
    }
}

impl<'a> AudioInput<'a> for StereoPlanes<'a> {}

impl<'a> sealed::Sealed<'a> for StereoPlanes<'a> {
    fn view<'w: 'a>(chunk: &'a mut Chunk<'w>) -> Result<Self, AnalyzeError> {
        let layout = chunk.layout;
        if layout != ChannelLayout::STEREO {
            return Err(AnalyzeError::Width {
                needs: ChannelLayout::STEREO,
                found: layout,
            });
        }
        match &chunk.held {
            Layout::Planar(planes) => Ok(StereoPlanes::new(planes[0], planes[1])?),
            Layout::Interleaved => {
                let frames = chunk.frames();
                chunk.planes.resize_with(2, Vec::new);
                for plane in &mut chunk.planes {
                    plane.clear();
                    plane.resize(frames.get(), 0.0);
                }
                deinterleave(
                    Interleaved::new(&chunk.data, layout)?,
                    PlanesMut::from_vecs(&mut chunk.planes)?,
                )?;
                Ok(StereoPlanes::new(&chunk.planes[0], &chunk.planes[1])?)
            }
        }
    }
}
