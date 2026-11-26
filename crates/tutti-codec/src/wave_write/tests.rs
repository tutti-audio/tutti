use super::*;
#[cfg(all(feature = "encode-wav", feature = "encode-flac", decode))]
use crate::FlacLevel;
use crate::{Encoding, Format, ToFile};
use crate::{IntDepth, IntPcm};
use tutti_buffers::FloatWidth;
#[cfg(all(feature = "encode-wav", decode))]
use tutti_buffers::{ChannelTopology, Interleaved};
use tutti_units::SampleRate;

/// `frames` frames over `topology` whose sample at `(c, i)` is
/// `((i * 7 + c * 3) % 400 - 200) / 256`: a ramp per channel that wraps, and
/// exact at 16 bits (a whole multiple of 128 / 32768).
#[cfg(all(feature = "encode-wav", decode))]
fn ramp_wave(topology: ChannelTopology, rate: f64, frames: usize) -> Wave {
    let mut wave = Wave::with_topology(topology, SampleRate::of(rate));
    let width = wave.channels();
    for i in 0..frames {
        let frame: Vec<f32> = (0..width)
            .map(|c| (((i * 7 + c * 3) % 400) as f32 - 200.0) / 256.0)
            .collect();
        wave.append(Interleaved::new(&frame, wave.layout()).unwrap())
            .unwrap();
    }
    wave
}

/// A wave wider than a topology holds has none, and is still written and read
/// back whole: its width is the file's, and the speaker mask says no speaker.
#[cfg(all(feature = "encode-wav", decode))]
#[test]
fn a_wave_wider_than_a_topology_has_none_and_round_trips() {
    use crate::{Pcm, read_wave};
    let mut wave = Wave::<f32>::new(17, SampleRate::SR_48K);
    assert_eq!(wave.topology(), None);
    assert_eq!(wave.layout().count(), 17);
    let frame: Vec<f32> = (0..17).map(|c| c as f32 / 32.0).collect();
    for _ in 0..5 {
        wave.append(Interleaved::new(&frame, wave.layout()).unwrap())
            .unwrap();
    }
    let bytes = encode(
        &wave,
        &Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32))),
    )
    .unwrap();
    let back = read_wave::<f32>(bytes).unwrap();
    assert_eq!(back, wave);
    assert_eq!((back.layout().count(), back.topology()), (17, None));
}

/// Writing a wave and reading the file back gives the same samples, rate and
/// topology: exactly for WAV's float format, and to within one 16-bit step
/// (the writer scales by 32767 and the reader by 32768) for the integer ones.
#[cfg(all(feature = "encode-wav", feature = "encode-flac", decode))]
#[test]
fn a_written_wave_reads_back_as_itself() {
    use crate::{Pcm, read_wave};
    let dir = tempfile::tempdir().unwrap();
    let stereo = || tutti_buffers::Bed::Stereo.topology();
    let cases = [
        (
            "stereo.wav",
            Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither())),
            stereo(),
        ),
        (
            "float.wav",
            Format::Wav(Pcm::Float(FloatWidth::F32)),
            stereo(),
        ),
        (
            "quad.wav",
            Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither())),
            tutti_buffers::Bed::Quad.topology(),
        ),
        (
            "stereo.flac",
            Format::Flac {
                pcm: IntPcm::new(IntDepth::Int16).no_dither(),
                level: FlacLevel::Standard,
            },
            stereo(),
        ),
    ];
    for (name, format, topology) in cases {
        // Past one write chunk, so the chunking is exercised too.
        let wave = ramp_wave(topology, 44_100.0, 20_000);
        let path = dir.path().join(name);
        write(&wave, &Encoding::new(format), &ToFile::new(&path)).unwrap();
        let back = read_wave::<f32>(&path).unwrap();
        assert_eq!(back.sample_rate(), wave.sample_rate(), "{name}: rate");
        assert_eq!(back.topology(), wave.topology(), "{name}: topology");
        assert_eq!(back.len(), wave.len(), "{name}: length");
        let step = if format == Format::Wav(Pcm::Float(FloatWidth::F32)) {
            0.0
        } else {
            1.0 / 32_768.0
        };
        for c in 0..wave.channels() {
            let pairs = back.channel(c).iter().zip(wave.channel(c));
            for (i, (&got, &want)) in pairs.enumerate() {
                assert!(
                    (got - want).abs() <= step,
                    "{name}: channel {c}, frame {i}: {got} for {want}"
                );
            }
        }
    }
}

