//! The [`Encoder`]: one type over every format.

use crate::error::{EncodeError, Result};
#[cfg(encode)]
use crate::format::Format;
use crate::pcm::{IntDepth, IntPcm};
use crate::spec::Spec;
use crate::writer::FormatWriter;
use std::io::{BufWriter, Seek, Write};
use std::path::PathBuf;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{I24, Interleaved, InterleavedMut, Sample, convert};
use tutti_dsp::{DitherNoise, NoiseShaping, Quantizer};
use tutti_platform::{Staged, StagedCommitError, StagedFile, StagedKeep};
use tutti_units::{SampleRate, Samples};

/// Frames quantized at a time: the stage's whole scratch, allocated at [`Encoder::create`].
const CHUNK: Samples = Samples::new(1024);

/// Writes blocks of audio as one file of a [format](crate::Format): whichever
/// format the [`Spec`] names, behind one type.
///
/// An encoder takes blocks already at the [`Spec`]'s rate and width: it does not
/// pull or resample, and nothing here stops a caller from handing it
/// blocks that were not. The pipeline in front of it is the caller's. An integer file
/// is dithered by the encoder itself, as its [`IntPcm`] says (see [`write`](Self::write)),
/// so no path forgets to. [`write`](Self::write) is fallible, so a full disk in the middle
/// of a take is an error at the block that hit it, not a short file that
/// reports success; [`finish`](Self::finish) closes the file exactly once.
///
/// The sink is any `Write + Seek`: a file, or memory. Every block is encoded on
/// the thread that calls [`write`](Self::write), and an `Encoder` is `Send` when
/// its sink is, so a recording can move it to its pump thread. A file written
/// beside its path and moved into place when whole is a [`FileEncoder`].
///
/// ```
/// # #[cfg(feature = "encode-wav")] {
/// use std::io::Cursor;
/// use tutti_codec::{Encoder, Format, IntDepth, IntPcm, Pcm, Spec};
/// use tutti_buffers::{ChannelLayout, ChannelSet};
/// use tutti_units::SampleRate;
/// use tutti_buffers::{Interleaved, Sample};
///
/// let pcm = Pcm::Int(IntPcm::new(IntDepth::Int16));
/// let spec = Spec::of_channels(Format::Wav(pcm), ChannelSet::default_for(ChannelLayout::STEREO), SampleRate::SR_48K);
/// let mut encoder = Encoder::create(&spec, Cursor::new(Vec::new()))?;
/// encoder.write(Interleaved::new(&[0.5, -0.5, 0.25, -0.25], ChannelLayout::STEREO)?)?;
/// let wav = encoder.finish()?.into_inner();
/// assert_eq!(&wav[..4], b"RIFF");
/// assert_eq!(wav.len(), 44 + 2 * 2 * 2); // a header, two stereo frames of 16 bits
/// # }
/// # Ok::<(), tutti_codec::EncodeError>(())
/// ```
pub struct Encoder<W: Write + Seek> {
    inner: Inner<W>,
    layout: ChannelLayout,
    /// The quantize stage of an integer file; a float file has none.
    quantize: Option<Quantize>,
    /// The first failure of a `write`, kept for `finish` to report.
    first_error: Option<EncodeError>,
}

impl<W: Write + Seek> std::fmt::Debug for Encoder<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

enum Inner<W: Write + Seek> {
    #[cfg(feature = "encode-wav")]
    Wav(crate::wav::WavWriter<W>),
    #[cfg(feature = "encode-aiff")]
    Aiff(Box<crate::aiff::AiffWriter<W>>),
    #[cfg(feature = "encode-flac")]
    Flac(Box<crate::flac::FlacWriter<W>>),
    // No format feature: `Format` has no value, so neither has this.
    #[cfg(not(encode))]
    Never(std::convert::Infallible, std::marker::PhantomData<W>),
}

