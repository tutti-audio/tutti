//! Writing a [`Wave`] as a file.

use crate::encoder::check_planned;
use crate::{Encoder, Format, Result, Spec};
use std::io::{Cursor, Seek, Write};
use tutti_buffers::{InterleavedMut, Sample, Wave, interleave};
use tutti_platform::Destination;
use tutti_units::Samples;

/// Frames written to the encoder at a time by [`write_wave`].
const WRITE_CHUNK: usize = 8192;

/// Writes `wave` as a file of `format` to `dest`, at its own rate and with its
/// own topology (the speaker mask of a WAV, the channel order of an Ogg Vorbis
/// file).
///
/// - A path is written beside itself and renamed over it when whole, as
///   [`Encoder::create_file`] does, so a failure leaves what was at the path as
///   it was.
/// - A writer (`&mut W` for any [`Write`]; a `Vec<u8>` is appended
///   to) is encoded into memory first and then handed over in one `write_all`,
///   so it needs no `Seek` and never holds a partial file from an encode error.
///   The file is held in memory once, whatever the writer.
///
/// Nothing is resampled, dithered or normalized: the samples go as they are, and
/// an integer width quantizes them with the format's own rounding.
///
/// # Errors
///
/// [`Error::Spec`](crate::Error::Spec) for a wave the format cannot hold (no
/// channels, a rate no header states, a width past the format's limit),
/// [`Error::WavTooLarge`](crate::Error::WavTooLarge) for a WAV too long for its
/// 32-bit sizes, [`Error::Io`](crate::Error::Io) when the writer fails, and the
/// encoder's other errors.
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
/// let wave = Wave::from_mono(SampleRate::SR_48K, Mono::new(&[0.0f32, 0.25, 0.5, 0.75]));
/// let mut bytes = Vec::new();
/// write_wave(&wave, &mut bytes, Format::Wav(Pcm::Float32))?;
/// assert_eq!(read_wave::<f32>(bytes)?, wave);
/// # Ok(())
/// # }
/// # }
/// ```
pub fn write_wave<'a, S: Sample>(
    wave: &Wave<S>,
    dest: impl Into<Destination<'a>>,
    format: Format,
) -> Result<()> {
    let spec = Spec::new(format, wave.topology().clone(), wave.sample_rate());
    match dest.into() {
        Destination::Path(path) => write_all(
            wave,
            Encoder::create_file_planned(&spec, &path, wave.len())?,
        )
        .map(drop),
        Destination::Writer(writer) => {
            check_planned(&spec, wave.len())?;
            // The Vorbis encoder's thread owns its sink, so a borrowed writer
            // cannot be the encoder's: the file is encoded into an owned cursor.
            let file = write_all(wave, Encoder::create(&spec, Cursor::new(Vec::new()))?)?;
            writer.write_all(file.get_ref())?;
            Ok(())
        }
    }
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