/// A wave of 16-bit samples written at 16 bits reads back as the same
/// integers: the encoder takes them as they are (an integer-to-integer
/// conversion of one width is the identity), with no float in between.
#[test]
fn an_integer_wave_writes_the_integers_it_holds() {
    use crate::{Pcm, read_wave};
    let samples = [0i16, 16_384, -16_384, 32_767, -32_768, 1, -1];
    let wave = Wave::from_mono(SampleRate::SR_48K, tutti_buffers::Mono::new(&samples));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ints.wav");
    let format = Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither()));
    write(&wave, &Encoding::new(format), &ToFile::new(&path)).unwrap();
    let back = read_wave::<i16>(&path).unwrap();
    assert!(back.channel(0).iter().eq(&samples));
}

/// A wave of `f64` samples written as 64-bit float reads back bit for bit:
/// `0.1` and `1 + 2^-40` have no `f32` of the same value, so a narrowing on
/// the way would change both.
#[test]
fn an_f64_wave_writes_as_float64_bit_for_bit() {
    use crate::{Pcm, read_wave};
    let samples = [0.1f64, 1.0 + 2f64.powi(-40), -0.3];
    let wave = Wave::from_mono(SampleRate::SR_48K, tutti_buffers::Mono::new(&samples));
    let bytes = encode(
        &wave,
        &Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F64))),
    )
    .unwrap();
    let back = read_wave::<f64>(bytes).unwrap();
    let got: Vec<u64> = back.channel(0).iter().map(|x| x.to_bits()).collect();
    let want: Vec<u64> = samples.iter().map(|x| x.to_bits()).collect();
    assert_eq!(got, want);
}

/// An `f64` sample is quantized to 24 bits from its own value: `x` with
/// `x * 2^23 = 4_194_304.5 + 2^-10` rounds up to `4_194_305`. Through `f32`
/// (whose step at `0.5` is `2^-24`) it would be the tie `4_194_304.5`, which
/// rounds half to even down to `4_194_304`.
#[test]
fn an_f64_wave_quantizes_to_24_bits_from_its_own_value() {
    use crate::{Pcm, read_wave};
    let scale = 8_388_608.0; // 2^23
    let x = (4_194_304.5 + 2f64.powi(-10)) / scale;
    let wave = Wave::from_mono(SampleRate::SR_48K, tutti_buffers::Mono::new(&[x]));
    let bytes = encode(
        &wave,
        &Encoding::new(Format::Wav(Pcm::Int(
            IntPcm::new(IntDepth::Int24).no_dither(),
        ))),
    )
    .unwrap();
    let back = read_wave::<f64>(bytes).unwrap();
    let got: Vec<f64> = back.channel(0).iter().copied().collect();
    assert_eq!(got, [4_194_305.0 / scale]);
}

