//! Writing a [`Wave`] as a file.

//! Writing a [`Wave`] as a file, or into memory, through an [`Encoding`].

use crate::chain::{Stages, options_of, spec_of};
use crate::{Chain, Encoding, Landed, Result, ToFile};
use std::io::Cursor;
use tutti_buffers::{Interleaved, InterleavedMut, Sample, SampleFormat, Wave, interleave};
use tutti_dsp::FftFloat;
use tutti_units::Samples;

/// Frames written to the chain at a time by [`write()`] and [`encode()`].
const WRITE_CHUNK: usize = 8192;

/// Writes `wave` as a file of `encoding` to `to`: folded onto the encoding's channels,
/// resampled, quantized and put in place, as a [`Chain`] does it for a stream.
///
/// - The file is written beside its path and renamed over it when whole, so a failure leaves
///   what was at the path as it was. An existing file is refused with
///   [`EncodeError::Exists`](crate::EncodeError::Exists), and nothing is written, unless
///   [`ToFile::replace`] says so; a file that appears before the commit is refused there
///   ([`EncodeError::Commit`](crate::EncodeError::Commit) of kind
///   [`AlreadyExists`](std::io::ErrorKind::AlreadyExists)), keeping the complete file at the
///   error's `kept` path, the caller's to move or delete.
/// - The wave's own rate and speakers are the source's: without
///   [`Encoding::channels`] the file has the wave's channels (the speaker mask of a WAV), and
///   without [`Encoding::resample`] its rate.
/// - An integer type quantizes as its [`IntPcm`](crate::IntPcm) says (dithered by default,
///   rounded to the nearest step with `no_dither`), from the wave's own format: a block that
///   needs no fold, resample or gain reaches the encoder unconverted. The stages run in `f64`
///   for a wave of `f64` and in `f32` for any other.
///
/// # Errors
///
/// [`EncodeError::Spec`](crate::EncodeError::Spec) for a wave the format cannot hold (no
/// channels, a rate no header states, a width past the format's limit),
/// [`EncodeError::WavTooLarge`](crate::EncodeError::WavTooLarge) for a WAV too long for its
/// 32-bit sizes, [`EncodeError::Fold`](crate::EncodeError::Fold) for channels the fold cannot
/// place (a channel is never dropped to make a width fit),
/// [`EncodeError::Resample`](crate::EncodeError::Resample), and the encoder's other errors.
///
/// # Examples
///
/// ```
/// # #[cfg(all(feature = "encode-wav", feature = "decode-wav"))] {
/// use tutti_buffers::{FloatWidth, Mono, Wave};
/// use tutti_codec::{Encoding, Format, Pcm, ToFile, read_wave, write};
/// use tutti_units::SampleRate;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let wave = Wave::from_mono(SampleRate::SR_48K, Mono::new(&[0.0f32, 0.25, 0.5, 0.75]));
/// let dir = tempfile::tempdir()?;
/// let path = dir.path().join("ramp.wav");
/// write(&wave, &Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32))), &ToFile::new(&path))?;
/// assert_eq!(read_wave::<f32>(&path)?, wave);
/// # Ok(())
/// # }
/// # }
/// ```
pub fn write<S: Sample>(wave: &Wave<S>, encoding: &Encoding, to: &ToFile) -> Result<Landed> {
    if S::FORMAT == SampleFormat::F64 {
        write_in::<f64, S>(wave, encoding, to)
    } else {
        write_in::<f32, S>(wave, encoding, to)
    }
}

fn write_in<F: FftFloat, S: Sample>(
    wave: &Wave<S>,
    encoding: &Encoding,
    to: &ToFile,
) -> Result<Landed> {
    let mut chain = Chain::<F>::new(
        encoding,
        to,
        &wave.channel_set(),
        wave.sample_rate(),
        wave.len(),
    )?;
    feed(wave, |block| chain.block(block))?;
    chain.finalize()?.commit()
}

/// Encodes `wave` into memory as a file of `encoding`: the bytes [`write()`] puts at a path,
/// through the same stages.
///
/// # Errors
///
/// As [`write()`], less the path's: nothing is refused for existing and nothing is committed.
///
/// # Examples
///
/// ```
/// # #[cfg(all(feature = "encode-wav", feature = "decode-wav"))] {
/// use tutti_buffers::{FloatWidth, Mono, Wave};
/// use tutti_codec::{Encoding, Format, Pcm, encode, read_wave};
/// use tutti_units::SampleRate;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let wave = Wave::from_mono(SampleRate::SR_48K, Mono::new(&[0.0f32, 0.25, 0.5, 0.75]));
/// let bytes = encode(&wave, &Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32))))?;
/// assert_eq!(read_wave::<f32>(bytes)?, wave);
/// # Ok(())
/// # }
/// # }
/// ```
pub fn encode<S: Sample>(wave: &Wave<S>, encoding: &Encoding) -> Result<Vec<u8>> {
    if S::FORMAT == SampleFormat::F64 {
        encode_in::<f64, S>(wave, encoding)
    } else {
        encode_in::<f32, S>(wave, encoding)
    }
}

fn encode_in<F: FftFloat, S: Sample>(wave: &Wave<S>, encoding: &Encoding) -> Result<Vec<u8>> {
    let source = wave.channel_set();
    let rate = wave.sample_rate();
    let mut stages = Stages::<F>::new(encoding, &source, rate)?;
    let spec = spec_of(encoding, &source, rate);
    let mut encoder =
        options_of(&spec, encoding, rate, wave.len()).wrap(Cursor::new(Vec::new()))?;
    feed(wave, |block| stages.block(block, &mut encoder))?;
    stages.flush(&mut encoder)?;
    Ok(encoder.finish()?.into_inner())
}

/// Feeds `wave` to `block` in chunks of its own sample format: the encoder
/// converts each sample once, from what the wave holds to what the file holds.
fn feed<S: Sample>(
    wave: &Wave<S>,
    mut block: impl FnMut(Interleaved<'_, S>) -> Result<()>,
) -> Result<()> {
    let layout = wave.layout();
    let mut rest = wave.planes();
    let mut chunk = vec![S::SILENCE; layout.interleaved_len(Samples::new(WRITE_CHUNK))];
    while !rest.is_empty() {
        let (head, tail) = rest.split_at(rest.frames().min(Samples::new(WRITE_CHUNK)));
        let len = layout.interleaved_len(head.frames());
        let mut out = InterleavedMut::new(&mut chunk[..len], layout)?;
        interleave(head, out.reborrow())?;
        block(out.as_interleaved())?;
        rest = tail;
    }
    Ok(())
}

#[cfg(all(test, feature = "encode-wav", decode))]
mod tests;
