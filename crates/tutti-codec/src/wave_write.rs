//! Writing a [`Wave`] as a file.

use crate::encoder::check_planned;
use crate::{Encoder, FileEncoder, Format, Result, Spec};
use std::io::{Cursor, Seek, Write};
use tutti_buffers::{Interleaved, InterleavedMut, Sample, Wave, interleave};
use tutti_platform::Destination;
use tutti_units::Samples;

/// Frames written to the encoder at a time by [`write_wave`].
const WRITE_CHUNK: usize = 8192;

/// Writes `wave` as a file of `format` to `dest`, at its own rate and with its
/// own topology (the speaker mask of a WAV).
///
/// - A path is written beside itself and renamed over it when whole, as
///   [`FileEncoder::create`] does, so a failure leaves what was at the path as
///   it was. An existing file is replaced unless the destination is
///   [`create_new`](Destination::create_new), which refuses it with an
///   [`EncodeError::Commit`](crate::EncodeError::Commit) whose source kind is
///   [`AlreadyExists`](std::io::ErrorKind::AlreadyExists). A failed commit
///   keeps the complete file at the error's `kept` path, the caller's to move
///   or delete.
/// - A writer (`&mut W` for any [`Write`]; a `Vec<u8>` is appended
///   to) is encoded into memory first and then handed over in one `write_all`,
///   so it needs no `Seek` and never holds a partial file from an encode error.
///   The file is held in memory once, whatever the writer.
///
/// Nothing is resampled or normalized: the samples go as they are. An integer type
/// quantizes them as its [`IntPcm`](crate::IntPcm) says (dithered by default, rounded to the
/// nearest step with `no_dither`).
///
/// # Errors
///
/// [`EncodeError::Spec`](crate::EncodeError::Spec) for a wave the format cannot hold (no
/// channels, a rate no header states, a width past the format's limit),
/// [`EncodeError::WavTooLarge`](crate::EncodeError::WavTooLarge) for a WAV too long for its
/// 32-bit sizes, [`EncodeError::Io`](crate::EncodeError::Io) when the writer fails, and the
/// encoder's other errors.
///
/// # Examples
///
/// ```
/// # #[cfg(all(feature = "encode-wav", feature = "decode-wav"))] {
/// use tutti_buffers::Mono;
/// use tutti_codec::{FloatWidth, Format, Pcm, read_wave, write_wave};
/// use tutti_units::SampleRate;
/// use tutti_buffers::Wave;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let wave = Wave::from_mono(SampleRate::SR_48K, Mono::new(&[0.0f32, 0.25, 0.5, 0.75]));
/// let mut bytes = Vec::new();
/// write_wave(&wave, &mut bytes, Format::Wav(Pcm::Float(FloatWidth::F32)))?;
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
    let spec = Spec::of_channels(format, wave.channel_set(), wave.sample_rate());
    match dest.into() {
        Destination::Path(staged) => {
            check_planned(&spec, wave.len())?;
            write_file(wave, FileEncoder::create(&spec, staged)?)
        }
        Destination::Writer(writer) => {
            check_planned(&spec, wave.len())?;
            // A borrowed writer need not seek: the file is encoded into an owned
            // cursor and handed over whole.
            let file = write_all(wave, Encoder::create(&spec, Cursor::new(Vec::new()))?)?;
            writer.write_all(file.get_ref())?;
            Ok(())
        }
    }
}

/// Feeds `wave` to `write` in chunks of its own sample format: the encoder
/// converts each sample once, from what the wave holds to what the file holds.
fn feed<S: Sample>(
    wave: &Wave<S>,
    mut write: impl FnMut(Interleaved<'_, S>) -> Result<()>,
) -> Result<()> {
    let layout = wave.layout();
    let mut rest = wave.planes();
    let mut chunk = vec![S::SILENCE; layout.interleaved_len(Samples::new(WRITE_CHUNK))];
    while !rest.is_empty() {
        let (head, tail) = rest.split_at(rest.frames().min(Samples::new(WRITE_CHUNK)));
        let len = layout.interleaved_len(head.frames());
        let mut out = InterleavedMut::new(&mut chunk[..len], layout)?;
        interleave(head, out.reborrow())?;
        write(out.as_interleaved())?;
        rest = tail;
    }
    Ok(())
}

/// Feeds `wave` to a plain-sink `encoder` and finishes it.
fn write_all<S: Sample, W: Write + Seek>(wave: &Wave<S>, mut encoder: Encoder<W>) -> Result<W> {
    feed(wave, |block| encoder.write(block))?;
    encoder.finish()
}

/// Feeds `wave` to a file `encoder` and finishes it, putting the file in place.
fn write_file<S: Sample>(wave: &Wave<S>, mut encoder: FileEncoder) -> Result<()> {
    feed(wave, |block| encoder.write(block))?;
    encoder.finish()
}

#[cfg(all(test, feature = "encode-wav", decode))]
mod tests;
