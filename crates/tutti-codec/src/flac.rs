//! FLAC (flacenc). Streams both ways: PCM in a block at a time, frames out to
//! the sink as they are encoded.
//!
//! flacenc's own entry point, `encode_with_fixed_block_size`, is a pull loop
//! over a `Source` that returns the whole compressed `Stream` at the end, and
//! it zero-pads the last block to the full block size. That gives the file
//! frames past the end of the audio (a decoder that does not trim to
//! STREAMINFO's sample count plays them), an MD5 over that padding, and, for
//! mono, padding that is stale samples from a reused buffer rather than zeros.
//!
//! So this drives flacenc's frame encoder, `encode_fixed_size_frame`, itself:
//! every full [`BLOCK_SIZE`] block is encoded on the caller's thread and written
//! before the call returns; the last block is encoded at its own length. The STREAMINFO is written as a placeholder first and
//! rewritten once the totals and the MD5 are known. Every full-size frame is
//! the one `encode_with_fixed_block_size` writes (the same function, block,
//! frame number and stream info).

use crate::error::{Codec, Error, Result};
use crate::format::FlacLevel;
use crate::pcm::IntDepth;
use crate::spec::Spec;
use crate::writer::FormatWriter;
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::config::Encoder as EncoderConfig;
use flacenc::constant::MIN_BLOCK_SIZE;
use flacenc::encode_fixed_size_frame;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Fill, FrameBuf};
use md5::{Digest, Md5};
use std::io::{Seek, SeekFrom, Write};
use tutti_buffers::{Interleaved, Sample};

/// Frames per FLAC block, every block but the last.
const BLOCK_SIZE: usize = 4096;

/// Writes a FLAC file through the [`FormatWriter`] contract.
pub struct FlacWriter<W: Write + Seek> {
    sink: W,
    frames: FrameWriter,
    depth: IntDepth,
    /// The block being filled, interleaved, at most [`BLOCK_SIZE`] frames.
    pcm: Vec<i32>,
    channels: usize,
}

impl<W: Write + Seek> std::fmt::Debug for FlacWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlacWriter").finish_non_exhaustive()
    }
}

impl<W: Write + Seek> FlacWriter<W> {
    /// Starts the file in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// As [`Encoder::create`](crate::Encoder::create).
    pub fn create(spec: &Spec, depth: IntDepth, level: FlacLevel, mut sink: W) -> Result<Self> {
        let rate = spec.header_rate()?.get() as usize;
        let channels = usize::from(spec.channels()?);
        let bits = usize::from(depth.bits());

        let verify = |c: EncoderConfig| {
            c.into_verified()
                .map_err(|e| Error::encoding(Codec::Flac, format!("invalid config: {e:?}")))
        };
        let configs = Configs {
            full: verify(encoder_config(level))?,
            short: verify(short_block_config(level))?,
        };
        let info = StreamInfo::new(rate, channels, bits)
            .map_err(|e| Error::encoding(Codec::Flac, format!("invalid stream: {e:?}")))?;

        // A placeholder of the final header's size (STREAMINFO is fixed
        // length), rewritten once the totals and the MD5 are known.
        sink.write_all(&header_bytes(&info)?)?;

        let frames = FrameWriter::start(configs, info.clone(), channels, bits)?;
        Ok(Self {
            sink,
            frames,
            depth,
            pcm: Vec::with_capacity(BLOCK_SIZE * channels),
            channels,
        })
    }
}

impl<W: Write + Seek> FormatWriter for FlacWriter<W> {
    type Sink = W;

    fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        let mut samples = block.samples();
        while !samples.is_empty() {
            let take = (BLOCK_SIZE * self.channels - self.pcm.len()).min(samples.len());
            let depth = self.depth;
            self.pcm
                .extend(samples[..take].iter().map(|&s| depth.quantize(s)));
            samples = &samples[take..];
            if self.pcm.len() == BLOCK_SIZE * self.channels {
                let written = self.frames.block(&self.pcm, &mut self.sink);
                // Cleared whatever came of it: a failed block is not encoded
                // again.
                self.pcm.clear();
                written?;
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Self::Sink> {
        if !self.pcm.is_empty() && !self.frames.failed {
            self.frames.block(&self.pcm, &mut self.sink)?;
        }
        let info = self.frames.finish()?;
        self.sink.flush()?;
        self.sink.seek(SeekFrom::Start(0))?;
        self.sink.write_all(&header_bytes(&info)?)?;
        self.sink.seek(SeekFrom::End(0))?;
        self.sink.flush()?;
        Ok(self.sink)
    }
}

/// `fLaC` and the STREAMINFO block, as flacenc writes a `Stream` with no
/// frames. Always 42 bytes, which is what lets the final one overwrite the
/// placeholder.
fn header_bytes(info: &StreamInfo) -> Result<Vec<u8>> {
    let mut sink = ByteSink::new();
    Stream::with_stream_info(info.clone())
        .write(&mut sink)
        .map_err(|e| Error::encoding(Codec::Flac, format!("failed to write header: {e:?}")))?;
    Ok(sink.into_inner())
}

/// The coding options, for a block of flacenc's minimum length or more, and
/// for a last block shorter than that.
struct Configs {
    full: Verified<EncoderConfig>,
    short: Verified<EncoderConfig>,
}

/// Encodes blocks on the caller's thread and writes their frames in order.
///
/// The caller hands it one interleaved block at a time ([`block`]); a full
/// one is [`BLOCK_SIZE`] frames, and a shorter one is the last. Each block is
/// encoded and written before `block` returns, so nothing accumulates.
///
/// [`block`]: Self::block
struct FrameWriter {
    configs: Configs,
    /// The buffer every block is encoded from.
    buf: FrameBuf,
    /// Frames written.
    written: usize,
    /// A frame failed to encode or to reach the sink: the frame is lost, so
    /// the stream cannot be completed.
    failed: bool,
    info: StreamInfo,
    md5: Md5,
    md5_bytes: Vec<u8>,
    bytes_per_sample: usize,
    channels: usize,
    sink: ByteSink,
}

impl FrameWriter {
    fn start(configs: Configs, info: StreamInfo, channels: usize, bits: usize) -> Result<Self> {
        let buf = FrameBuf::with_size(channels, BLOCK_SIZE)
            .map_err(|e| Error::encoding(Codec::Flac, format!("frame buffer: {e:?}")))?;
        Ok(Self {
            configs,
            buf,
            written: 0,
            failed: false,
            info,
            md5: Md5::new(),
            md5_bytes: Vec::new(),
            bytes_per_sample: bits.div_ceil(8),
            channels,
            sink: ByteSink::new(),
        })
    }

    /// Encode one interleaved block, [`BLOCK_SIZE`] frames or fewer for the
    /// last, and write its frame to `out`.
    fn block<W: Write>(&mut self, pcm: &[i32], out: &mut W) -> Result<()> {
        if self.failed {
            return Err(lost_frame());
        }
        let written = self.encode(pcm, out);
        if written.is_err() {
            self.failed = true;
        }
        written
    }

    fn encode<W: Write>(&mut self, pcm: &[i32], out: &mut W) -> Result<()> {
        let frames = pcm.len() / self.channels;
        // The MD5 is over the samples as the decoder will return them: the
        // first `bytes_per_sample` bytes of each, little-endian, in
        // interleaved order (and, unlike flacenc's, over no padding).
        self.md5_bytes.clear();
        for v in pcm {
            self.md5_bytes
                .extend_from_slice(&v.to_le_bytes()[..self.bytes_per_sample]);
        }
        self.md5.update(&self.md5_bytes);

        // The last block is encoded at its own length rather than padded.
        // `resize` is unchecked, which is what lets that length be under
        // flacenc's 64-frame minimum: FLAC allows any length for the last
        // block.
        self.buf.resize(frames);
        self.buf
            .fill_interleaved(pcm)
            .map_err(|e| Error::encoding(Codec::Flac, format!("frame buffer: {e:?}")))?;
        let config = if frames < MIN_BLOCK_SIZE {
            &self.configs.short
        } else {
            &self.configs.full
        };
        let mut frame = encode_fixed_size_frame(config, &self.buf, self.written, &self.info)
            .map_err(|e| Error::encoding(Codec::Flac, format!("encoding failed: {e:?}")))?;
        frame.precompute_bitstream();
        self.info.update_frame_info(&frame);
        self.sink.clear();
        frame
            .write(&mut self.sink)
            .map_err(|e| Error::encoding(Codec::Flac, format!("failed to write frame: {e:?}")))?;
        out.write_all(self.sink.as_slice())?;
        self.written += 1;
        Ok(())
    }

    /// Return the final STREAMINFO.
    fn finish(mut self) -> Result<StreamInfo> {
        if self.failed {
            return Err(lost_frame());
        }
        // A fixed-blocksize stream's STREAMINFO states the block size every
        // block but the last has, whatever the last one's; an empty stream
        // states it too, rather than `update_frame_info`'s unset 65535 and 0.
        self.info
            .set_block_sizes(BLOCK_SIZE, BLOCK_SIZE)
            .map_err(|e| Error::encoding(Codec::Flac, format!("block size: {e:?}")))?;
        if self.written == 0 {
            // Zero is "unknown"; unset, the two would read u32::MAX and 0.
            self.info
                .set_frame_sizes(0, 0)
                .map_err(|e| Error::encoding(Codec::Flac, format!("frame size: {e:?}")))?;
        }
        // `update_frame_info` has summed every block's length, the last at its
        // own, so the sample count is already exact.
        self.info.set_md5_digest(&self.md5.finalize().into());
        Ok(self.info)
    }
}

fn lost_frame() -> Error {
    Error::encoding(
        Codec::Flac,
        "an earlier frame failed, so the stream cannot be completed",
    )
}

/// flacenc's coding options for a level, modelled loosely on libFLAC's
/// presets. [`Fastest`](FlacLevel::Fastest) codes with fixed predictors only;
/// the next adds LPC at an exact order (flacenc's `lpc_order` is the order used,
/// not a searched maximum): 6, 8, then 12 at [`Best`](FlacLevel::Best). Every
/// level but the first tries mid/side and left/right-side stereo.
///
/// Where this departs from libFLAC: its level 3 has no stereo decorrelation,
/// and its levels 0 to 2 use 1152-sample blocks, while the block size here is
/// [`BLOCK_SIZE`] at every level. Every level is lossless.
fn encoder_config(level: FlacLevel) -> EncoderConfig {
    let mut config = EncoderConfig::default();
    let side = level != FlacLevel::Fastest;
    config.stereo_coding.use_leftside = side;
    config.stereo_coding.use_rightside = side;
    config.stereo_coding.use_midside = side;
    config.subframe_coding.use_lpc = !matches!(level, FlacLevel::Fastest | FlacLevel::Fast);
    config.subframe_coding.qlpc.lpc_order = match level {
        FlacLevel::Fastest | FlacLevel::Fast | FlacLevel::Medium => 6,
        FlacLevel::Standard => 8,
        FlacLevel::Best => 12,
    };
    config
}

/// The coding options for a last block under flacenc's [`MIN_BLOCK_SIZE`]
/// (64 frames): `level`'s, with only constant and verbatim subframes.
///
/// flacenc's predictive coders assume at least that many frames (its Rice
/// partition search underflows below it), but FLAC allows any length for the
/// last block, and a verbatim subframe of under 64 samples costs at most a
/// few hundred bytes once per file.
fn short_block_config(level: FlacLevel) -> EncoderConfig {
    let mut config = encoder_config(level);
    config.subframe_coding.use_fixed = false;
    config.subframe_coding.use_lpc = false;
    config
}

#[cfg(test)]
mod tests;
