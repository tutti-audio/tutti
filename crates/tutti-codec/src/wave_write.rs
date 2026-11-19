//! Writing a [`Wave`] as a file.

use crate::encoder::check_planned;
use crate::{Encoder, Format, Result, Spec};
use std::io::{Seek, Write};
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
/// [`write_wave_to`] writes to a sink that is not a file.
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
    write_all(wave, Encoder::create_file_planned(&spec, path, wave.len())?).map(drop)
}

/// Writes `wave` as a file of `format` into `sink` (a `Cursor` over a `Vec`,
/// say), as [`write_wave`] writes one at a path: its own rate and topology,
/// nothing resampled, dithered or normalized. The sink is returned, positioned
/// at the end of the file.
///
/// Nothing is staged: a failure leaves the sink holding what was written
/// before it.
///
/// # Errors
///
/// As [`write_wave`], and [`Error::Io`](crate::Error::Io) when the sink fails.
///
/// # Examples
///
/// ```
/// # #[cfg(all(feature = "encode-wav", feature = "decode-wav"))] {
/// use std::io::Cursor;
/// use tutti_buffers::{Mono, Wave};
/// use tutti_codec::{Format, Pcm, read_wave_bytes, write_wave_to};
/// use tutti_units::SampleRate;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let wave = Wave::from_mono(SampleRate::SR_48K, Mono::new(&[0.0f32, 0.25, 0.5, -0.5]));
/// let bytes = write_wave_to(&wave, Cursor::new(Vec::new()), Format::Wav(Pcm::Float32))?;
/// assert_eq!(read_wave_bytes::<f32>(bytes.into_inner())?, wave);
/// # Ok(())
/// # }
/// # }
/// ```
pub fn write_wave_to<S: Sample, W: Write + Seek + Send + 'static>(
    wave: &Wave<S>,
    sink: W,
    format: Format,
) -> Result<W> {
    let spec = Spec::new(format, wave.topology().clone(), wave.sample_rate());
    check_planned(&spec, wave.len())?;
    write_all(wave, Encoder::create(&spec, sink)?)
}

/// Feeds `wave` to `encoder` in chunks and finishes it.
fn write_all<S: Sample, W: Write + Seek + Send + 'static>(
    wave: &Wave<S>,
    mut encoder: Encoder<W>,
) -> Result<W> {
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
    encoder.finish()
}

#[cfg(all(test, feature = "encode-wav", decode))]
mod tests;
