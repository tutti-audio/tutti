//! What binds a meter to audio: the context it is built with and the view each
//! chunk is pushed as, both read from the chunk the job reads.

use tutti_buffers::{
    BufferError, Float, Fold, FoldError, Interleaved, InterleavedMut, Mono, Planes, PlanesMut,
    StereoPlanes, deinterleave, interleave,
};
use tutti_buffers::{ChannelLayout, ChannelSet};
use tutti_units::{SampleRate, Samples};

/// Why a chunk cannot be shown to a meter in the view it takes: what binding
/// a stream to a meter, offline or live, can refuse.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MeterBindError {
    /// The analysis reads two channels and the audio is of another width.
    #[error("the analysis reads {needs:?} audio, and the audio is {found:?}")]
    Width {
        /// The width the analysis reads.
        needs: ChannelLayout,
        /// The audio's width.
        found: ChannelLayout,
    },
    /// The audio's channels cannot be folded to one for a mono analysis.
    #[error("the audio's channels cannot be folded to mono: {0}")]
    Fold(#[from] FoldError),
    /// A chunk was not a whole number of frames of the audio's width.
    #[error("a chunk of the audio is malformed: {0}")]
    Buffer(#[from] BufferError),
}

/// How a chunk holds its frames.
#[derive(Debug)]
enum Layout<'w, S> {
    /// One plane per channel, borrowed from a wave in memory: no copy.
    Planar(Vec<&'w [S]>),
    /// Interleaved frames the job decoded into its own buffer.
    Interleaved,
}

/// One chunk of audio at the audio's own width, as the source holds it (planes
/// of a wave in memory, interleaved frames of a decoded file), with the
/// scratch a view in the other shape is made in. Opaque: [`AudioInput`] names
/// it and only this crate makes one.
#[derive(Debug)]
pub struct Chunk<'w, S: Float> {
    held: Layout<'w, S>,
    layout: ChannelLayout,
    to_mono: Result<Fold<S>, FoldError>,
    /// The decoded frames of a file chunk, interleaved.
    data: Vec<S>,
    /// Interleaved frames made from planes.
    interleaved: Vec<S>,
    /// Planes made from interleaved frames.
    planes: Vec<Vec<S>>,
    mono: Vec<S>,
}

impl<'w, S: Float> Chunk<'w, S> {
    /// An empty chunk of audio placed as `channels` says; audio wider than a
    /// topology holds has none, and no fold takes it to mono.
    pub(crate) fn new(channels: &ChannelSet) -> Self {
        let layout = channels.width();
        Self {
            held: Layout::Interleaved,
            layout,
            to_mono: match channels.topology() {
                Some(topology) => Fold::between(topology, &tutti_buffers::Bed::Mono.topology()),
                None => Err(FoldError::Unassigned {
                    src: layout,
                    dst: ChannelLayout::MONO,
                }),
            },
            data: Vec::new(),
            interleaved: Vec::new(),
            planes: Vec::new(),
            mono: Vec::new(),
        }
    }

    /// Holds `planes`, one per channel, as the chunk.
    pub(crate) fn hold_planes(&mut self, planes: impl IntoIterator<Item = &'w [S]>) {
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

    /// The interleaved buffer to fill with `frames` frames (decoded, or popped
    /// from a tap).
    pub(crate) fn fill(
        &mut self,
        frames: Samples,
    ) -> Result<InterleavedMut<'_, S>, MeterBindError> {
        self.held = Layout::Interleaved;
        self.data.clear();
        self.data
            .resize(self.layout.interleaved_len(frames), S::ZERO);
        Ok(InterleavedMut::new(&mut self.data, self.layout)?)
    }

    /// Keeps the first `frames` frames of what was decoded.
    pub(crate) fn truncate(&mut self, frames: Samples) {
        self.data.truncate(self.layout.interleaved_len(frames));
    }

