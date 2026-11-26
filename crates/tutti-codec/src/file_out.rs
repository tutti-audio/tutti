//! [`FileOut`]: a [`Sink`] that writes a file of any [`Format`](crate::Format).

use std::fs::File;
use std::io::BufWriter;

use std::marker::PhantomData;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Interleaved, Sample, Sink, SinkFailed};
use tutti_units::SampleRate;

use crate::encoder::Encoder;
use crate::error::Result;
use crate::spec::Spec;

/// A file sink: the [`Sink`] a recording writes into, as a file of any
/// [`Format`](crate::Format) this build can write, taking blocks of any
/// [`Sample`] format `S` (`f32` by default) and converting them to the file's
/// sample type by the rule in [`Sample`]. A file of an integer type is dithered as its
/// [`IntPcm`](crate::IntPcm) says, where a block has more resolution than the file's depth.
///
/// The write edge, the mirror of `FileIn` (with a `decode-*` feature): `FileIn` is the
/// codec's [`Source`](tutti_buffers::Source) and this is its
/// [`Sink`]. It is an [`Encoder`] behind that contract, the same writer
/// an export bounces through, so a recorded and an exported file agree
/// sample-for-sample and byte-for-byte for a [`Spec`].
///
/// A capture into an integer file is dithered by default (triangular, one LSB wide). A capture
/// whose floats already sit on the file's grid (a 24-bit converter into a 24-bit file) gains
/// about one LSB of noise it did not need and loses bit-transparency; `IntPcm::new(depth)`
/// with [`no_dither`](crate::IntPcm::no_dither) keeps it bit-exact.
///
/// It writes incrementally through a buffered file writer, so a take of any
/// length is never held in memory. Frames are whole: a trailing part of a
/// frame in a [`write`](Sink::write) is ignored, so a caller's odd slice
/// cannot rotate the file's channels.
///
/// Writes do blocking file I/O: drive the sink from a thread of its own
/// (a session take's pump is one), never from the audio thread.
///
/// # Failure
///
/// [`Sink::write`] returns only the marker [`SinkFailed`], so the detail of a
/// failure has nowhere to go mid-stream: the **first** write error (a full
/// disk, or a WAV reaching its 4 GiB limit with
/// [`EncodeError::WavTooLarge`](crate::EncodeError::WavTooLarge)) is kept, that write and
/// every later one returns [`SinkFailed`], and later writes are skipped.
/// [`finalize`](Sink::finalize) must run for the file to be
/// readable: it closes what was written even after a failure (a recording
/// keeps its readable prefix) and returns the first error, else the result of
/// closing.
///
/// The file is written in place, not staged beside its path like
/// [`FileEncoder::create`](crate::FileEncoder::create): a recording cut short by a fault or a crash is
/// worth keeping, which a staged file removes. For WAV, the writer's drop patches the header, so a
/// take dropped without finalizing (a pump thread that panicked) still
/// opens.
///
/// The sample rate is the [`Spec`]'s: [`Source`](tutti_buffers::Source)
/// carries none, so build the spec at the source's rate or the file plays at
/// the wrong speed.
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "encode-wav")] {
/// use tutti_codec::{FileOut, Format, IntDepth, IntPcm, Pcm, Spec};
/// use tutti_buffers::{Interleaved, Sink};
/// use tutti_units::SampleRate;
/// use tutti_buffers::{ChannelLayout, ChannelSet};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = tempfile::tempdir()?;
/// let pcm = Pcm::Int(IntPcm::new(IntDepth::Int24));
/// let spec = Spec::of_channels(Format::Wav(pcm), ChannelSet::default_for(ChannelLayout::STEREO), SampleRate::SR_48K);
/// let file = std::fs::File::create(dir.path().join("tone.wav"))?;
/// let mut out = FileOut::<f32>::new(&spec, file)?;
/// let frames = [0.5, -0.5, 0.25, -0.25]; // two stereo frames
/// out.write(Interleaved::new(&frames, ChannelLayout::STEREO)?).expect("the write lands");
/// out.finalize()?;
/// # Ok(())
/// # }
/// # }
/// ```
pub struct FileOut<S: Sample = f32> {
    encoder: Encoder<BufWriter<File>>,
    spec: Spec,
    /// The first write failure, kept until [`finalize`](Sink::finalize)
    /// can report it. Keeping the *first* is what makes the report meaningful:
    /// later failures are usually the same cause repeated.
    first_error: Option<std::io::Error>,
    /// The format of the blocks it is written.
    format: PhantomData<fn(S)>,
}

impl<S: Sample> std::fmt::Debug for FileOut<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileOut")
            .field("layout", &self.spec.layout())
            .field("sample_rate", &self.spec.rate())
            .field("format", &self.spec.format())
            .field("failed", &self.first_error.is_some())
            .finish_non_exhaustive()
    }
}

