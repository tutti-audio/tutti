//! The sample format a render is written from: any `Float`, and a float file
//! takes it as it is.
//!
//! The WAV bytes are read here by their layout (a 68-byte extensible header,
//! then little-endian samples), so no reader of ours stands between the file
//! and the figure.

use tutti_buffers::ChannelLayout;
use tutti_buffers::FloatWidth;
use tutti_codec::IntDepth;
use tutti_codec::IntPcm;
use tutti_codec::{Format, Pcm};
use tutti_export::{EventLossPolicy, FileTarget, Rendered, Resample, write_buffers};
use tutti_units::SampleRate;

const RATE: SampleRate = SampleRate::SR_48K;
/// The extensible `WAVE_FORMAT` header written for a float or wide file.
const HEADER: usize = 68;

fn write<F: tutti_dsp::FftFloat>(
    planes: Vec<Vec<F>>,
    format: Format,
) -> (Vec<u8>, tutti_export::Written) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    let rendered = Rendered::new(planes, RATE, 0).unwrap();
    let target = FileTarget::new(&path, format, ChannelLayout::STEREO);
    let written = write_buffers(&rendered, &target, EventLossPolicy::Fail).unwrap();
    (std::fs::read(&path).unwrap(), written)
}

/// A float file takes the float input bit for bit: values past full scale, a subnormal and a negative zero included.
#[test]
fn an_f32_file_gets_the_f32_input_bit_for_bit() {
    let left = vec![
        0.1f32,
        -0.7,
        1.5,
        f32::MIN_POSITIVE / 4.0,
        -0.0,
        0.333_333_34,
    ];
    let right = vec![0.25f32, 1.0, -1.0, -3.0, 1.0e-30, 0.999_999_9];
    let (bytes, written) = write(
        vec![left.clone(), right.clone()],
        Format::Wav(Pcm::Float(FloatWidth::F32)),
    );
    assert_eq!(written.clipped_samples, 0, "a float file clips nothing");
    let got: Vec<u32> = bytes[HEADER..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    let want: Vec<u32> = left
        .iter()
        .zip(&right)
        .flat_map(|(l, r)| [l.to_bits(), r.to_bits()])
        .collect();
    assert_eq!(got, want);
}

/// An f64 render written to an f64 WAV comes back exactly: the header says 64
/// bits and IEEE float, and every sample keeps all of its bits.
#[test]
fn an_f64_render_to_an_f64_wav_round_trips_exactly() {
    let left = vec![1.0 / 3.0, -2.0 / 3.0, 1.0 + f64::EPSILON, 1.0e-300, 0.1];
    let right = vec![f64::MIN_POSITIVE, -0.0, 0.5 - f64::EPSILON, -7.0, 1.0 / 7.0];
    let (bytes, written) = write(
        vec![left.clone(), right.clone()],
        Format::Wav(Pcm::Float(FloatWidth::F64)),
    );
    assert_eq!(written.clipped_samples, 0);
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[20..22], &0xFFFEu16.to_le_bytes(), "extensible");
    assert_eq!(&bytes[34..36], &64u16.to_le_bytes(), "bits per sample");
    assert_eq!(
        &bytes[44..46],
        &3u16.to_le_bytes(),
        "the IEEE float sub-format"
    );
    let got: Vec<u64> = bytes[HEADER..]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| u64::from_le_bytes(*b))
        .collect();
    let want: Vec<u64> = left
        .iter()
        .zip(&right)
        .flat_map(|(l, r)| [l.to_bits(), r.to_bits()])
        .collect();
    assert_eq!(got, want);
}

/// A full-scale square written to 16 bits without dither is `i16::MAX` at
/// `+1.0` and `i16::MIN` at `-1.0` (the rule in `Sample`), and the file counts
/// the values at or above `+1.0`.
#[test]
fn a_full_scale_square_to_i16_is_max_or_min_and_counts_its_peaks() {
    let frames = 3000;
    let square: Vec<f32> = (0..frames)
        .map(|i| if i % 8 < 4 { 1.0 } else { -1.0 })
        .collect();
    let positive = square.iter().filter(|&&s| s >= 1.0).count() as u64;
    let (bytes, written) = write(
        vec![square.clone(), square.clone()],
        Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither())),
    );
    // Two channels hold the same square.
    assert_eq!(written.clipped_samples, 2 * positive);
    let got: Vec<i16> = bytes[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    assert_eq!(got.len(), 2 * frames);
    for (frame, s) in got.as_chunks::<2>().0.iter().zip(&square) {
        let want = if *s >= 1.0 { i16::MAX } else { i16::MIN };
        assert_eq!(*frame, [want, want]);
    }
}

/// An f64 render reaches an integer file by the same rule: 0.5 is half of
/// full scale, exactly, at 16 and 24 bits.
#[test]
fn an_f64_render_converts_to_integers_by_the_rule() {
    let plane = vec![0.5f64, -0.5, 0.25, -1.0];
    let (bytes, written) = write(
        vec![plane.clone(), plane.clone()],
        Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16).no_dither())),
    );
    assert_eq!(written.clipped_samples, 0);
    let got: Vec<i16> = bytes[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    assert_eq!(
        got,
        [16384, 16384, -16384, -16384, 8192, 8192, -32768, -32768]
    );

    let (bytes, _) = write(
        vec![plane.clone(), plane],
        Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24).no_dither())),
    );
    let sample = |at: usize| {
        let b = &bytes[HEADER + 3 * at..HEADER + 3 * at + 3];
        i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8
    };
    assert_eq!(
        [sample(0), sample(2), sample(6)],
        [4_194_304, -4_194_304, -8_388_608]
    );
}

/// An f64 render resampled 48 kHz -> 96 kHz stays f64: a 1 kHz sine, far inside
/// the resampler's flat passband, comes out as the analytic sine at the output
/// rate. The 1e-9 bound is the windowed-sinc filter's passband ripple (the
/// worst deviation measures 2.3e-10), not the float width. The first and last
/// 4096 output frames are left out: the converter's edge transient is not
/// passband.
#[test]
fn an_f64_resample_holds_f64_precision_against_the_analytic_sine() {
    const HZ: f64 = 1000.0;
    const OUT: SampleRate = SampleRate::SR_96K;
    let frames = 48_000;
    let plane: Vec<f64> = (0..frames)
        .map(|n| (std::f64::consts::TAU * HZ * n as f64 / RATE.get()).sin())
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    let rendered = Rendered::new(vec![plane.clone(), plane], RATE, 0).unwrap();
    let target = FileTarget::new(
        &path,
        Format::Wav(Pcm::Float(FloatWidth::F64)),
        ChannelLayout::STEREO,
    )
    .resample(Resample::to(OUT));
    write_buffers(&rendered, &target, EventLossPolicy::Fail).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let left: Vec<f64> = bytes[HEADER..]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| f64::from_le_bytes(*b))
        .step_by(2)
        .collect();
    assert_eq!(
        left.len(),
        2 * frames,
        "the file holds the frames at 96 kHz"
    );
    let edge = 4096;
    let worst = (edge..left.len() - edge)
        .map(|m| {
            let want = (std::f64::consts::TAU * HZ * m as f64 / OUT.get()).sin();
            (left[m] - want).abs()
        })
        .fold(0.0, f64::max);
    assert!(
        worst < 1e-9,
        "worst deviation {worst:e} from the analytic sine"
    );
}
