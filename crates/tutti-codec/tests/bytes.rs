//! The bytes each encoder writes, against the formats' own layouts.
//!
//! A header field is read at its offset in the RIFF or AIFF layout and a few
//! sample codes are checked at the values the conversion rule gives (`x * 2^(n-1)`,
//! rounded half to even, clipped), so a change to a tag, a width, a block size,
//! a GUID or a scale fails here with the field named. FLAC's bytes are the
//! encoder library's choice, so its suite holds what the format promises
//! instead: every level decodes back to the quantized input, and the levels
//! differ.

#![cfg(any(
    feature = "encode-wav",
    feature = "encode-aiff",
    feature = "encode-flac"
))]

use std::io::Cursor;
use tutti_buffers::ChannelLayout;
use tutti_buffers::ChannelSet;
use tutti_buffers::Interleaved;
#[cfg(all(feature = "encode-flac", feature = "encode-wav"))]
use tutti_codec::FileEncoder;
use tutti_codec::FloatWidth;
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_codec::{Encoder, Format, Spec};
use tutti_units::SampleRate;

/// `frames` interleaved frames of `channels`: a 1024-step sawtooth offset per
/// channel, plus a 15-bit hash scaled by 1/16, every term exact in `f32`.
#[cfg(any(feature = "encode-wav", feature = "encode-flac"))]
fn input(channels: usize, frames: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            (0..channels).map(move |c| {
                let noise = ((i as u64 * i as u64 * 2_654_435_761 + c as u64 * 977) >> 11) % 32_768;
                ((i + c * 13) % 1024) as f32 / 1024.0 - 0.5 + noise as f32 / 32_768.0 / 16.0
            })
        })
        .collect()
}

/// The file `format` writes for `frames` frames of the input, as one block cut
/// in two.
#[cfg(any(feature = "encode-wav", feature = "encode-flac"))]
fn written(format: Format, channels: u16, frames: usize) -> Vec<u8> {
    let layout = ChannelLayout::from(channels);
    let spec = Spec::of_channels(format, ChannelSet::default_for(layout), SampleRate::SR_44K1);
    let samples = input(usize::from(channels), frames);
    let cut = frames / 3 * usize::from(channels);
    let mut encoder = Encoder::create(&spec, Cursor::new(Vec::new())).unwrap();
    encoder
        .write(Interleaved::new(&samples[..cut], layout).unwrap())
        .unwrap();
    encoder
        .write(Interleaved::new(&samples[cut..], layout).unwrap())
        .unwrap();
    encoder.finish().unwrap().into_inner()
}

/// The file `format` writes for exactly `samples`, at 44.1 kHz.
#[cfg(any(feature = "encode-wav", feature = "encode-aiff"))]
fn file_of<S: tutti_buffers::Sample>(format: Format, channels: u16, samples: &[S]) -> Vec<u8> {
    let layout = ChannelLayout::from(channels);
    let spec = Spec::of_channels(format, ChannelSet::default_for(layout), SampleRate::SR_44K1);
    let mut encoder = Encoder::create(&spec, Cursor::new(Vec::new())).unwrap();
    encoder
        .write(Interleaved::new(samples, layout).unwrap())
        .unwrap();
    encoder.finish().unwrap().into_inner()
}

#[cfg(feature = "encode-wav")]
fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

#[cfg(feature = "encode-wav")]
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The `KSDATAFORMAT_SUBTYPE_PCM` and `..._IEEE_FLOAT` GUIDs: the format tag
/// (1 and 3) then the fixed tail Microsoft's headers define.
#[cfg(feature = "encode-wav")]
fn subtype(tag: u8) -> [u8; 16] {
    [
        tag, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71,
    ]
}