    /// Whether the chunk holds no frame.
    pub(crate) fn is_empty(&self) -> bool {
        self.frames() == Samples::ZERO
    }

    /// The frames the chunk holds.
    pub(crate) fn frames(&self) -> Samples {
        match &self.held {
            Layout::Planar(planes) => planes
                .first()
                .map_or(Samples::ZERO, |p| Samples::new(p.len())),
            Layout::Interleaved => self.layout.from_interleaved_len(self.data.len()),
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
    use tutti_buffers::ChannelLayout;
    use tutti_units::SampleRate;

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

/// Makes the view a meter takes ([`StreamMeter::Input`](tutti_measure::StreamMeter::Input))
/// from one chunk.
///
/// [`Interleaved`] is the chunk interleaved. [`Mono`] is the audio itself when
/// it is mono, otherwise the chunk folded to one channel by the audio's own
/// topology. [`StereoPlanes`] is a stereo chunk's two planes; audio of another
/// width is refused with [`MeterBindError::Width`]. A wave in memory is read
/// through its own planes, and a file's frames are interleaved by nature: each
/// is converted only to the shape a meter asks for.
///
/// Sealed: the three views above are the whole set.
pub trait AudioInput<'a, S: Float>: sealed::Sealed<'a, S> {}

pub(crate) use sealed::Sealed as View;

mod sealed {
    use tutti_buffers::Float;

    use super::{Chunk, MeterBindError};

    /// Makes the view from a chunk; unnameable outside the crate.
    pub trait Sealed<'a, S: Float>: Sized {
        fn view<'w: 'a>(chunk: &'a mut Chunk<'w, S>) -> Result<Self, MeterBindError>;
    }
}

impl<'a, S: Float> AudioInput<'a, S> for Interleaved<'a, S> {}

impl<'a, S: Float> sealed::Sealed<'a, S> for Interleaved<'a, S> {
    fn view<'w: 'a>(chunk: &'a mut Chunk<'w, S>) -> Result<Self, MeterBindError> {
        let layout = chunk.layout;
        match &chunk.held {
            Layout::Interleaved => Ok(Interleaved::new(&chunk.data, layout)?),
            Layout::Planar(planes) => {
                let frames = chunk.frames();
                chunk.interleaved.clear();
                chunk
                    .interleaved
                    .resize(layout.interleaved_len(frames), S::ZERO);
                interleave(
                    Planes::new(planes)?,
                    InterleavedMut::new(&mut chunk.interleaved, layout)?,
                )?;
                Ok(Interleaved::new(&chunk.interleaved, layout)?)
            }
        }
    }
}

impl<'a, S: Float> AudioInput<'a, S> for Mono<'a, S> {}

impl<'a, S: Float> sealed::Sealed<'a, S> for Mono<'a, S> {
    fn view<'w: 'a>(chunk: &'a mut Chunk<'w, S>) -> Result<Self, MeterBindError> {
        let layout = chunk.layout;
        if layout == ChannelLayout::MONO {
            return Ok(match &chunk.held {
                Layout::Interleaved => Mono::new(&chunk.data),
                Layout::Planar(planes) => Mono::new(planes[0]),
            });
        }
        let fold = chunk.to_mono.as_ref().map_err(|error| *error)?;
        chunk.mono.clear();
        chunk
            .mono
            .resize(ChannelLayout::MONO.interleaved_len(chunk.frames()), S::ZERO);
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

impl<'a, S: Float> AudioInput<'a, S> for StereoPlanes<'a, S> {}

impl<'a, S: Float> sealed::Sealed<'a, S> for StereoPlanes<'a, S> {
    fn view<'w: 'a>(chunk: &'a mut Chunk<'w, S>) -> Result<Self, MeterBindError> {
        let layout = chunk.layout;
        if layout != ChannelLayout::STEREO {
            return Err(MeterBindError::Width {
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
                    plane.resize(frames.get(), S::ZERO);
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