impl<S: Sample> FileOut<S> {
    /// Writes the header of `spec` into `file` and returns the sink over it.
    /// The caller opens the file, and so chooses what happens to one already
    /// at its path: [`File::create`] replaces it, and
    /// [`OpenOptions::create_new`](std::fs::OpenOptions::create_new) refuses
    /// it. The file is written in place, so a recording cut short keeps what
    /// was written.
    ///
    /// # Errors
    ///
    /// [`EncodeError::Io`](crate::EncodeError::Io) from writing the header (a full disk),
    /// and the codec's other errors when the [`Spec`] cannot be written.
    pub fn new(spec: &Spec, file: File) -> Result<Self> {
        let sink = BufWriter::new(file);
        Ok(Self {
            encoder: Encoder::create(spec, sink)?,
            spec: spec.clone(),
            first_error: None,
            format: PhantomData,
        })
    }

    /// The sample rate the header declares.
    ///
    /// Compare it with the source's before pumping: 48 kHz frames fed to a
    /// sink that declared 8 kHz make a valid file that plays six times too
    /// slow, and nothing downstream can detect it.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        self.spec.rate()
    }

    /// Lowers the number of frames a WAV file may hold to `frames`
    /// ([`Encoder::limit_frames`]): for a caller that records into a space it
    /// knows, and for a test that reaches the limit without writing 4 GiB.
    /// The write that would cross it fails the sink like any other write
    /// failure.
    #[must_use]
    pub fn limit_frames(self, frames: u64) -> Self {
        Self {
            encoder: self.encoder.limit_frames(frames),
            ..self
        }
    }
}

impl<S: Sample> Sink for FileOut<S> {
    type Sample = S;
    type Error = std::io::Error;

    fn layout(&self) -> ChannelLayout {
        self.spec.layout()
    }

    /// Writes frames **already at this sink's own width**. A view of another
    /// width fails the sink rather than rotating the file's channels. A caller
    /// feeding another width folds first, through [`tutti_buffers::Fold`].
    ///
    /// A failure stops the sink: this write and every later one returns
    /// [`SinkFailed`], and the detail of the first is reported once, from
    /// [`finalize`](Sink::finalize). Swallowing it would leave a disk-full
    /// mid-take looking like a complete recording.
    fn write(&mut self, frames: Interleaved<'_, S>) -> std::result::Result<(), SinkFailed> {
        if self.first_error.is_some() {
            return Err(SinkFailed); // already failed; the file is being abandoned
        }
        let written = if frames.layout() == self.spec.layout() {
            self.encoder.write(frames)
        } else {
            Err(crate::EncodeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "a {:?} view written to a {:?} file",
                    frames.layout(),
                    self.spec.layout()
                ),
            )))
        };
        match written {
            Ok(()) => Ok(()),
            Err(e) => {
                self.first_error = Some(e.into());
                Err(SinkFailed)
            }
        }
    }

    fn finalize(self) -> std::result::Result<(), Self::Error> {
        // A write that failed mid-stream is reported ahead of the finish
        // itself: the header may well back-patch cleanly over a truncated
        // recording, so finishing `Ok` would hide the real fault. The encoder
        // is still finished, so the bytes that did land are readable.
        let finished = self.encoder.finish();
        match self.first_error {
            Some(error) => Err(error),
            None => finished.map(drop).map_err(Into::into),
        }
    }
}

#[cfg(all(test, feature = "encode-wav"))]
mod tests;
