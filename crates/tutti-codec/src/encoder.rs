//! The [`Encoder`]: one type over every format.

use crate::error::{EncodeError, Result};
#[cfg(encode)]
use crate::format::Format;
use crate::pcm::{IntDepth, IntPcm};
use crate::spec::Spec;
use crate::writer::FormatWriter;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tutti_buffers::ChannelLayout;
use tutti_buffers::{I24, Interleaved, InterleavedMut, Sample, convert};
use tutti_dsp::{DitherNoise, NoiseShaping, Quantizer};
use tutti_platform::{Staged, StagedCommitError, StagedFile, StagedKeep};
use tutti_units::{SampleRate, Samples};

/// Frames quantized at a time: the stage's whole scratch, allocated when [`EncoderOptions`] finishes.
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
/// beside its path and moved into place when whole is a [`FileEncoder`]. Both are built by
/// [`EncoderOptions`].
///
/// ```
/// # #[cfg(feature = "encode-wav")] {
/// use std::io::Cursor;
/// use tutti_codec::{EncoderOptions, Format, IntDepth, IntPcm, Pcm, Spec};
/// use tutti_buffers::{ChannelLayout, ChannelSet};
/// use tutti_units::SampleRate;
/// use tutti_buffers::{Interleaved, Sample};
///
/// let pcm = Pcm::Int(IntPcm::new(IntDepth::Int16));
/// let spec = Spec::of_channels(Format::Wav(pcm), ChannelSet::default_for(ChannelLayout::STEREO), SampleRate::SR_48K);
/// let mut encoder = EncoderOptions::new(&spec).wrap(Cursor::new(Vec::new()))?;
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
            quantizer,
            widened: Vec::with_capacity(room),
            ints: match pcm.depth() {
                IntDepth::Int16 => Ints::I16(Vec::with_capacity(room)),
                IntDepth::Int24 => Ints::I24(Vec::with_capacity(room)),
            },
            clipped: 0,
        }
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

/// How an [`Encoder`] or a [`FileEncoder`] is made: the spec, then the options, then the sink
/// it finishes in: [`wrap`](Self::wrap) for any `Write + Seek`, or [`file`](Self::file) for a
/// path, whose [`FileOptions`] hold what only a file has.
///
/// An option is fixed when the encoder is made, so none can change mid-file.
///
/// ```
/// # #[cfg(feature = "encode-wav")] {
/// use tutti_codec::{EncoderOptions, Format, IntDepth, IntPcm, Pcm, Spec};
/// use tutti_buffers::{ChannelLayout, ChannelSet, Interleaved};
/// use tutti_units::SampleRate;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = tempfile::tempdir()?;
/// let pcm = Pcm::Int(IntPcm::new(IntDepth::Int24));
/// let spec = Spec::of_channels(Format::Wav(pcm), ChannelSet::default_for(ChannelLayout::STEREO), SampleRate::SR_48K);
/// let mut encoder = EncoderOptions::new(&spec)
///     .dither_seed(7)
///     .file(dir.path().join("tone.wav"))
///     .create()?;
/// encoder.write(Interleaved::new(&[0.5f32, -0.5, 0.25, -0.25], ChannelLayout::STEREO)?)?;
/// encoder.finish()?;
/// let again = EncoderOptions::new(&spec).file(dir.path().join("tone.wav")).create();
/// assert!(matches!(again, Err(tutti_codec::EncodeError::Exists { .. })));
/// # Ok(())
/// # }
/// # }
/// ```
#[derive(Debug, Clone)]
#[must_use = "options make nothing until `wrap` or `file(..).create()`"]
pub struct EncoderOptions<'a> {
    spec: &'a Spec,
    seed: u64,
    limit_frames: Option<Samples>,
    planned: Option<Samples>,
}

impl<'a> EncoderOptions<'a> {
    /// Options for a file of `spec`: the dither of [`DitherNoise::DEFAULT_SEED`], and no
    /// length stated.
    pub fn new(spec: &'a Spec) -> Self {
        Self {
            spec,
            seed: DitherNoise::DEFAULT_SEED,
            limit_frames: None,
            planned: None,
        }
    }

