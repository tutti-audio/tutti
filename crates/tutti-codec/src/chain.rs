//! [`Chain`]: the streaming pipeline in front of a file's encoder.

use crate::{
    Converter, EncodeError, Encoder, EncoderOptions, FileEncoder, FinalizedFile, Format, Kept,
    Resample, Result, Spec, ToFile,
};
use std::io::{Seek, Write};
use tutti_buffers::{
    ChannelLayout, ChannelSet, Float, Fold, Interleaved, InterleavedMut, Sample, convert,
};
use tutti_dsp::{FftFloat, resampled_frames};
use tutti_units::{Amplitude, Db, SampleRate, Samples};

/// The stages every block of an [`Encoding`](crate::Encoding) goes through, and the buffers they
/// run in: fold, resample, gain, then the encoder's own quantize.
///
/// `F` is the float format the stages run in. A block that needs none of them is
/// handed to the encoder as it is, in its own format.
pub(crate) struct Stages<F: FftFloat> {
    dst: ChannelLayout,
    fold: Option<Fold<F>>,
    /// The block converted to `F`, and then folded, interleaved.
    converted: Vec<F>,
    folded: Vec<F>,
    converter: Option<Converter<F>>,
    staging: Vec<F>,
    /// A gain applied after the converter and before the quantize: the normalize
    /// of a file, which was measured at the file's rate.
    gain: Option<Amplitude>,
}

/// A destination of encoded blocks.
pub(crate) trait Encode {
    fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()>;
}

impl Encode for FileEncoder {
    fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        FileEncoder::write(self, block)
    }
}

impl<W: Write + Seek> Encode for Encoder<W> {
    fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        Encoder::write(self, block)
    }
}

/// What a file of `format` is written as, with `channels` when stated and else the source's,
/// at `rate`.
pub(crate) fn spec_of(
    format: Format,
    channels: Option<ChannelSet>,
    source: &ChannelSet,
    rate: SampleRate,
) -> Spec {
    Spec::of_channels(format, channels.unwrap_or(*source), rate)
}

/// The options of an encoder of `spec` for `frames` source frames at `source_rate`: the
/// length the file will hold is judged at the file's rate.
pub(crate) fn options_of(
    spec: &Spec,
    source_rate: SampleRate,
    frames: Samples,
) -> EncoderOptions<'_> {
    EncoderOptions::new(spec).planned(resampled_frames(source_rate, spec.rate(), frames))
}

impl<F: FftFloat> Stages<F> {
    /// The stages onto `channels` (the source's when not stated) and through `resample`,
    /// from `source` channels at `source_rate`.
    pub(crate) fn new(
        channels: Option<ChannelSet>,
        resample: Option<Resample>,
        source: &ChannelSet,
        source_rate: SampleRate,
    ) -> Result<Self> {
        let dst = channels.unwrap_or(*source);
        let layout = dst.width();
        let converter = match resample {
            Some(resample) => resample.converter(layout, source_rate)?,
            None => None,
        };
        Ok(Self {
            dst: layout,
            fold: fold_between(source, &dst)?,
            converted: Vec::new(),
            folded: Vec::new(),
            converter,
            staging: Vec::new(),
            gain: None,
        })
    }

    pub(crate) fn set_gain(&mut self, gain: Db) {
        self.gain = Some(gain.to_amplitude());
    }

    /// One block of the source through the stages into `to`.
    pub(crate) fn block<B: Sample>(
        &mut self,
        block: Interleaved<'_, B>,
        to: &mut impl Encode,
    ) -> Result<()> {
        let Some(fold) = &self.fold else {
            return self.resample(block, to);
        };
        let mut folded = std::mem::take(&mut self.folded);
        self.converted.clear();
        self.converted.resize(block.samples().len(), F::SILENCE);
        let result = convert(
            block,
            InterleavedMut::new(&mut self.converted, block.layout())?,
        )
        .and_then(|()| {
            folded.clear();
            folded.resize(self.dst.interleaved_len(block.frames()), F::SILENCE);
            fold.fold(
                Interleaved::new(&self.converted, block.layout())?,
                InterleavedMut::new(&mut folded, self.dst)?,
            )
        })
        .map_err(EncodeError::from)
        .and_then(|()| self.resample(Interleaved::new(&folded, self.dst)?, to));
        self.folded = folded;
        result
    }