/// A 16-bit stereo WAV is the 44-byte `PCMWAVEFORMAT` header: tag 1, the rate,
/// the byte rate, a block align of 4 and 16 bits; and its codes are the rule's.
#[cfg(feature = "encode-wav")]
#[test]
fn a_16_bit_wav_has_the_pcm_header_and_the_rules_codes() {
    let bytes = file_of(
        Format::Wav(tutti_codec::Pcm::Int(
            IntPcm::new(IntDepth::Int16).no_dither(),
        )),
        2,
        &[0.5f32, -0.5, 1.0, -1.0],
    );
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(u32_at(&bytes, 4), bytes.len() as u32 - 8, "RIFF size");
    assert_eq!(&bytes[8..16], b"WAVEfmt ");
    assert_eq!(u32_at(&bytes, 16), 16, "fmt size");
    assert_eq!(u16_at(&bytes, 20), 1, "PCM");
    assert_eq!(u16_at(&bytes, 22), 2, "channels");
    assert_eq!(u32_at(&bytes, 24), 44_100, "rate");
    assert_eq!(u32_at(&bytes, 28), 44_100 * 4, "byte rate");
    assert_eq!(u16_at(&bytes, 32), 4, "block align");
    assert_eq!(u16_at(&bytes, 34), 16, "bits");
    assert_eq!(&bytes[36..40], b"data");
    assert_eq!(u32_at(&bytes, 40), 8, "data size");
    let codes: Vec<i16> = bytes[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    assert_eq!(codes, [16384, -16384, 32767, -32768]);
}

/// A 24-bit six-channel WAV is `WAVE_FORMAT_EXTENSIBLE`: tag 0xFFFE, 22 extra
/// bytes, 24 valid bits, the 5.1 mask, the PCM sub-format and a block align of
/// 18; codes are three bytes each.
#[cfg(feature = "encode-wav")]
#[test]
fn a_24_bit_wav_is_extensible_with_three_byte_codes() {
    let mut samples = vec![0.0f32; 6];
    samples[0] = 0.5;
    samples[1] = -1.0;
    samples[2] = 1.0;
    let bytes = file_of(
        Format::Wav(tutti_codec::Pcm::Int(
            IntPcm::new(IntDepth::Int24).no_dither(),
        )),
        6,
        &samples,
    );
    assert_eq!(u32_at(&bytes, 16), 40, "fmt size");
    assert_eq!(u16_at(&bytes, 20), 0xFFFE, "extensible");
    assert_eq!(u16_at(&bytes, 22), 6, "channels");
    assert_eq!(u16_at(&bytes, 32), 18, "block align");
    assert_eq!(u16_at(&bytes, 34), 24, "container bits");
    assert_eq!(u16_at(&bytes, 36), 22, "extension size");
    assert_eq!(u16_at(&bytes, 38), 24, "valid bits");
    assert_eq!(u32_at(&bytes, 40), 0x60F, "5.1 channel mask");
    assert_eq!(&bytes[44..60], &subtype(1), "PCM sub-format");
    assert_eq!(&bytes[60..64], b"data");
    assert_eq!(u32_at(&bytes, 64), 18, "data size");
    let code = |at: usize| {
        let b = &bytes[68 + 3 * at..71 + 3 * at];
        i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8
    };
    assert_eq!(
        [code(0), code(1), code(2)],
        [4_194_304, -8_388_608, 8_388_607]
    );
}

/// The float widths are extensible with the IEEE float sub-format, 32 and 64
/// valid bits, block aligns of 8 and 16 for stereo, and every sample's bits.
#[cfg(feature = "encode-wav")]
#[test]
fn the_float_wavs_carry_the_ieee_float_subtype_and_every_bit() {
    use tutti_codec::Pcm;
    let wide = [1.0 / 3.0f64, -1.0e-300, 1.5, -0.0];
    for (pcm, bits, align) in [
        (Pcm::Float(FloatWidth::F32), 32u16, 8u16),
        (Pcm::Float(FloatWidth::F64), 64, 16),
    ] {
        let bytes = file_of(Format::Wav(pcm), 2, &wide);
        assert_eq!(u16_at(&bytes, 20), 0xFFFE, "{pcm:?} extensible");
        assert_eq!(u16_at(&bytes, 32), align, "{pcm:?} block align");
        assert_eq!(u16_at(&bytes, 34), bits, "{pcm:?} bits");
        assert_eq!(u16_at(&bytes, 38), bits, "{pcm:?} valid bits");
        assert_eq!(&bytes[44..60], &subtype(3), "{pcm:?} IEEE float sub-format");
        assert_eq!(
            u32_at(&bytes, 64) as usize,
            wide.len() * usize::from(bits / 8)
        );
        let data = &bytes[68..];
        match pcm {
            Pcm::Float(FloatWidth::F32) => {
                let got: Vec<u32> = data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_le_bytes(*b))
                    .collect();
                let want: Vec<u32> = wide.iter().map(|&x| (x as f32).to_bits()).collect();
                assert_eq!(got, want);
            }
            _ => {
                let got: Vec<u64> = data
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| u64::from_le_bytes(*b))
                    .collect();
                let want: Vec<u64> = wide.iter().map(|x| x.to_bits()).collect();
                assert_eq!(got, want);
            }
        }
    }
}