/// Runs `$body` on whichever format's writer `$inner` holds: the one place the
/// variants are listed beside their features.
macro_rules! each_format {
    ($inner:expr, $w:ident => $body:expr) => {
        match $inner {
            #[cfg(feature = "encode-wav")]
            Inner::Wav($w) => $body,
            #[cfg(feature = "encode-aiff")]
            Inner::Aiff($w) => $body,
            #[cfg(feature = "encode-flac")]
            Inner::Flac($w) => $body,
            #[cfg(not(encode))]
            Inner::Never(never, _) => absurd(never),
        }
    };
}

/// Stands for any value of the uninhabited type, however it is held.
#[cfg(not(encode))]
fn absurd(never: impl std::borrow::Borrow<std::convert::Infallible>) -> ! {
    match *never.borrow() {}
}

impl<W: Write + Seek> Inner<W> {
    #[cfg_attr(not(encode), expect(unused_variables))]
    fn open(spec: &Spec, sink: W) -> Result<Self> {
        match spec.format() {
            #[cfg(feature = "encode-wav")]
            Format::Wav(pcm) => Ok(Self::Wav(crate::wav::WavWriter::create(spec, pcm, sink)?)),
            #[cfg(feature = "encode-aiff")]
            Format::Aiff(pcm) => Ok(Self::Aiff(Box::new(crate::aiff::AiffWriter::create(
                spec, pcm, sink,
            )?))),
            #[cfg(feature = "encode-flac")]
            Format::Flac { pcm, level } => Ok(Self::Flac(Box::new(
                crate::flac::FlacWriter::create(spec, pcm.depth(), level, sink)?,
            ))),
        }
    }
}

impl<W: Write + Seek> FormatWriter for Inner<W> {
    type Sink = W;

    fn limit_frames(&mut self, frames: u64) {
        each_format!(self, w => w.limit_frames(frames))
    }

    fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        each_format!(self, w => w.write(block))
    }

    fn finish(self) -> Result<W> {
        each_format!(self, w => w.finish())
    }
}

/// [`EncodeError::WavTooLarge`] when `frames` is past the spec's format's limit.
pub(crate) fn check_planned(spec: &Spec, frames: Samples) -> Result<()> {
    match spec.format().max_frames(spec.layout()) {
        Some(limit_frames) if frames.get() as u64 > limit_frames => {
            Err(EncodeError::WavTooLarge { limit_frames })
        }
        _ => Ok(()),
    }
}

/// The scratch an integer file's chunk is quantized into, as the file's depth stores it.
enum Ints {
    I16(Vec<i16>),
    I24(Vec<I24>),
}

/// The quantize stage of an integer file: the dither and rounding of a block that has more
/// resolution than the file's depth.
///
/// A chunk is widened to `f64` once, given its noise and error feedback by the
/// [`Quantizer`] in `f64`, and narrowed to the file's integer there: no step goes through `f32`.
struct Quantize {
    pcm: IntPcm,
    layout: ChannelLayout,
    rate: SampleRate,
    quantizer: Quantizer<f64>,
    /// The chunk as `f64`.
    widened: Vec<f64>,
    /// The chunk as the file's integer.
    ints: Ints,
    clipped: u64,
}

impl Quantize {
    /// The stage for an integer file of `pcm`, blocks of `layout` at `rate`, its noise the
    /// sequence of `seed`.
    fn new(pcm: IntPcm, layout: ChannelLayout, rate: SampleRate, seed: u64) -> Self {
        let room = layout.interleaved_len(CHUNK);
        let mut quantizer = Quantizer::new(pcm.dither_shape(), layout, seed);
        if pcm.is_noise_shaped() {
            quantizer = quantizer.shaped(&NoiseShaping::new(rate));
        }
        Self {
            pcm,
            layout,
            rate,
            quantizer,
            widened: Vec::with_capacity(room),
            ints: match pcm.depth() {
                IntDepth::Int16 => Ints::I16(Vec::with_capacity(room)),
                IntDepth::Int24 => Ints::I24(Vec::with_capacity(room)),
            },
            clipped: 0,
        }
    }

    /// The same stage with the noise of `seed`.
    fn seeded(self, seed: u64) -> Self {
        Self::new(self.pcm, self.layout, self.rate, seed)
    }

