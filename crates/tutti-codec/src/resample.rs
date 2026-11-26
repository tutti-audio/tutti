//! Sample-rate conversion on the way out: [`Resample`] and the [`Converter`] it builds.

use crate::{EncodeError, Result};
use tutti_buffers::{BufferError, PlanesMut};
use tutti_buffers::{
    ChannelLayout, Interleaved, InterleavedMut, Planes, Sample, deinterleave, interleave,
};
use tutti_dsp::{
    ChunkSize, FftFloat, StreamingResampler, StreamingResamplerConfig, needs_resampling,
};
use tutti_units::{SampleRate, Samples};

/// Sample-rate conversion applied on the way out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resample {
    /// Rate the file is written at. Equal to the source rate means no
    /// conversion: the comparison is made on the integer rate the codec headers
    /// speak, so two rates an ULP apart are the same rate.
    pub target_rate: SampleRate,
    /// Input FRAMES per FFT chunk, and how finely each is subdivided. Sets the
    /// converter's anti-alias steepness and its latency.
    pub chunk: ChunkSize,
}

impl Resample {
    /// Convert to `target_rate` with the default [`ChunkSize`].
    pub fn to(target_rate: impl Into<SampleRate>) -> Self {
        Self {
            target_rate: target_rate.into(),
            chunk: ChunkSize::default(),
        }
    }

    /// The converter from frames at `source_rate` of `layout` channels, or
    /// `None` when they are already at the target rate: whether to convert at
    /// all is [`tutti_dsp::needs_resampling`]'s to say. `source_rate` is the
    /// rate of the frames that will be pushed, not a rate this value knows.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Resample`] for a rate or width the resampler cannot take.
    pub fn converter<F: FftFloat>(
        &self,
        layout: ChannelLayout,
        source_rate: SampleRate,
    ) -> Result<Option<Converter<F>>> {
        if !needs_resampling(source_rate, self.target_rate) {
            return Ok(None);
        }
        let ch = usize::from(layout.count());
        let resampler = StreamingResampler::try_from(
            StreamingResamplerConfig::new(layout, source_rate, self.target_rate).chunk(self.chunk),
        )?;
        Ok(Some(Converter {
            resampler,
            layout,
            planes: vec![Vec::new(); ch],
            out: vec![Vec::new(); ch],
            staging: Vec::new(),
        }))
    }
}

/// A streaming resample of interleaved blocks ([`Resample::converter`]): each
/// block goes in, and what the resampler has produced so far comes out as an
/// interleaved block in the converter's format `F`.
pub struct Converter<F: FftFloat> {
    resampler: StreamingResampler<F>,
    layout: ChannelLayout,
    /// The planes a block is split into, and the planes the resampler gives.
    planes: Vec<Vec<F>>,
    out: Vec<Vec<F>>,
    /// The interleaved block handed on, `layout` wide.
    staging: Vec<F>,
}

impl<F: FftFloat> std::fmt::Debug for Converter<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Converter")
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

impl<F: FftFloat> Converter<F> {
    /// Resamples `block`, converted to `F`, and hands what came out to
    /// `emit` (a block that may be written to, so a gain can be applied in
    /// place).
    ///
    /// # Errors
    ///
    /// [`EncodeError::Buffer`] for a block of another width than the
    /// converter's, [`EncodeError::Resample`] when the resampler refuses it,
    /// and `emit`'s own error.
    pub fn push<B: Sample, E: From<EncodeError>>(
        &mut self,
        block: Interleaved<'_, B>,
        emit: impl FnOnce(InterleavedMut<'_, F>) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        for plane in &mut self.planes {
            plane.clear();
            plane.resize(block.frames().get(), F::SILENCE);
        }
        deinterleave(
            block,
            PlanesMut::from_vecs(&mut self.planes).map_err(buffer)?,
        )
        .map_err(buffer)?;
        let planes = Planes::from_vecs(&self.planes).map_err(buffer)?;
        self.resampler
            .push(planes, &mut self.out)
            .map_err(EncodeError::from)?;
        self.emit(emit)
    }

    /// What the resampler still holds, to `emit`; nothing is held after.
    ///
    /// # Errors
    ///
    /// As [`push`](Self::push).
    pub fn finish<E: From<EncodeError>>(
        &mut self,
        emit: impl FnOnce(InterleavedMut<'_, F>) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        self.resampler
            .finish(&mut self.out)
            .map_err(EncodeError::from)?;
        self.emit(emit)
    }

    /// Interleaves the resampler's output planes into `emit`, and clears them
    /// for the next push.
    fn emit<E: From<EncodeError>>(
        &mut self,
        emit: impl FnOnce(InterleavedMut<'_, F>) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let frames = Samples::new(self.out.first().map_or(0, Vec::len));
        self.staging.clear();
        self.staging
            .resize(self.layout.interleaved_len(frames), F::SILENCE);
        let result = if frames == Samples::ZERO {
            Ok(())
        } else {
            let block = Planes::from_vecs(&self.out)
                .and_then(|planes| {
                    interleave(planes, InterleavedMut::new(&mut self.staging, self.layout)?)
                })
                .map_err(buffer);
            match block {
                Ok(()) => {
                    emit(InterleavedMut::new(&mut self.staging, self.layout).map_err(buffer)?)
                }
                Err(e) => Err(e),
            }
        };
        for plane in &mut self.out {
            plane.clear();
        }
        result
    }
}

fn buffer<E: From<EncodeError>>(error: BufferError) -> E {
    E::from(EncodeError::Buffer(error))
}
