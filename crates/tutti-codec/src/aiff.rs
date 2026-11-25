//! AIFF (`aifc`). Streams; `finish` back-patches the COMM/SSND sizes.
//!
//! AIFF does not require a total size up front: `aifc` takes samples
//! incrementally and patches the sizes at `finalize`, the same way a WAV
//! header is patched. It also owns the 80-bit IEEE 754 extended encoding of the
//! sample-rate field, which is the part worth not writing by hand.
//!
//! `SampleFormat::{I16, I24, F32, F64}` line up exactly with the four [`Pcm`]
//! widths, so there is no conversion policy here beyond the rule in
//! [`Sample`] every format shares.

use crate::error::{Codec, EncodeError, Result, SpecError};
use crate::pcm::{FloatWidth, IntDepth, Pcm};
use crate::spec::Spec;
use crate::writer::FormatWriter;
use aifc::{AifcWriteInfo, AifcWriter, FileFormat, SampleFormat};
use std::io::{Read, Seek, SeekFrom, Write};
use tutti_buffers::{I24, Interleaved, Sample};

/// Writes an AIFF (or AIFF-C) file through the [`FormatWriter`] contract.
pub(crate) struct AiffWriter<W: Write + Seek> {
    writer: AifcWriter<W>,
    pcm: Pcm,
    ints: Vec<i32>,
    shorts: Vec<i16>,
    floats: Vec<f32>,
    doubles: Vec<f64>,
}

impl<W: Write + Seek> std::fmt::Debug for AiffWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiffWriter").finish_non_exhaustive()
    }
}

impl<W: Write + Seek> AiffWriter<W> {
    /// Starts the file in `sink`, writing its header.
    ///
    /// # Errors
    ///
    /// As [`Encoder::create`](crate::Encoder::create).
    pub(crate) fn create(spec: &Spec, pcm: Pcm, sink: W) -> Result<Self> {
        let sample_rate = spec.header_hz()?;
        let channels = spec.channels()?;
        let info = AifcWriteInfo {
            // Float samples are an AIFF-C extension; plain AIFF is integer-only,
            // so the container follows the width rather than the other way
            // round.
            file_format: match pcm {
                Pcm::Float(_) => FileFormat::Aifc,
                Pcm::Int(_) => FileFormat::Aiff,
            },
            channels: i16::try_from(channels).map_err(|_| SpecError::TooManyChannels {
                channels,
                max: i16::MAX.unsigned_abs(),
            })?,
            // The extended float the header states, written exactly.
            sample_rate,
            sample_format: match pcm {
                Pcm::Int(pcm) => match pcm.depth() {
                    IntDepth::Int16 => SampleFormat::I16,
                    IntDepth::Int24 => SampleFormat::I24,
                },
                Pcm::Float(FloatWidth::F32) => SampleFormat::F32,
                Pcm::Float(FloatWidth::F64) => SampleFormat::F64,
            },
        };
        let writer = AifcWriter::new(sink, &info)
            .map_err(|e| EncodeError::encoding(Codec::Aiff, format!("{e:?}")))?;
        Ok(Self {
            writer,
            pcm,
            ints: Vec::new(),
            shorts: Vec::new(),
            floats: Vec::new(),
            doubles: Vec::new(),
        })
    }
}

impl<W: Write + Seek> FormatWriter for AiffWriter<W> {
    type Sink = W;

    fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()> {
        // `aifc` takes an interleaved sample run, which is the shape the block
        // already has.
        let buf = block.samples();
        match self.pcm {
            Pcm::Int(pcm) => match pcm.depth() {
                IntDepth::Int16 => {
                    self.shorts.clear();
                    self.shorts
                        .extend(buf.iter().map(|&s| s.convert_to::<i16>()));
                    self.writer.write_samples_i16(&self.shorts)
                }
                IntDepth::Int24 => {
                    self.ints.clear();
                    self.ints
                        .extend(buf.iter().map(|&s| s.convert_to::<I24>().get()));
                    self.writer.write_samples_i24(&self.ints)
                }
            },
            Pcm::Float(FloatWidth::F32) => {
                self.floats.clear();
                self.floats
                    .extend(buf.iter().map(|&s| s.convert_to::<f32>()));
                self.writer.write_samples_f32(&self.floats)
            }
            Pcm::Float(FloatWidth::F64) => {
                self.doubles.clear();
                self.doubles
                    .extend(buf.iter().map(|&s| s.convert_to::<f64>()));
                self.writer.write_samples_f64(&self.doubles)
            }
        }
        .map_err(|e| EncodeError::encoding(Codec::Aiff, format!("write failed: {e:?}")))
    }

    fn finish(mut self) -> Result<Self::Sink> {
        self.writer.finalize().map_err(|e| {
            EncodeError::encoding(Codec::Aiff, format!("finalization failed: {e:?}"))
        })?;
        Ok(self.writer.into_inner())
    }
}

/// Makes the unfinished AIFF `file`, whose samples are `block_align` bytes a
/// frame, a readable one: the chunks are walked from the start, and the COMM
/// frame count, the SSND size and the FORM size are written for the whole
/// frames the file holds on disk; a torn last frame is cut off.
///
/// The sizes `aifc` writes at `finalize` are its own; this writes the same
/// fields from the file's length.
pub(crate) fn patch_unfinished(file: &mut std::fs::File, block_align: u64) -> std::io::Result<()> {
    let len = file.metadata()?.len();
    let (mut comm, mut ssnd) = (None, None);
    let mut at = 12u64;
    while at + 8 <= len && ssnd.is_none() {
        let mut head = [0u8; 8];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut head)?;
        let size = u64::from(u32::from_be_bytes([head[4], head[5], head[6], head[7]]));
        match &head[..4] {
            b"COMM" => comm = Some(at + 8),
            b"SSND" => ssnd = Some(at),
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    let (Some(comm), Some(ssnd)) = (comm, ssnd) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no COMM and SSND chunks to patch",
        ));
    };
    // The SSND chunk's offset and block size fields precede its samples, and
    // `offset` bytes more come before the first frame.
    let mut offset = [0u8; 4];
    file.seek(SeekFrom::Start(ssnd + 8))?;
    file.read_exact(&mut offset)?;
    let offset = u64::from(u32::from_be_bytes(offset));
    let first = ssnd + 16 + offset;
    let frames = len.saturating_sub(first) / block_align;
    let end = first + frames * block_align;
    let size = |value: u64| {
        u32::try_from(value).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a size past what AIFF's 32-bit fields hold",
            )
        })
    };
    let (form, count, chunk) = (
        size(end - 8)?,
        size(frames)?,
        size(8 + offset + frames * block_align)?,
    );
    let put = |file: &mut std::fs::File, at: u64, value: u32| -> std::io::Result<()> {
        file.seek(SeekFrom::Start(at))?;
        file.write_all(&value.to_be_bytes())
    };
    put(file, 4, form)?;
    put(file, comm + 2, count)?;
    put(file, ssnd + 4, chunk)?;
    file.set_len(end)?;
    file.sync_all()
}
