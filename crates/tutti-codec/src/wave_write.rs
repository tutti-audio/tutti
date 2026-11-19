//! Writing a [`Wave`] as a file.

use crate::{Encoder, Format, Result, Spec};
use std::path::Path;
use tutti_buffers::{InterleavedMut, Sample, Wave, interleave};
use tutti_units::Samples;

/// Frames written to the encoder at a time by [`write_wave`].
const WRITE_CHUNK: usize = 8192;

/// Writes `wave` as a file of `format` at `path`, at its own rate and with its
/// own topology (the speaker mask of a WAV, the channel order of an Ogg Vorbis
/// file).
///
/// The file is written beside `path` and renamed over it when it is whole, as
/// [`Encoder::create_file`] does, so a failure leaves what was at `path` as it
/// was. Nothing is resampled, dithered or normalized: the samples go as they
/// are, and an integer width quantizes them with the format's own rounding.
///
/// # Errors
///
/// [`Error::Spec`](crate::Error::Spec) for a wave the format cannot hold (no
/// channels, a rate no header states, a width past the format's limit),
/// [`Error::WavTooLarge`](crate::Error::WavTooLarge) for a WAV too long for its
/// 32-bit sizes, and the encoder's other errors.
///
/// # Examples
///
/// ```
/// # #[cfg(all(feature = "encode-wav", feature = "decode-wav"))] {
/// use tutti_buffers::Mono;
/// use tutti_codec::{Format, Pcm, read_wave, write_wave};
/// use tutti_units::SampleRate;
/// use tutti_buffers::Wave;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = tempfile::tempdir()?;
/// let path = dir.path().join("ramp.wav");
/// let wave = Wave::from_mono(SampleRate::SR_48K, Mono::new(&[0.0f32, 0.25, 0.5, 0.75]));
/// write_wave(&wave, &path, Format::Wav(Pcm::Float32))?;
/// assert_eq!(read_wave::<f32>(&path)?, wave);
/// # Ok(())
/// # }
/// # }
/// ```
pub fn write_wave<S: Sample>(wave: &Wave<S>, path: impl AsRef<Path>, format: Format) -> Result<()> {
    let spec = Spec::new(format, wave.topology().clone(), wave.sample_rate());
    let mut encoder = Encoder::create_file_planned(&spec, path, wave.len())?;
    let layout = wave.layout();
    let mut rest = wave.planes();
    let mut chunk = vec![0.0f32; layout.interleaved_len(Samples::new(WRITE_CHUNK))];
    while !rest.is_empty() {
        let (head, tail) = rest.split_at(rest.frames().min(Samples::new(WRITE_CHUNK)));
        let len = layout.interleaved_len(head.frames());
        let mut out = InterleavedMut::new(&mut chunk[..len], layout)?;
        interleave(head, out.reborrow())?;
        encoder.write(out.as_interleaved())?;
        rest = tail;
    }
    encoder.finish().map(drop)
}

#[cfg(all(test, feature = "encode-wav", decode))]
mod tests;
