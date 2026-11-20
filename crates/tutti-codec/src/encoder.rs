//! The [`Encoder`]: one type over every format.

use crate::error::{Error, Result};
#[cfg(encode)]
use crate::format::Format;
use crate::spec::Spec;
#[cfg(encode)]
use crate::writer::FormatWriter;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Interleaved, Sample};
use tutti_platform::{Staged, StagedFile};
use tutti_units::Samples;

/// Writes blocks of audio as one file of a [format](crate::Format): the
/// [writer](crate::FormatWriter) of whichever format the [`Spec`] names,
/// behind one type.
///
/// An encoder takes blocks already at the [`Spec`]'s rate and width: it does not
/// pull, resample or dither, and nothing here stops a caller from handing it
/// blocks that were not. The pipeline in front of it is the caller's. [`write`](Self::write) is fallible, so a full disk in the middle
/// of a take is an error at the block that hit it, not a short file that
/// reports success; [`finish`](Self::finish) closes the file exactly once.
///
/// The sink is any `Write + Seek + Send + 'static`: a file, or memory. (`Send`
/// and `'static` because Ogg Vorbis encodes on a thread that takes the sink;
/// the bound is the same whichever features are on, since bounds that followed
/// the features would be worse. In return an `Encoder` is `Send`, so a
/// recording can move it to its pump thread.) A file written beside its path and
/// moved into place when whole is a [`FileEncoder`].
///
/// ```
/// # #[cfg(feature = "encode-wav")] {
/// use std::io::Cursor;
/// use tutti_codec::{Encoder, Format, Pcm, Spec};
/// use tutti_buffers::ChannelLayout;
/// use tutti_units::SampleRate;
/// use tutti_buffers::{Interleaved, Sample};
///
/// let spec = Spec::of_width(Format::Wav(Pcm::Int16), ChannelLayout::STEREO, SampleRate::SR_48K);
/// let mut encoder = Encoder::create(&spec, Cursor::new(Vec::new()))?;
/// encoder.write(Interleaved::new(&[0.5, -0.5, 0.25, -0.25], ChannelLayout::STEREO)?)?;
/// let wav = encoder.finish()?.into_inner();
/// assert_eq!(&wav[..4], b"RIFF");
/// assert_eq!(wav.len(), 44 + 2 * 2 * 2); // a header, two stereo frames of 16 bits
/// # }
/// # Ok::<(), tutti_codec::Error>(())
/// ```
pub struct Encoder<W: Write + Seek + Send + 'static> {
    inner: Inner<W>,
    layout: ChannelLayout,
    /// The first failure of a `write`, kept for `finish` to report.
    first_error: Option<Error>,
}

impl<W: Write + Seek + Send + 'static> std::fmt::Debug for Encoder<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder")
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

enum Inner<W: Write + Seek + Send + 'static> {
    #[cfg(feature = "encode-wav")]
    Wav(crate::WavWriter<W>),
    #[cfg(feature = "encode-aiff")]
    Aiff(Box<crate::AiffWriter<W>>),
    #[cfg(feature = "encode-flac")]
    Flac(Box<crate::FlacWriter<W>>),
    #[cfg(feature = "encode-vorbis")]
    Vorbis(Box<crate::VorbisWriter<W>>),
    // No format feature: `Format` has no value, so neither has this.
    #[cfg(not(encode))]
    Never(std::convert::Infallible, std::marker::PhantomData<W>),
}

impl<W: Write + Seek + Send + 'static> Inner<W> {
    #[cfg_attr(not(encode), expect(unused_variables))]
    fn open(spec: &Spec, sink: W) -> Result<Self> {
        match spec.format() {
            #[cfg(feature = "encode-wav")]
            Format::Wav(pcm) => Ok(Self::Wav(crate::WavWriter::create(spec, pcm, sink)?)),
            #[cfg(feature = "encode-aiff")]
            Format::Aiff(pcm) => Ok(Self::Aiff(Box::new(crate::AiffWriter::create(
                spec, pcm, sink,
            )?))),
            #[cfg(feature = "encode-flac")]
            Format::Flac { depth, level } => Ok(Self::Flac(Box::new(crate::FlacWriter::create(
                spec, depth, level, sink,
            )?))),
            #[cfg(feature = "encode-vorbis")]
            Format::Vorbis { quality } => Ok(Self::Vorbis(Box::new(crate::VorbisWriter::create(
                spec, quality, sink,
            )?))),
        }
    }
}