    /// The noise sequence an integer file's dither draws from (a float file has no dither and
    /// ignores it), which starts at [`DitherNoise::DEFAULT_SEED`].
    ///
    /// Encoders of signals that are summed later (a bounce's stems, takes armed together)
    /// take seeds mixed from distinct indices ([`SplitMix64::mix`](tutti_rt::SplitMix64::mix)),
    /// never one seed, which would add their noise coherently.
    pub fn dither_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// States that the file will hold `frames` frames: a count past the format's limit
    /// ([`Format::max_frames`](crate::Format::max_frames)) is refused with
    /// [`EncodeError::WavTooLarge`] before any file exists. It does not lower the limit on the
    /// writes ([`limit_frames`](Self::limit_frames) does).
    pub fn planned(mut self, frames: Samples) -> Self {
        self.planned = Some(frames);
        self
    }

    /// Lowers the frames a WAV file may hold to `frames`; it never raises the limit
    /// the format's 32-bit sizes set, and other formats are unbounded. For a caller that
    /// records into a space it knows.
    pub fn limit_frames(mut self, frames: Samples) -> Self {
        self.limit_frames = Some(frames);
        self
    }

    /// What is refused before any sink exists: a spec the format cannot write, and a
    /// [`planned`](Self::planned) length past its limit.
    fn check(&self) -> Result<()> {
        let spec = self.spec;
        spec.channels()?;
        match (self.planned, spec.format().max_frames(spec.layout())) {
            (Some(frames), Some(limit_frames)) if frames.get() as u64 > limit_frames => {
                Err(EncodeError::WavTooLarge { limit_frames })
            }
            _ => Ok(()),
        }
    }

    /// Starts the file in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Spec`] when the format cannot be written for the spec (a zero
    /// rate or width, a width past the format's limit, a topology of another
    /// width); [`EncodeError::WavTooLarge`] for a [`planned`](Self::planned) length past the
    /// limit; [`EncodeError::Encoding`] when the codec rejects it; [`EncodeError::Io`]
    /// when the sink fails on the header.
    pub fn wrap<W: Write + Seek>(self, sink: W) -> Result<Encoder<W>> {
        let spec = self.spec;
        self.check()?;
        let mut inner = Inner::open(spec, sink)?;
        if let Some(frames) = self.limit_frames {
            // The writers count frames in `u64`, their format's own width.
            inner.limit_frames(frames.get() as u64);
        }
        Ok(Encoder {
            inner,
            layout: spec.layout(),
            quantize: spec
                .format()
                .int_pcm()
                .map(|pcm| Quantize::new(pcm, spec.layout(), spec.rate(), self.seed)),
            first_error: None,
        })
    }

    /// Goes on to a file at `path`, whose [`FileOptions`] say what happens to what is there
    /// and where the bytes go.
    pub fn file(self, path: impl AsRef<Path>) -> FileOptions<'a> {
        FileOptions {
            options: self,
            path: path.as_ref().to_path_buf(),
            replace: false,
            in_place: false,
        }
    }
}

/// The options of a file at a path ([`EncoderOptions::file`]): an existing file is refused by
/// default ([`EncodeError::Exists`]) and replaced with [`replace`](Self::replace); the file is
/// staged beside its path and put in place when whole by default, and written at its path from
/// the start with [`in_place`](Self::in_place). Neither means anything for a sink the caller
/// owns, so neither is an option of [`EncoderOptions::wrap`].
#[derive(Debug, Clone)]
#[must_use = "options make nothing until `create`"]
pub struct FileOptions<'a> {
    options: EncoderOptions<'a>,
    path: PathBuf,
    replace: bool,
    in_place: bool,
}