/// An empty take is a header alone: 44 bytes at 16 bits and 68 at 24, with
/// zero data and a RIFF size of the file less 8.
#[cfg(feature = "encode-wav")]
#[test]
fn an_empty_wav_is_a_header_alone() {
    for (pcm, len) in [
        (
            tutti_codec::Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither()),
            44usize,
        ),
        (
            tutti_codec::Pcm::Int(IntPcm::new(IntDepth::Int24).no_dither()),
            68,
        ),
    ] {
        let bytes = written(Format::Wav(pcm), 2, 0);
        assert_eq!(bytes.len(), len, "{pcm:?}");
        assert_eq!(u32_at(&bytes, 4) as usize, len - 8, "{pcm:?} RIFF size");
        assert_eq!(u32_at(&bytes, len - 4), 0, "{pcm:?} data size");
    }
}

/// The big-endian bytes of the `SSND` chunk's samples: the chunk's 8-byte header
/// and its offset and block size fields are skipped.
#[cfg(feature = "encode-aiff")]
fn ssnd_data(b: &[u8]) -> &[u8] {
    let at = b
        .windows(4)
        .position(|w| w == b"SSND")
        .expect("an SSND chunk");
    &b[at + 16..]
}

/// A 16-bit stereo AIFF is `FORM` / `AIFF` with a `COMM` chunk of 18 bytes
/// (channels, frames, bits and the rate as an 80-bit extended float: 44100 is
/// exponent 0x400E, mantissa 0xAC44 followed by zeros), and its codes are
/// big-endian and the rule's.
#[cfg(feature = "encode-aiff")]
#[test]
fn a_16_bit_aiff_has_the_comm_chunk_and_the_rules_codes() {
    let bytes = file_of(
        Format::Aiff(tutti_codec::Pcm::Int(
            IntPcm::new(IntDepth::Int16).no_dither(),
        )),
        2,
        &[0.5f32, -0.5, 1.0, -1.0],
    );
    assert_eq!(&bytes[..4], b"FORM");
    assert_eq!(
        u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize,
        bytes.len() - 8,
        "FORM size"
    );
    assert_eq!(&bytes[8..12], b"AIFF");
    assert_eq!(&bytes[12..16], b"COMM");
    assert_eq!(
        u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        18,
        "COMM size"
    );
    assert_eq!(u16::from_be_bytes([bytes[20], bytes[21]]), 2, "channels");
    assert_eq!(
        u32::from_be_bytes(bytes[22..26].try_into().unwrap()),
        2,
        "frames"
    );
    assert_eq!(u16::from_be_bytes([bytes[26], bytes[27]]), 16, "bits");
    assert_eq!(
        &bytes[28..38],
        &[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0],
        "44100 Hz as an 80-bit extended float"
    );
    let codes: Vec<i16> = ssnd_data(&bytes)
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_be_bytes(*b))
        .collect();
    assert_eq!(codes, [16384, -16384, 32767, -32768]);
}