/// A stage that runs for an `i32` wave keeps every bit of it: a stereo wave whose samples
/// need all 31 magnitude bits, written with the same speakers in the other order (a fold at
/// unity gain, so a stage runs), reads back with the channels swapped and `n / 2^31` exactly
/// in a 64-bit float file, whose step is below an `i32`'s. A stage in `f32` would round each
/// value to 24 bits. (The codec writes no 32-bit integer file: `IntDepth` is 16 or 24.)
#[cfg(all(feature = "encode-wav", decode))]
#[test]
fn an_i32_wave_keeps_its_low_bits_through_a_stage() {
    use crate::{Pcm, read_wave};
    use tutti_buffers::{ChannelSet, ChannelTopology, Speaker};
    let frames = 64usize;
    let left: Vec<i32> = (0..frames as i32).map(|i| 0x4000_0001 + 3 * i).collect();
    let right: Vec<i32> = (0..frames as i32).map(|i| -0x2000_0007 - 5 * i).collect();
    let mut wave = Wave::<i32>::with_topology(
        ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight]).unwrap(),
        SampleRate::SR_48K,
    );
    for (l, r) in left.iter().zip(&right) {
        wave.append(Interleaved::new(&[*l, *r], wave.layout()).unwrap())
            .unwrap();
    }
    let swapped =
        ChannelSet::from(ChannelTopology::new([Speaker::FrontRight, Speaker::FrontLeft]).unwrap());
    let encoding = Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F64))).channels(swapped);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swapped.wav");
    write(&wave, &encoding, &ToFile::new(&path)).unwrap();
    for back in [
        read_wave::<f64>(&path).unwrap(),
        read_wave::<f64>(encode(&wave, &encoding).unwrap()).unwrap(),
    ] {
        let exact = |plane: &[i32]| -> Vec<f64> {
            plane
                .iter()
                .map(|&n| f64::from(n) / 2_147_483_648.0)
                .collect()
        };
        assert_eq!(
            back.channel(0).iter().copied().collect::<Vec<_>>(),
            exact(&right)
        );
        assert_eq!(
            back.channel(1).iter().copied().collect::<Vec<_>>(),
            exact(&left)
        );
    }
}

/// A path is refused when something is at it, leaving the old bytes and no temporary,
/// written when nothing is, and replaced when `replace` says so.
#[test]
fn a_path_refuses_an_existing_file_unless_it_replaces() {
    use crate::{EncodeError, Pcm};
    let wave = Wave::from_mono(SampleRate::SR_48K, tutti_buffers::Mono::new(&[0.5f32; 8]));
    let encoding = Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32)));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    std::fs::write(&path, b"old").unwrap();
    let refused = write(&wave, &encoding, &ToFile::new(&path));
    assert!(
        matches!(&refused, Err(EncodeError::Exists { path: p }) if *p == path),
        "an existing file is refused: {refused:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    write(&wave, &encoding, &ToFile::new(&path).replace()).unwrap();
    assert_eq!(&std::fs::read(&path).unwrap()[..4], b"RIFF");

    let fresh = dir.path().join("fresh.wav");
    write(&wave, &encoding, &ToFile::new(&fresh)).unwrap();
    assert_eq!(&std::fs::read(&fresh).unwrap()[..4], b"RIFF");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

/// A wave of 20 001 16-bit samples: past one write chunk, an odd length.
#[cfg(all(feature = "encode-wav", decode))]
fn long_int_wave() -> (Vec<i16>, Wave<i16>) {
    let samples: Vec<i16> = (0..20_001i32)
        .map(|i| ((i * 7919) % 65_536 - 32_768) as i16)
        .collect();
    let wave = Wave::from_mono(SampleRate::SR_48K, tutti_buffers::Mono::new(&samples));
    (samples, wave)
}

/// A wave written as WAV and read back is equal to the original: the same
/// samples, rate and speakers are the same wave, whether the original's
/// topology was stated or the default layout for its channel count.
#[cfg(all(feature = "encode-wav", decode))]
#[test]
fn a_wave_written_as_wav_reads_back_equal() {
    use crate::{Pcm, read_wave};
    let (_, mono) = long_int_wave();
    let stereo = ramp_wave(tutti_buffers::Bed::Stereo.topology(), 44_100.0, 500);
    let format = Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither()));
    let bytes = encode(&mono, &Encoding::new(format)).unwrap();
    assert_eq!(read_wave::<i16>(bytes).unwrap(), mono);
    let bytes = encode(
        &stereo,
        &Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F32))),
    )
    .unwrap();
    assert_eq!(read_wave::<f32>(bytes).unwrap(), stereo);
}