    /// Whether a block of `S` has more resolution than the file's depth: a float, or a wider
    /// integer. Only such a block is brought down here; a block already on the file's grid, or
    /// coarser, is written as it is.
    fn lowers<S: Sample>(&self) -> bool {
        S::STEP.is_none_or(|step| step < self.pcm.depth().lsb().get())
    }

    /// Quantizes `block` a chunk at a time and writes each chunk to `inner`.
    fn write<S: Sample, W: Write + Seek>(
        &mut self,
        inner: &mut Inner<W>,
        block: Interleaved<'_, S>,
    ) -> Result<()> {
        let mut at = Samples::ZERO;
        while at < block.frames() {
            let end = (at + CHUNK).min(block.frames());
            let part = block.slice(at..end);
            // Within the capacity `new` reserved: neither grows.
            self.widened.clear();
            self.widened.resize(part.samples().len(), 0.0);
            let mut widened = InterleavedMut::new(&mut self.widened, self.layout)?;
            convert(part, widened.reborrow())?;
            let widened = widened.as_interleaved();
            self.clipped += match &mut self.ints {
                Ints::I16(ints) => quantize_chunk(&mut self.quantizer, widened, ints, inner)?,
                Ints::I24(ints) => quantize_chunk(&mut self.quantizer, widened, ints, inner)?,
            };
            at = end;
        }
        Ok(())
    }
}

/// `widened` quantized to `I` in `ints` and written to `inner`; the values that clipped.
fn quantize_chunk<I: Sample, W: Write + Seek>(
    quantizer: &mut Quantizer<f64>,
    widened: Interleaved<'_, f64>,
    ints: &mut Vec<I>,
    inner: &mut Inner<W>,
) -> Result<u64> {
    let layout = widened.layout();
    ints.clear();
    ints.resize(widened.samples().len(), I::SILENCE);
    let clipped = quantizer.quantize(widened, InterleavedMut::new(ints, layout)?);
    inner.write(Interleaved::new(ints, layout)?)?;
    Ok(clipped)
}

impl<W: Write + Seek> Encoder<W> {
    /// Starts a file of `spec` in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Spec`] when the format cannot be written for `spec` (a zero
    /// rate or width, a width past the format's limit, a topology of another
    /// width); [`EncodeError::Encoding`] when the codec rejects it; [`EncodeError::Io`]
    /// when the sink fails on the header.
    pub fn create(spec: &Spec, sink: W) -> Result<Self> {
        spec.channels()?;
        Ok(Self {
            inner: Inner::open(spec, sink)?,
            layout: spec.layout(),
            quantize: spec.format().int_pcm().map(|pcm| {
                Quantize::new(pcm, spec.layout(), spec.rate(), DitherNoise::DEFAULT_SEED)
            }),
            first_error: None,
        })
    }

    /// Sets the noise sequence an integer file's dither draws from, which
    /// [`create`](Self::create) starts at [`DitherNoise::DEFAULT_SEED`]; a float file has
    /// no dither and ignores it. Call it before the first write.
    ///
    /// Encoders of signals that are summed later (a bounce's stems, takes armed together)
    /// take seeds mixed from distinct indices ([`SplitMix64::mix`](tutti_rt::SplitMix64::mix)),
    /// never one seed, which would add their noise coherently.
    #[must_use]
    pub fn dither_seed(mut self, seed: u64) -> Self {
        self.quantize = self.quantize.map(|q| q.seeded(seed));
        self
    }

    /// The values the quantize stage has clipped so far: a float or wider integer value past
    /// the file's range, held at its rail. `0` for a float file, which clips nothing.
    #[must_use]
    pub fn clipped(&self) -> u64 {
        self.quantize.as_ref().map_or(0, |q| q.clipped)
    }

    /// Lowers the number of frames a WAV file may hold to `frames`; it never
    /// raises the limit the format's 32-bit sizes set, and other formats are
    /// unbounded. For a caller that records into a space it knows.
    #[must_use]
    pub fn limit_frames(mut self, frames: u64) -> Self {
        self.inner.limit_frames(frames);
        self
    }