/// The float widths are AIFF-C (`FORM` / `AIFC`) naming `fl32` or `fl64`, with
/// every sample's bits big-endian.
#[cfg(feature = "encode-aiff")]
#[test]
fn the_float_aiffs_are_aifc_naming_their_compression() {
    use tutti_codec::Pcm;
    let wide = [1.0 / 3.0f64, -1.0e-300];
    for (pcm, name) in [
        (Pcm::Float(FloatWidth::F32), b"fl32"),
        (Pcm::Float(FloatWidth::F64), b"fl64"),
    ] {
        let bytes = file_of(Format::Aiff(pcm), 2, &wide);
        assert_eq!(&bytes[8..12], b"AIFC", "{pcm:?}");
        assert!(
            bytes.windows(4).any(|w| w == name),
            "{pcm:?} names {name:?}"
        );
        let data = ssnd_data(&bytes);
        match pcm {
            Pcm::Float(FloatWidth::F32) => {
                let got: Vec<u32> = data[..8]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_be_bytes(*b))
                    .collect();
                let want: Vec<u32> = wide.iter().map(|&x| (x as f32).to_bits()).collect();
                assert_eq!(got, want);
            }
            _ => {
                let got: Vec<u64> = data[..16]
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|b| u64::from_be_bytes(*b))
                    .collect();
                let want: Vec<u64> = wide.iter().map(|x| x.to_bits()).collect();
                assert_eq!(got, want);
            }
        }
    }
}

/// The file `format` writes for the input, decoded back: one `Vec` per channel.
#[cfg(all(feature = "encode-flac", feature = "encode-wav"))]
fn decoded(format: Format, channels: u16, frames: usize) -> Vec<Vec<f32>> {
    let layout = ChannelLayout::from(channels);
    let spec = Spec::of_channels(format, ChannelSet::default_for(layout), SampleRate::SR_44K1);
    let samples = input(usize::from(channels), frames);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("written");
    let mut encoder = FileEncoder::create(&spec, &path).unwrap();
    encoder
        .write(Interleaved::new(&samples, layout).unwrap())
        .unwrap();
    encoder.finish().unwrap();
    let wave = tutti_codec::read_wave::<f32>(&path).unwrap();
    assert_eq!(
        wave.channels(),
        usize::from(channels),
        "{format:?} channels"
    );
    (0..wave.channels())
        .map(|c| wave.channel(c).samples().to_vec())
        .collect()
}

/// FLAC is lossless: at every level and depth, the file decodes to exactly the
/// samples the WAV writer of the same depth stores, which is the one
/// quantization (its codes are checked above).
#[cfg(all(feature = "encode-flac", feature = "encode-wav"))]
#[test]
fn flac_decodes_to_the_quantized_input_at_every_level() {
    use tutti_codec::FlacLevel::{Best, Fast, Fastest, Medium, Standard};
    use tutti_codec::IntDepth::{Int16, Int24};
    use tutti_codec::Pcm;
    for depth in [Int16, Int24] {
        let int = IntPcm::new(depth).no_dither();
        let pcm = Pcm::Int(int);
        for (channels, frames) in [(1u16, 3usize), (2, 10_000), (6, 10_000)] {
            let quantized = decoded(Format::Wav(pcm), channels, frames);
            for level in [Fastest, Fast, Medium, Standard, Best] {
                assert_eq!(
                    decoded(Format::Flac { pcm: int, level }, channels, frames),
                    quantized,
                    "FLAC {level:?} {depth:?} {channels}ch {frames}"
                );
            }
        }
    }
}

/// The five levels code this input with different options, so no level is
/// mapped onto another's: their files differ pairwise.
#[cfg(feature = "encode-flac")]
#[test]
fn the_flac_levels_write_different_files() {
    use tutti_codec::FlacLevel::{Best, Fast, Fastest, Medium, Standard};
    use tutti_codec::IntDepth::Int16;
    let files: Vec<_> = [Fastest, Fast, Medium, Standard, Best]
        .into_iter()
        .map(|level| {
            (
                level,
                written(
                    Format::Flac {
                        pcm: IntPcm::new(Int16).no_dither(),
                        level,
                    },
                    2,
                    10_000,
                ),
            )
        })
        .collect();
    for (i, (a, fa)) in files.iter().enumerate() {
        for (b, fb) in &files[i + 1..] {
            assert_ne!(fa, fb, "FLAC {a:?} and {b:?} wrote the same file");
        }
    }
}
