#![doc = include_str!("../README.md")]
//! **The audio file codecs**: WAV, AIFF and FLAC out behind one
//! `Encoder`, over any `Write + Seek` sink; WAV, FLAC, MP3 and Ogg in as a
//! [`Wave`](tutti_buffers::Wave) or a streaming `FileIn`.
//!
//! ## What is here
//!
//! - `Format` names the container and carries only the settings it accepts:
//!   `Wav(Pcm)`, `Aiff(Pcm)` and `Flac { pcm: IntPcm, level: FlacLevel }`. FLAC at float is not
//!   expressible. Each variant exists only with its feature (`encode-wav`, `encode-aiff`,
//!   `encode-flac`), none on by default.
//! - `Spec` is what a file is written as: the format, the speakers (the topology,
//!   which is also the width) and the rate. `Spec::of_channels` takes a
//!   `ChannelSet`, and `ChannelSet::default_for` states the engine's order for a bare width.
//! - `EncoderOptions::new(&spec)` is how a file is begun: chained options (`dither_seed`,
//!   `planned`, `limit_frames`), then `wrap(sink)` for an `Encoder` over any `Write + Seek`, or
//!   `file(path)` for the `FileOptions` of a file (`replace`, `in_place`) and `create()` for a
//!   `FileEncoder`. An `Encoder` has a fallible `write(block)` and `finish()`. A `FileEncoder`
//!   is staged by default: it writes beside the path and moves the file into place on `finish`
//!   (which returns `()`: the file is synced and closed first, so nothing holds it open);
//!   dropping it first removes the file, and so does `finish` after a failed `write`. With
//!   `in_place` it writes the path from the start, and a recording cut short is kept. An
//!   existing file at the path is refused (`EncodeError::Exists`) unless `replace` is set.
//! - `EncoderSink` is the file sink, `FileIn`'s mirror: a `Sink` over a `FileEncoder`, for any
//!   `Format` the build can write. It keeps the first write error (later writes are skipped and
//!   return `SinkFailed`), and `finalize` is `FileEncoder::finish_keeping`, so a recording cut
//!   short is kept, with the first error in `Kept`.
//! - `write_wave(&wave, dest, format)` saves a `Wave` at its own rate and topology, to
//!   a path (staged, and refusing a file that is there) or into a borrowed writer.
//! - Decoding is plain functions and types of our own, no trait: `read_wave`
//!   decodes a path or bytes (any `impl Into<tutti_platform::Asset>`) into a `Wave` (a resident, planar sample buffer),
//!   `Wave::channel_set` and `FileIn::channel_set` say which speaker each channel feeds,
//!   `read_wave_metadata` reads a header (and the sample format the file holds
//!   its samples in), and `FileIn` streams a file as a `Source` with a sample-accurate `seek`. Both
//!   are generic over the sample format `S` they hand back: the file's own
//!   (`WaveMetadata::format`) is exact, so an app matches on it once at open. A failure is a `DecodeError`. `symphonia` does the
//!   work behind them, in `src/decode/` alone, and none of its types is in a
//!   signature or an error. `can_decode` and `decodable_extensions` say what this
//!   build reads. Each `decode-*` feature (`decode-wav`, `decode-flac`,
//!   `decode-aiff`, `decode-mp3`, `decode-ogg`) pulls exactly one container.
//! - `Pcm` is the sample type a file holds: `Int(IntPcm)`, a depth (`IntDepth`) with the dither
//!   and noise shaping that bring a block down to it, or `Float(tutti_buffers::FloatWidth)`, which has none. An
//!   encoder takes a block in any `tutti_buffers::Sample` format and converts it by that
//!   trait's rule, the one conversion every container shares, dithering an integer file where
//!   resolution drops (`tutti_dsp::Quantizer`).
//!
//! ## What this crate does not own
//!
//! The pipeline in front of the encoder: pulling blocks from a render or a live
//! source, and resampling. An encoder takes blocks already at the rate
//! and width the `Spec` states, and nothing here stops a caller from handing it
//! others. `tutti-export` is the offline pipeline and
//! `EncoderSink` the live one; both write through this crate, so a bounced
//! and a recorded file quantize and lay out a given sample identically.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
// With no format feature a `Format` has no value, so every body that follows
// one is unreachable by construction. That build compiles and writes nothing.
#![cfg_attr(
    not(encode),
    allow(unreachable_code, unused_mut, unused_variables, dead_code)
)]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

#[cfg(feature = "encode-aiff")]
mod aiff;
#[cfg(all(test, encode))]
mod contract;
mod decode;
mod decode_error;
mod encoder;
mod encoder_sink;
mod error;
#[cfg(feature = "encode-flac")]
mod flac;
mod format;
mod pcm;
mod spec;
#[cfg(test)]
mod test_sink;
#[cfg(feature = "encode-wav")]
mod wav;
#[cfg(any(feature = "encode-wav", decode))]
mod wave_mask;
#[cfg(encode)]
mod wave_write;
mod writer;

#[cfg(decode)]
pub use decode::{FileIn, WaveDecode, WaveMetadata, read_wave, read_wave_metadata};
pub use decode::{can_decode, decodable_extensions};
pub use decode_error::DecodeError;
pub use encoder::{
    Encoder, EncoderOptions, FileEncoder, FileOptions, FinalizedFile, Kept, Unfinished,
};
pub use encoder_sink::EncoderSink;
pub use error::{Codec, EncodeError, Result, SpecError};
pub use format::{FlacLevel, Format};
pub use pcm::{IntDepth, IntPcm, Pcm};
pub use spec::Spec;
#[cfg(encode)]
pub use wave_write::write_wave;