/// [`Error::WavTooLarge`] when `frames` is past the spec's format's limit.
pub(crate) fn check_planned(spec: &Spec, frames: Samples) -> Result<()> {
    match spec.format().max_frames(spec.layout()) {
        Some(limit_frames) if frames.get() as u64 > limit_frames => {
            Err(Error::WavTooLarge { limit_frames })
        }
        _ => Ok(()),
    }
}

impl<W: Write + Seek + Send + 'static> Encoder<W> {
    /// Starts a file of `spec` in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// [`Error::Spec`] when the format cannot be written for `spec` (a zero
    /// rate or width, a width past the format's limit, a topology of another
    /// width); [`Error::Encoding`] when the codec rejects it; [`Error::Io`]
    /// when the sink fails on the header.
    pub fn create(spec: &Spec, sink: W) -> Result<Self> {
        spec.channels()?;
        Ok(Self {
            inner: Inner::open(spec, sink)?,
            layout: spec.layout(),
            first_error: None,
        })
    }

    /// Lowers the number of frames a WAV file may hold to `frames`; it never
    /// raises the limit the format's 32-bit sizes set, and other formats are
    /// unbounded. For a caller that records into a space it knows.
    #[must_use]
    #[cfg_attr(
        not(feature = "encode-wav"),
        allow(unused_mut, unused_variables, unreachable_code)
    )]
    pub fn limit_frames(mut self, frames: u64) -> Self {
        match &mut self.inner {
            #[cfg(feature = "encode-wav")]
            Inner::Wav(w) => w.limit_frames(frames),
            #[cfg(feature = "encode-aiff")]
            Inner::Aiff(_) => {}
            #[cfg(feature = "encode-flac")]
            Inner::Flac(_) => {}
            #[cfg(feature = "encode-vorbis")]
            Inner::Vorbis(_) => {}
            #[cfg(not(encode))]
            Inner::Never(never, _) => match *never {},
        }
        self
    }

    /// Writes one block of any [`Sample`] format; the file's width is
    /// reached by the rule in [`Sample`], and a block already in it is written
    /// as it is. A caller that dithers converts first.
    ///
    /// An empty block writes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::BlockWidth`] for a block of another width than the spec's;
    /// [`Error::Io`] when the sink fails (a full disk); [`Error::WavTooLarge`]
    /// when a WAV file reaches its size limit, after the frames that fit were
    /// written; [`Error::Encoding`] when the codec rejects the block. (Ogg
    /// Vorbis encodes on a thread of its own, so its failure may reach the
    /// next `write` or `finish` instead.) After an error the file is not a complete take: [`finish`](Self::finish) still
    /// closes what was written for a caller that keeps it, and
    /// [`abort`](Self::abort) discards it.
    pub fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        let written = self.write_block(block);
        if let Err(e) = &written {
            self.first_error.get_or_insert_with(|| e.clone());
        }
        written
    }

    fn write_block<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        if block.layout() != self.layout {
            return Err(Error::BlockWidth {
                expected: self.layout,
                got: block.layout(),
            });
        }
        match &mut self.inner {
            #[cfg(feature = "encode-wav")]
            Inner::Wav(w) => w.write(block),
            #[cfg(feature = "encode-aiff")]
            Inner::Aiff(w) => w.write(block),
            #[cfg(feature = "encode-flac")]
            Inner::Flac(w) => w.write(block),
            #[cfg(feature = "encode-vorbis")]
            Inner::Vorbis(w) => w.write(block),
            #[cfg(not(encode))]
            Inner::Never(never, _) => match *never {},
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
    /// [`Error::Io`] or [`Error::Encoding`] when closing fails.
    pub fn finish(self) -> Result<W> {
        Ok(self.finalize()?.commit())
    }

    /// Closes the file as [`finish`](Self::finish) does and returns it as a
    /// [`Finalized`].
    ///
    /// # Errors
    ///
    /// As [`finish`](Self::finish).
    pub fn finalize(self) -> Result<Finalized<W>> {
        let mut sink: W = match self.inner {
            #[cfg(feature = "encode-wav")]
            Inner::Wav(w) => w.finish()?,
            #[cfg(feature = "encode-aiff")]
            Inner::Aiff(w) => w.finish()?,
            #[cfg(feature = "encode-flac")]
            Inner::Flac(w) => w.finish()?,
            #[cfg(feature = "encode-vorbis")]
            Inner::Vorbis(w) => w.finish()?,
            #[cfg(not(encode))]
            Inner::Never(never, _) => match never {},
        };
        // A buffered sink reports a failed write only when it is flushed, and
        // a writer that does not flush would lose it to the sink's `Drop`.
        sink.flush()?;
        Ok(Finalized { sink })
    }

    /// Discards the encoder, dropping the sink with what was written.
    pub fn abort(self) {}
}