    /// Writes one block of any [`Sample`] format; the file's sample type is
    /// reached by the rule in [`Sample`], and a block already in it is written
    /// as it is.
    ///
    /// Where resolution drops, an integer file is dithered as its [`IntPcm`] says: a float
    /// block, or an integer one wider than the file's depth (an `i32` into 16 bits). An
    /// integer block as narrow as the file, or narrower, and every block into a float file, is
    /// converted exactly.
    ///
    /// An empty block writes nothing.
    ///
    /// # Errors
    ///
    /// [`EncodeError::BlockWidth`] for a block of another width than the spec's;
    /// [`EncodeError::Io`] when the sink fails (a full disk); [`EncodeError::WavTooLarge`]
    /// when a WAV file reaches its size limit, after the frames that fit were
    /// written; [`EncodeError::Encoding`] when the codec rejects the block. After an
    /// error the file is not a complete take: [`finish`](Self::finish) still
    /// closes what was written for a caller that keeps it, and dropping the
    /// encoder discards it.
    pub fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        let written = self.write_block(block);
        if let Err(e) = &written {
            self.first_error.get_or_insert_with(|| e.duplicate());
        }
        written
    }

    fn write_block<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        if block.layout() != self.layout {
            return Err(EncodeError::BlockWidth {
                expected: self.layout,
                got: block.layout(),
            });
        }
        match &mut self.quantize {
            Some(quantize) if quantize.lowers::<S>() => quantize.write(&mut self.inner, block),
            _ => self.inner.write(block),
        }
    }

    /// Closes the file: back-patches its sizes and flushes the sink. Returns
    /// the sink, positioned at the end of the file.
    ///
    /// After a failed [`write`](Self::write) the sink gets what was written
    /// closed and handed back (a recording keeps its readable prefix; the
    /// failure was the `write`'s to report). A take that must be whole is a
    /// [`FileEncoder`] instead.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Io`] or [`EncodeError::Encoding`] when closing fails.
    pub fn finish(self) -> Result<W> {
        let mut sink = self.inner.finish()?;
        // A buffered sink reports a failed write only when it is flushed, and
        // a writer that does not flush would lose it to the sink's `Drop`.
        sink.flush()?;
        Ok(sink)
    }
}

/// An encoder that writes a file beside its path and moves it into place
/// when whole.
///
/// The file is written to a temporary beside the path and put in place by
/// [`finish`](Self::finish) (synced and closed first), so the path holds
/// either what it held before or a complete new file, never a partial take. A
/// take that fails a `write`, or an encoder dropped before `finish`, removes the
/// temporary and leaves the path alone; a commit that fails keeps the file. The commit is the encoder's own, so a
/// file is never put in place while its sink is open, and never left
/// uncommitted by a caller that kept the sink.
///
/// Created from a path with [`create`](Self::create) (which replaces an
/// existing file, on Windows too, and a symlink at the path rather than writing
/// through it; the new file has the default permissions).
pub struct FileEncoder {
    encoder: Encoder<BufWriter<StagedFile>>,
    keep: StagedKeep,
    temp: PathBuf,
    spec: Spec,
}

impl std::fmt::Debug for FileEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileEncoder")
            .field("encoder", &self.encoder)
            .finish()
    }
}

impl FileEncoder {
    /// Starts a file for `target`: a path (which an existing file there is
    /// replaced by) or a [`Staged`], whose [`create_new`](Staged::create_new)
    /// refuses an existing file at the commit with [`EncodeError::Commit`] of kind
    /// `AlreadyExists`, keeping the finished file.
    ///
    /// # Errors
    ///
    /// As [`Encoder::create`], and [`EncodeError::Io`] when the temporary cannot be
    /// created.
    pub fn create(spec: &Spec, target: impl Into<Staged>) -> Result<Self> {
        // Created before the header is written: a failure from here on drops
        // the file, which removes the temporary.
        let file = target.into().open()?;
        let keep = file.keeper();
        let temp = file.temp().to_path_buf();
        let sink = BufWriter::new(file);
        Ok(Self {
            encoder: Encoder::create(spec, sink)?,
            keep,
            temp,
            spec: spec.clone(),
        })
    }