impl FileOptions<'_> {
    /// Replaces a file that exists at the path instead of refusing it (on Windows too, and a
    /// symlink at the path is replaced rather than written through; the new file has the
    /// default permissions). In place, the file is truncated at once.
    pub fn replace(mut self) -> Self {
        self.replace = true;
        self
    }

    /// Writes the file at its path from the start, not beside it. A recording cut short by a
    /// fault or a crash is then there to keep, and a dropped encoder patches a WAV's or AIFF's
    /// header to the frames written.
    pub fn in_place(mut self) -> Self {
        self.in_place = true;
        self
    }

    /// Starts the file: staged beside its path, or in place.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Exists`] when something is at the path and [`replace`](Self::replace)
    /// was not called, with nothing written. Staged, that is a fast check, and the commit is
    /// the authority: a file that appears in between is refused at the commit
    /// ([`EncodeError::Commit`] of kind `AlreadyExists`), keeping the finished file. In place,
    /// the open is the authority. [`EncodeError::Io`] when the file cannot be created; the
    /// errors of [`EncoderOptions::wrap`] otherwise.
    pub fn create(self) -> Result<FileEncoder> {
        let Self {
            options,
            path,
            replace,
            in_place,
        } = self;
        let path = path.as_path();
        let spec = options.spec;
        // Before a file exists, so a spec the format cannot write, or a length it cannot
        // hold, touches nothing.
        options.check()?;
        let exists = || EncodeError::Exists {
            path: path.to_path_buf(),
        };
        let (sink, keep, at) = if in_place {
            let mut open = OpenOptions::new();
            open.write(true);
            if replace {
                open.create(true).truncate(true);
            } else {
                open.create_new(true);
            }
            let file = open.open(path).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    exists()
                } else {
                    e.into()
                }
            })?;
            (FileSink::InPlace(file), None, path.to_path_buf())
        } else {
            // A dangling symlink is something at the path too.
            if !replace && std::fs::symlink_metadata(path).is_ok() {
                return Err(exists());
            }
            let staged = Staged::new(path);
            let staged = if replace { staged } else { staged.create_new() };
            // Created before the header is written: a failure from here on drops
            // the file, which removes the temporary.
            let file = staged.open()?;
            let keep = file.keeper();
            let temp = file.temp().to_path_buf();
            (FileSink::Staged(file), Some(keep), temp)
        };
        let created = in_place && !replace;
        match options.wrap(BufWriter::new(sink)) {
            Ok(encoder) => Ok(FileEncoder {
                encoder,
                keep,
                path: at,
                spec: spec.clone(),
            }),
            Err(e) => {
                // An in-place file this call made holds nothing worth keeping.
                if created {
                    let _ = std::fs::remove_file(path);
                }
                Err(e)
            }
        }
    }
}

impl<W: Write + Seek> Encoder<W> {
    /// The values the quantize stage has clipped so far: a float or wider integer value past
    /// the file's range, held at its rail. `0` for a float file, which clips nothing.
    #[must_use]
    pub fn clipped(&self) -> u64 {
        self.quantize.as_ref().map_or(0, |q| q.clipped)
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

/// An encoder that writes a file to a path, built by [`FileOptions::create`].
///
/// - **Staged**, the default: the file is written to a temporary beside the path and put in
///   place by [`finish`](Self::finish) (synced and closed first), so the path holds either what
///   it held before or a complete new file, never a partial take. A take that fails a `write`,
///   or an encoder dropped before `finish`, removes the temporary and leaves the path alone; a
///   commit that fails keeps the file. The commit is the encoder's own, so a file is never put
///   in place while its sink is open, and never left uncommitted by a caller that kept the
///   sink.
/// - **In place** ([`FileOptions::in_place`]): the file is at its path from the start,
///   `finish` closes and flushes it, and [`FinalizedFile::commit`] only syncs. A failed take
///   and a dropped encoder keep the file, and a dropped one patches a WAV's or AIFF's header to
///   the frames written.
pub struct FileEncoder {
    encoder: Encoder<BufWriter<FileSink>>,
    /// Staged only: keeps the temporary through a drop.
    keep: Option<StagedKeep>,
    /// The temporary of a staged file, the target of one in place.
    path: PathBuf,
    spec: Spec,
}

/// Where a [`FileEncoder`]'s bytes go.
enum FileSink {
    Staged(StagedFile),
    InPlace(File),
}

impl Write for FileSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Staged(file) => file.write(buf),
            Self::InPlace(file) => file.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Staged(file) => file.flush(),
            Self::InPlace(file) => file.flush(),
        }
    }
}

impl Seek for FileSink {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Staged(file) => file.seek(to),
            Self::InPlace(file) => file.seek(to),
        }
    }
}

impl std::fmt::Debug for FileEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileEncoder")
            .field("encoder", &self.encoder)
            .finish()
    }
}

impl FileEncoder {
    pub(crate) fn layout(&self) -> ChannelLayout {
        self.spec.layout()
    }

    /// The values the file's quantize stage has clipped so far: [`Encoder::clipped`].
    #[must_use]
    pub fn clipped(&self) -> u64 {
        self.encoder.clipped()
    }