    /// A folded block through the converter, the gain and into `to`.
    fn resample<B: Sample>(
        &mut self,
        block: Interleaved<'_, B>,
        to: &mut impl Encode,
    ) -> Result<()> {
        let gain = self.gain;
        if let Some(converter) = self.converter.as_mut() {
            converter.push(block, |out| scaled(out, gain, to))
        } else if let Some(gain) = gain {
            self.staging.clear();
            self.staging.resize(block.samples().len(), F::SILENCE);
            convert(
                block,
                InterleavedMut::new(&mut self.staging, block.layout())?,
            )?;
            scaled(
                InterleavedMut::new(&mut self.staging, block.layout())?,
                Some(gain),
                to,
            )
        } else {
            to.write(block)
        }
    }

    /// What the converter still holds, into `to`; with no converter nothing is held.
    pub(crate) fn flush(&mut self, to: &mut impl Encode) -> Result<()> {
        let gain = self.gain;
        match self.converter.as_mut() {
            Some(converter) => converter.finish(|out| scaled(out, gain, to)),
            None => Ok(()),
        }
    }
}

/// `block` times `gain`, into `to`.
fn scaled<F: Float>(
    mut block: InterleavedMut<'_, F>,
    gain: Option<Amplitude>,
    to: &mut impl Encode,
) -> Result<()> {
    if let Some(gain) = gain {
        let gain = <F as Sample>::from_f64(gain.get());
        for s in block.samples_mut() {
            *s *= gain;
        }
    }
    to.write(block.as_interleaved())
}

/// The fold of `src`'s channels onto `dst`'s, as an output's is built
/// ([`Fold::to_channels`]), or `None` when they are the same channels and the block goes as
/// it is.
fn fold_between<F: Float>(src: &ChannelSet, dst: &ChannelSet) -> Result<Option<Fold<F>>> {
    if src == dst {
        return Ok(None);
    }
    Ok(Some(Fold::to_channels(src, dst).build()?))
}

/// The streaming file pipeline: blocks of a source go through the [`Encoding`](crate::Encoding)'s stages
/// into a file begun for a [`ToFile`].
///
/// The file is written to a temporary beside its path (which the encoder removes unless it
/// is finalized and committed) unless the destination is `in_place`. A render the format
/// cannot hold is refused by [`new`](Self::new), before the temporary exists: the encoder
/// judges the frames the file will hold, which are the planned frames at the output rate
/// ([`tutti_dsp::resampled_frames`]).
///
/// [`block`](Self::block) takes the blocks, [`finalize`](Self::finalize) closes the file
/// beside its path, and [`Closed::commit`] puts it in place: all-or-nothing across several
/// files is the caller's, between those two.
///
/// A chain dropped before `finalize` removes its file. A block that failed is recorded as the
/// file's first error, and `finalize` then keeps what was written beside the path and
/// reports it ([`Kept`]): the caller owns the unfinished file, to move or delete.
///
/// `F` is the format the converter and the fold run in.
pub struct Chain<F: FftFloat> {
    stages: Stages<F>,
    encoder: FileEncoder,
    /// Frames the encoder accepted: what the file holds.
    frames: Samples,
}

impl<F: FftFloat> std::fmt::Debug for Chain<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

/// An encoder that counts the frames it accepts.
struct Counted<'a> {
    encoder: &'a mut FileEncoder,
    frames: &'a mut Samples,
}

impl Encode for Counted<'_> {
    fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        self.encoder.write(block)?;
        *self.frames += block.frames();
        Ok(())
    }
}