/// An encoder's closed sink ([`Encoder::finalize`]).
#[derive(Debug)]
pub struct Finalized<W> {
    sink: W,
}

impl<W> Finalized<W> {
    /// The sink, positioned at the end of the file.
    pub fn commit(self) -> W {
        self.sink
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
}

impl std::fmt::Debug for FileEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileEncoder")
            .field("encoder", &self.encoder)
            .finish()
    }
}

impl FileEncoder {
    /// Starts a file at `path`.
    ///
    /// # Errors
    ///
    /// As [`Encoder::create`], and [`Error::Io`] when the temporary cannot be
    /// created.
    pub fn create(spec: &Spec, path: impl AsRef<Path>) -> Result<Self> {
        Self::create_staged(spec, Staged::new(path.as_ref()))
    }

    /// Starts a file at `path` that will hold `frames` frames, refusing a
    /// count the format's file cannot hold before the temporary exists.
    ///
    /// For a caller that knows the length up front (an export), so it learns
    /// that a WAV is too long before it renders a frame, from the codec's own
    /// limit ([`crate::Format::max_frames`]) and its own error. It does not lower the
    /// limit on the writes.
    ///
    /// # Errors
    ///
    /// [`Error::WavTooLarge`] when `frames` is past [`crate::Format::max_frames`] for
    /// the spec's width; otherwise as [`create`](Self::create).
    pub fn create_planned(spec: &Spec, path: impl AsRef<Path>, frames: Samples) -> Result<Self> {
        check_planned(spec, frames)?;
        Self::create(spec, path)
    }

    /// Starts the file `staged` stands for: its temporary is created here and
    /// `staged` decides at the commit whether an existing target is replaced.
    pub(crate) fn create_staged(spec: &Spec, staged: Staged) -> Result<Self> {
        // Created before the header is written: a failure from here on drops
        // the file, which removes the temporary.
        let sink = BufWriter::new(staged.open()?);
        Ok(Self {
            encoder: Encoder::create(spec, sink)?,
        })
    }

    /// As [`Encoder::limit_frames`].
    #[must_use]
    pub fn limit_frames(mut self, frames: u64) -> Self {
        self.encoder = self.encoder.limit_frames(frames);
        self
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
    /// As [`finalize`](Self::finalize), and [`Error::Commit`] when the sync or
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
    /// [`Error::Io`] or [`Error::Encoding`] when closing fails, or the first
    /// failed `write`: a take must be whole, so the file is removed.
    pub fn finalize(mut self) -> Result<FinalizedFile> {
        let first_error = self.encoder.first_error.take();
        let sink = self.encoder.finalize()?.commit();
        if let Some(error) = first_error {
            return Err(error);
        }
        let file = sink.into_inner().map_err(|e| e.into_error())?;
        Ok(FinalizedFile { file })
    }

    /// Discards the file and leaves what was at its path alone. Dropping the
    /// encoder does the same.
    pub fn abort(self) {}
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
    /// [`Error::Commit`] when the sync or the move fails (or the path exists
    /// under `create_new`): what was at the path is left alone and the complete
    /// file is kept at the error's `kept` path, the caller's to move or delete.
    pub fn commit(self) -> Result<()> {
        Ok(self.file.commit()?)
    }
}

#[cfg(all(test, encode))]
mod tests;
