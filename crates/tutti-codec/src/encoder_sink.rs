//! [`EncoderSink`]: a [`Sink`] over a [`FileEncoder`].

use std::marker::PhantomData;
use tutti_buffers::ChannelLayout;
use tutti_buffers::{Interleaved, Sample, Sink, SinkFailed};
use tutti_units::Samples;

use crate::encoder::{FileEncoder, Kept};
use crate::error::EncodeError;

/// The [`Sink`] a recording or an export writes into: a [`FileEncoder`] taking blocks of any
/// [`Sample`] format `S` (`f32` by default), converted to the file's sample type by the rule in
/// [`Sample`]. A file of an integer type is dithered as its [`IntPcm`](crate::IntPcm) says,
/// where a block has more resolution than the file's depth.
///
/// The write edge, the mirror of `FileIn` (with a `decode-*` feature): `FileIn` is the
/// codec's [`Source`](tutti_buffers::Source) and this is its [`Sink`]. A recorded and an
/// exported file go through the same encoder, so they agree sample-for-sample and
/// byte-for-byte for a [`Spec`](crate::Spec).
///
/// A block is whole frames at the encoder's width: a block of another width fails the sink
/// rather than rotating the file's channels (a caller feeding another width folds first,
/// through [`tutti_buffers::Fold`]). Writes do blocking file I/O: drive the sink from a thread
/// of its own, never from the audio thread.
///
/// # Failure
///
/// [`Sink::write`] returns only the marker [`SinkFailed`], so the detail of a failure has
/// nowhere to go mid-stream: the **first** write error is kept ([`take_error`](Self::take_error)),
/// that write and every later one returns [`SinkFailed`], and later writes are skipped.
/// [`finalize`](Sink::finalize) is [`FileEncoder::finish_keeping`]: the file is put in place
/// whole, and when it cannot be (a failed write, the close, the commit) it is **kept**, the
/// error [`Kept`] carrying the first failure. A caller that commits itself takes the encoder
/// back with [`into_encoder`](Self::into_encoder).
///
/// The sample rate is the [`Spec`](crate::Spec)'s: [`Source`](tutti_buffers::Source)
/// carries none, so build the spec at the source's rate or the file plays at the wrong speed.
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "encode-wav")] {
/// use tutti_codec::{EncoderOptions, EncoderSink, Format, IntDepth, IntPcm, Pcm, Spec};
/// use tutti_buffers::{Interleaved, Sink};
/// use tutti_units::SampleRate;
/// use tutti_buffers::{ChannelLayout, ChannelSet};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = tempfile::tempdir()?;
/// let pcm = Pcm::Int(IntPcm::new(IntDepth::Int24));
/// let spec = Spec::of_channels(Format::Wav(pcm), ChannelSet::default_for(ChannelLayout::STEREO), SampleRate::SR_48K);
/// let mut out = EncoderSink::<f32>::new(EncoderOptions::new(&spec).file(dir.path().join("tone.wav")).in_place().create()?);
/// let frames = [0.5, -0.5, 0.25, -0.25]; // two stereo frames
/// out.write(Interleaved::new(&frames, ChannelLayout::STEREO)?).expect("the write lands");
/// out.finalize()?;
/// # Ok(())
/// # }
/// # }
/// ```
pub struct EncoderSink<S: Sample = f32> {
    encoder: FileEncoder,
    /// Frames the encoder accepted, which is what the file holds.
    frames: Samples,
    /// The first write failure, kept for [`take_error`](Self::take_error) and, through the
    /// encoder, for [`finalize`](Sink::finalize).
    error: Option<EncodeError>,
    /// The format of the blocks it is written.
    format: PhantomData<fn(S)>,
}

impl<S: Sample> std::fmt::Debug for EncoderSink<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncoderSink")
            .field("layout", &self.encoder.layout())
            .field("frames", &self.frames)
            .field("failed", &self.error.is_some())
            .finish_non_exhaustive()
    }
}

impl<S: Sample> EncoderSink<S> {
    /// The sink over `encoder`, at its width.
    pub fn new(encoder: FileEncoder) -> Self {
        Self {
            encoder,
            frames: Samples::ZERO,
            error: None,
            format: PhantomData,
        }
    }

    /// The frames the encoder accepted: what the file holds.
    #[must_use]
    pub fn frames(&self) -> Samples {
        self.frames
    }

    /// The first write failure, with its detail; `None` when none failed or it was taken.
    /// The sink stays failed.
    pub fn take_error(&mut self) -> Option<EncodeError> {
        self.error.take()
    }

    /// The encoder, for a caller that closes and commits it itself.
    #[must_use]
    pub fn into_encoder(self) -> FileEncoder {
        self.encoder
    }
}

impl<S: Sample> Sink for EncoderSink<S> {
    type Sample = S;
    type Error = Kept;

    fn layout(&self) -> ChannelLayout {
        self.encoder.layout()
    }

    /// Writes frames **already at this sink's own width**. A failure stops the sink: this
    /// write and every later one returns [`SinkFailed`], and the detail of the first is
    /// reported once, from [`finalize`](Sink::finalize) or [`take_error`](EncoderSink::take_error).
    /// Swallowing it would leave a disk-full mid-take looking like a complete recording.
    fn write(&mut self, frames: Interleaved<'_, S>) -> Result<(), SinkFailed> {
        if self.error.is_some() {
            return Err(SinkFailed);
        }
        match self.encoder.write(frames) {
            Ok(()) => {
                self.frames += frames.frames();
                Ok(())
            }
            Err(e) => {
                self.error = Some(e);
                Err(SinkFailed)
            }
        }
    }

    fn finalize(self) -> Result<(), Kept> {
        self.encoder.finish_keeping()
    }
}

#[cfg(all(test, feature = "encode-wav"))]
mod tests;