impl<F: FftFloat> Chain<F> {
    /// The chain into `to`, through its encoding, from frames of `source` channels at
    /// `source_rate`, of which `planned` are expected (at the source's rate).
    ///
    /// # Errors
    ///
    /// [`EncodeError::Spec`] and [`EncodeError::WavTooLarge`] for a file the format cannot
    /// hold, [`EncodeError::Exists`] for an existing path without
    /// [`replace`](ToFile::replace), [`EncodeError::Fold`] for channels the fold cannot
    /// place, [`EncodeError::Resample`] for a rate or width the converter cannot take, and
    /// the errors of [`EncoderOptions::wrap`] otherwise.
    pub fn new(
        to: &ToFile,
        source: &ChannelSet,
        source_rate: SampleRate,
        planned: Samples,
    ) -> Result<Self> {
        let stages = Stages::new(to.channel_set(), to.resampling(), source, source_rate)?;
        let spec = spec_of(to.format(), to.channel_set(), source, to.rate(source_rate));
        let encoder = to.create(options_of(&spec, source_rate, planned))?;
        Ok(Self {
            stages,
            encoder,
            frames: Samples::ZERO,
        })
    }

    /// Scales everything from here on by `gain`, after the converter and before the
    /// quantize (the gain of a normalize, measured at the file's rate). Without it the
    /// gain is 0 dB.
    pub fn set_gain(&mut self, gain: Db) {
        self.stages.set_gain(gain);
    }

    /// The width a block is written at: the file's.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.stages.dst
    }

    /// One block of the source through the stages into the encoder.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Buffer`] for a block of another width than the source's, and the
    /// encoder's errors. The first failure is kept for [`finalize`](Self::finalize).
    pub fn block<B: Sample>(&mut self, block: Interleaved<'_, B>) -> Result<()> {
        let mut to = Counted {
            encoder: &mut self.encoder,
            frames: &mut self.frames,
        };
        let written = self.stages.block(block, &mut to);
        if let Err(e) = &written {
            self.encoder.fail(e);
        }
        written
    }

    /// Flushes the converter and closes the file beside its path: everything that can
    /// fail for the file's own sake, short of moving it.
    ///
    /// # Errors
    ///
    /// [`Kept::Unfinished`] when the file cannot be closed whole (the first failed block, the
    /// flush, the header, the close): the file is kept beside its path with the frames written,
    /// and the error names it.
    pub fn finalize(mut self) -> std::result::Result<Closed, Kept> {
        let mut to = Counted {
            encoder: &mut self.encoder,
            frames: &mut self.frames,
        };
        if let Err(e) = self.stages.flush(&mut to) {
            self.encoder.fail(&e);
        }
        let clipped = self.encoder.clipped();
        Ok(Closed {
            file: self.encoder.finalize_keeping()?,
            clipped,
            frames: self.frames,
        })
    }
}

/// What a [`Chain`] finalized into: the file closed beside its path and not yet put in
/// place, and what the pass counted of it.
#[derive(Debug)]
#[must_use = "a closed file is not in place until `commit`"]
pub struct Closed {
    file: FinalizedFile,
    clipped: u64,
    frames: Samples,
}

impl Closed {
    /// Puts the file in place. A file that could not be put in place is kept where it was
    /// written, and the error ([`EncodeError::Commit`]) names it.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Commit`].
    pub fn commit(self) -> Result<Landed> {
        self.file.commit()?;
        Ok(Landed {
            clipped: self.clipped,
            frames: self.frames,
        })
    }
}

/// A file that was put in place: the samples that clipped on the way and the frames it
/// holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Landed {
    clipped: u64,
    frames: Samples,
}

impl Landed {
    /// Values the conversion to an integer format clipped (after any gain, resample and
    /// dither): those whose rounded value lies past the format's `MAX` or `MIN`, held there
    /// by the rule in [`Sample`]; an infinity is counted. A NaN is written as 0 and is not
    /// counted. `0` for a format that does not quantize.
    #[must_use]
    pub fn clipped(&self) -> u64 {
        self.clipped
    }

    /// Frames the file holds, at the file's rate.
    #[must_use]
    pub fn frames(&self) -> Samples {
        self.frames
    }
}