    /// Starts a file for `target` that will hold `frames` frames, refusing a
    /// count the format's file cannot hold before the temporary exists.
    ///
    /// For a caller that knows the length up front (an export), so it learns
    /// that a WAV is too long before it renders a frame, from the codec's own
    /// limit ([`crate::Format::max_frames`]) and its own error. It does not lower the
    /// limit on the writes.
    ///
    /// # Errors
    ///
    /// [`EncodeError::WavTooLarge`] when `frames` is past [`crate::Format::max_frames`] for
    /// the spec's width; otherwise as [`create`](Self::create).
    pub fn create_planned(spec: &Spec, target: impl Into<Staged>, frames: Samples) -> Result<Self> {
        check_planned(spec, frames)?;
        Self::create(spec, target)
    }

    /// Sets the noise sequence of an integer file's dither: [`Encoder::dither_seed`].
    #[must_use]
    pub fn dither_seed(self, seed: u64) -> Self {
        Self {
            encoder: self.encoder.dither_seed(seed),
            ..self
        }
    }

    /// The values the file's quantize stage has clipped so far: [`Encoder::clipped`].
    #[must_use]
    pub fn clipped(&self) -> u64 {
        self.encoder.clipped()
    }

    /// As [`Encoder::write`].
    ///
    /// # Errors
    ///
    /// As [`Encoder::write`]; the first failure is kept and returned by
    /// [`finish`](Self::finish) too.
    pub fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        self.encoder.write(block)
    }

    /// Closes the file and moves it onto its path: [`finalize`](Self::finalize)
    /// then [`FinalizedFile::commit`].
    ///
    /// # Errors
    ///
    /// As [`finalize`](Self::finalize), and [`EncodeError::Commit`] when the sync or
    /// the move fails: the path is left alone and the complete file is kept.
    pub fn finish(self) -> Result<()> {
        self.finalize()?.commit()
    }

    /// Closes the file as [`finish`](Self::finish) does, but leaves it beside
    /// its path: [`FinalizedFile::commit`] moves it, and dropping the
    /// [`FinalizedFile`] removes it and leaves what was at the path alone.
    ///
    /// The split is for a caller that writes several files and wants every one
    /// closed (the work that can fail for the file's own sake) before any is
    /// put in place.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Io`] or [`EncodeError::Encoding`] when closing fails, or the first
    /// failed `write`: a take must be whole, so the file is removed.
    pub fn finalize(self) -> Result<FinalizedFile> {
        close(self.encoder)
    }

    /// As [`finish`](Self::finish), for a recording that must never be lost:
    /// when the file cannot be closed whole (a failed `write`, the header, the
    /// close) it is **kept** beside its path as an [`Unfinished`] instead of
    /// removed: the target is untouched, nothing removes the file, and it is
    /// the caller's to move or delete. A commit that fails keeps the complete
    /// file as [`finish`](Self::finish) does ([`Kept::Commit`]).
    ///
    /// What was buffered is flushed on a best effort, and a failure to flush or
    /// to sync is not reported. A header that states lengths is then patched
    /// from what is on disk, as a finished file's is from what was written: the
    /// sizes are the whole frames in the file, and a torn last frame is cut
    /// off. This holds for WAV (the RIFF and `data` sizes) and AIFF (the COMM
    /// frame count, the SSND and FORM sizes). FLAC states no length it needs (a
    /// zero STREAMINFO total and MD5 mean unknown), so neither is patched; the
    /// last FLAC frame may be cut short. [`Unfinished::header_describes_frames`] is `false` when a WAV's or
    /// AIFF's patch failed, and the file is kept all the same: its header then
    /// states no frames.
    ///
    /// # Errors
    ///
    /// [`Kept`], with the error [`finish`](Self::finish) would give.
    pub fn finish_keeping(self) -> std::result::Result<(), Kept> {
        let Self {
            encoder,
            keep,
            temp,
            spec,
        } = self;
        // Set before the writer runs: it drops the file on a failure.
        keep.keep();
        match close(encoder) {
            Ok(file) => {
                keep.release();
                file.commit_staged().map_err(Kept::from)
            }
            Err(source) => Err(Kept::Unfinished {
                source,
                unfinished: patched(temp, &spec),
            }),
        }
    }
}