    /// Records a failure of the stages in front of the encoder as the file's first error, so
    /// that closing it (as [`finalize`](Self::finalize) does) reports it.
    pub(crate) fn fail(&mut self, error: &EncodeError) {
        self.encoder
            .first_error
            .get_or_insert_with(|| error.duplicate());
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
        let (file, path) = self.close_keeping()?;
        match file.file {
            FileSink::Staged(file) => file.commit().map_err(Kept::from),
            // Whole and at its path: a failed sync keeps it there.
            FileSink::InPlace(file) => file.sync_all().map_err(|e| Kept::Unfinished {
                source: EncodeError::Io(e),
                unfinished: Unfinished {
                    path,
                    header_describes_frames: true,
                },
            }),
        }
    }

    /// As [`finalize`](Self::finalize), for a file that must not be lost: when it cannot be
    /// closed whole (a failed `write`, the header, the close) it is **kept** beside its path
    /// as [`finish_keeping`](Self::finish_keeping) keeps it, and the error is
    /// [`Kept::Unfinished`], naming it. The caller owns a kept file, to move or delete.
    ///
    /// # Errors
    ///
    /// [`Kept::Unfinished`].
    pub fn finalize_keeping(self) -> std::result::Result<FinalizedFile, Kept> {
        self.close_keeping().map(|(file, _)| file)
    }

    /// The file closed, or kept unfinished; with the path it was written at.
    fn close_keeping(self) -> std::result::Result<(FinalizedFile, PathBuf), Kept> {
        let Self {
            encoder,
            keep,
            path,
            spec,
        } = self;
        // Set before the writer runs: it drops a staged file on a failure.
        if let Some(keep) = &keep {
            keep.keep();
        }
        match close(encoder) {
            Ok(file) => {
                if let Some(keep) = &keep {
                    keep.release();
                }
                Ok((file, path))
            }
            Err(source) => Err(Kept::Unfinished {
                source,
                unfinished: patched(path, &spec),
            }),
        }
    }
}

/// The encoder's file closed, flushed and its header lengths written from the
/// frames on disk where the format has them: the file at `path`, which the
/// caller has made outlive the encoder.
fn patched(path: PathBuf, spec: &Spec) -> Unfinished {
    let header_describes_frames = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .and_then(|mut file| patch_header(spec, &mut file))
        .is_ok();
    Unfinished {
        path,
        header_describes_frames,
    }
}

/// [`FileEncoder::finalize`]'s body: the encoder closed and its sink taken
/// back as a [`FinalizedFile`].
fn close(mut encoder: Encoder<BufWriter<FileSink>>) -> Result<FinalizedFile> {
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

impl Kept {
    /// Gives the file up: removes an unfinished file (a complete one a commit could not put
    /// in place is the caller's, and stays) and returns what failed, for a caller whose
    /// result must be whole or nothing.
    pub fn discard(self) -> EncodeError {
        match self {
            Self::Commit(e) => EncodeError::Commit(e),
            Self::Unfinished { source, unfinished } => {
                // Best effort: the failure being reported is the file's own.
                let _ = std::fs::remove_file(&unfinished.path);
                source
            }
        }
    }
}

/// The unfinished file [`FileEncoder::finish_keeping`] kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unfinished {
    /// Where the file is: beside the path it was meant for, or the path itself for an
    /// [`in_place`](FileOptions::in_place) file.
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

/// A [`FileEncoder`]'s closed file ([`FileEncoder::finalize`]): a staged file is not yet put
/// in place, and one in place is complete at its path.
#[must_use = "a staged file is removed when its finalized file is dropped; commit it"]
pub struct FinalizedFile {
    file: FileSink,
}

impl std::fmt::Debug for FinalizedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FinalizedFile").finish_non_exhaustive()
    }
}

impl FinalizedFile {
    /// Syncs the file and moves it onto its path (replacing what is there, if the encoder was
    /// made with [`FileOptions::replace`]). A file in place is already at its path, so
    /// this only syncs it.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Commit`] when the sync or the move fails (or the path exists and
    /// `replace` was not called): what was at the path is left alone and the complete
    /// file is kept at the error's `kept` path, the caller's to move or delete.
    /// [`EncodeError::Io`] when an in-place file cannot be synced.
    pub fn commit(self) -> Result<()> {
        match self.file {
            FileSink::Staged(file) => Ok(file.commit()?),
            FileSink::InPlace(file) => Ok(file.sync_all()?),
        }
    }
}

#[cfg(all(test, encode))]
mod tests;