/// The encoder's file closed, flushed and its header lengths written from the
/// frames on disk where the format has them: the file at `temp`, which the
/// caller has made outlive the encoder.
fn patched(temp: PathBuf, spec: &Spec) -> Unfinished {
    let header_describes_frames = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temp)
        .and_then(|mut file| patch_header(spec, &mut file))
        .is_ok();
    Unfinished {
        path: temp,
        header_describes_frames,
    }
}

/// [`FileEncoder::finalize`]'s body: the encoder closed and its sink taken
/// back as a [`FinalizedFile`].
fn close(mut encoder: Encoder<BufWriter<StagedFile>>) -> Result<FinalizedFile> {
    let first_error = encoder.first_error.take();
    let sink = encoder.finish()?;
    if let Some(error) = first_error {
        return Err(error);
    }
    let file = sink.into_inner().map_err(|e| e.into_error())?;
    Ok(FinalizedFile { file })
}

/// Why [`FileEncoder::finish_keeping`] did not put the file in place; either
/// way the file is kept.
#[derive(Debug, thiserror::Error)]
pub enum Kept {
    /// The complete file could not be put in place; it is kept where the error
    /// says ([`StagedCommitError::kept`]).
    #[error(transparent)]
    Commit(#[from] StagedCommitError),
    /// The file could not be closed whole (a failed `write`, the header, the
    /// close); it is kept unfinished.
    #[error("{source}")]
    Unfinished {
        /// What failed.
        #[source]
        source: EncodeError,
        /// The file kept.
        unfinished: Unfinished,
    },
}

/// The unfinished file [`FileEncoder::finish_keeping`] kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unfinished {
    /// Where the file is, beside the path it was meant for.
    pub path: PathBuf,
    /// Whether the file's header describes the frames kept: `true` when the
    /// header now states the lengths of the frames on disk (a WAV or AIFF that
    /// was patched), or needs none (FLAC, whose STREAMINFO total of 0 means
    /// unknown). `false`
    /// only when a patch the format needs failed.
    pub header_describes_frames: bool,
}

/// Writes the lengths of the frames on disk into an unfinished file's header,
/// for a format that states them.
#[cfg_attr(
    not(any(feature = "encode-wav", feature = "encode-aiff")),
    allow(unused_variables, unreachable_code)
)]
fn patch_header(spec: &Spec, file: &mut std::fs::File) -> std::io::Result<()> {
    match spec.format() {
        #[cfg(feature = "encode-wav")]
        Format::Wav(pcm) => crate::wav::patch_unfinished(file, spec, pcm),
        #[cfg(feature = "encode-aiff")]
        Format::Aiff(pcm) => {
            let block_align = u64::from(pcm.bits().div_ceil(8)) * u64::from(spec.layout().count());
            crate::aiff::patch_unfinished(file, block_align.max(1))
        }
        #[allow(
            unreachable_patterns,
            reason = "the only `Format` variant when one encoder is the only feature on"
        )]
        _ => Ok(()),
    }
}

/// A [`FileEncoder`]'s closed file, not yet put in place
/// ([`FileEncoder::finalize`]).
#[must_use = "a finalized file is removed when dropped; commit it"]
#[derive(Debug)]
pub struct FinalizedFile {
    file: StagedFile,
}

impl FinalizedFile {
    /// Syncs the file and moves it onto its path (replacing what is there,
    /// unless the destination was `create_new`).
    ///
    /// # Errors
    ///
    /// [`EncodeError::Commit`] when the sync or the move fails (or the path exists
    /// under `create_new`): what was at the path is left alone and the complete
    /// file is kept at the error's `kept` path, the caller's to move or delete.
    pub fn commit(self) -> Result<()> {
        Ok(self.commit_staged()?)
    }

    fn commit_staged(self) -> std::result::Result<(), StagedCommitError> {
        self.file.commit()
    }
}

#[cfg(all(test, encode))]
mod tests;
