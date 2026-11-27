//! The file pipeline: an `Encoding` into a file or into memory, whole (`write`, `encode`) or
//! streamed (`Chain`).
//!
//! The WAV bytes are read by their layout (a 68-byte extensible header, then little-endian
//! samples) or by `hound`, so no reader of ours stands between the file and the figure. Every
//! expected figure is analytic: the conversion rule of `Sample`, the ITU/Dolby fold
//! coefficients, a sine at the output rate.

#![cfg(feature = "encode-wav")]

use tutti_buffers::{
    Bed, ChannelLayout, ChannelSet, ChannelTopology, Float, FloatWidth, Interleaved, Planes,
    Speaker, Wave,
};
use tutti_codec::{
    Chain, EncodeError, Encoding, Format, IntDepth, IntPcm, Kept, Landed, Pcm, Resample, ToFile,
    encode, write,
};
use tutti_units::{SampleRate, Samples};

/// The extensible `WAVE_FORMAT` header written for a float or wide file.
const HEADER: usize = 68;

fn float_wav() -> Format {
    Format::Wav(Pcm::Float(FloatWidth::F32))
}

fn int_wav(depth: IntDepth) -> Format {
    Format::Wav(Pcm::Int(IntPcm::new(depth).no_dither()))
}

fn wave<F: Float>(rate: SampleRate, planes: &[Vec<F>]) -> Wave<F> {
    Wave::from_planes(rate, Planes::from_vecs(planes).unwrap())
}

/// `planes` written as a WAV of `format` at 48 kHz; the file's bytes and what landed.
fn written<F: Float>(planes: Vec<Vec<F>>, format: Format) -> (Vec<u8>, Landed) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    let landed = write(
        &wave(SampleRate::SR_48K, &planes),
        &ToFile::new(&path, Encoding::new(format)),
    )
    .unwrap();
    (std::fs::read(&path).unwrap(), landed)
}

// ---- a float file takes the input as it is -------------------------------------------------

/// A float file takes the float input bit for bit: values past full scale, a subnormal and a
/// negative zero included.
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
    let (bytes, landed) = written(vec![left.clone(), right.clone()], float_wav());
    assert_eq!(landed.clipped(), 0, "a float file clips nothing");
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

/// An f64 wave written to an f64 WAV comes back exactly: the header says 64 bits and IEEE
/// float, and every sample keeps all of its bits.
#[test]
fn an_f64_wave_to_an_f64_wav_round_trips_exactly() {
    let left = vec![1.0 / 3.0, -2.0 / 3.0, 1.0 + f64::EPSILON, 1.0e-300, 0.1];
    let right = vec![f64::MIN_POSITIVE, -0.0, 0.5 - f64::EPSILON, -7.0, 1.0 / 7.0];
    let (bytes, landed) = written(
        vec![left.clone(), right.clone()],
        Format::Wav(Pcm::Float(FloatWidth::F64)),
    );
    assert_eq!(landed.clipped(), 0);
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

/// A full-scale square written to 16 bits without dither is `i16::MAX` at `+1.0` and `i16::MIN`
/// at `-1.0` (the rule in `Sample`), and the file counts the values at or above `+1.0`.
#[test]
fn a_full_scale_square_to_i16_is_max_or_min_and_counts_its_peaks() {
    let frames = 3000;
    let square: Vec<f32> = (0..frames)
        .map(|i| if i % 8 < 4 { 1.0 } else { -1.0 })
        .collect();
    let positive = square.iter().filter(|&&s| s >= 1.0).count() as u64;
    let (bytes, landed) = written(
        vec![square.clone(), square.clone()],
        int_wav(IntDepth::Int16),
    );
    // Two channels hold the same square.
    assert_eq!(landed.clipped(), 2 * positive);
    assert_eq!(landed.frames(), Samples::new(frames));
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

/// An f64 wave reaches an integer file by the same rule: 0.5 is half of full scale, exactly, at
/// 16 and 24 bits.
#[test]
fn an_f64_wave_converts_to_integers_by_the_rule() {
    let plane = vec![0.5f64, -0.5, 0.25, -1.0];
    let (bytes, landed) = written(vec![plane.clone(), plane.clone()], int_wav(IntDepth::Int16));
    assert_eq!(landed.clipped(), 0);
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

    let (bytes, _) = written(vec![plane.clone(), plane], int_wav(IntDepth::Int24));
    let sample = |at: usize| {
        let b = &bytes[HEADER + 3 * at..HEADER + 3 * at + 3];
        i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8
    };
    assert_eq!(
        [sample(0), sample(2), sample(6)],
        [4_194_304, -4_194_304, -8_388_608]
    );
}

// ---- fold, resample ------------------------------------------------------------------------

/// An f64 wave resampled 48 kHz -> 96 kHz stays f64: a 1 kHz sine, far inside the resampler's
/// flat passband, comes out as the analytic sine at the output rate. The 1e-9 bound is the
/// windowed-sinc filter's passband ripple (the worst deviation measures 2.3e-10), not the float
/// width. The first and last 4096 output frames are left out: the converter's edge transient
/// is not passband.
#[test]
fn an_f64_resample_holds_f64_precision_against_the_analytic_sine() {
    const HZ: f64 = 1000.0;
    const OUT: SampleRate = SampleRate::SR_96K;
    let frames = 48_000;
    let plane: Vec<f64> = (0..frames)
        .map(|n| (std::f64::consts::TAU * HZ * n as f64 / SampleRate::SR_48K.get()).sin())
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    let encoding =
        Encoding::new(Format::Wav(Pcm::Float(FloatWidth::F64))).resample(Resample::to(OUT));
    write(
        &wave(SampleRate::SR_48K, &[plane.clone(), plane]),
        &ToFile::new(&path, encoding.clone()),
    )
    .unwrap();

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

/// A whole wave folded onto stereo and resampled reads back at the encoding's rate and
/// channels: 96 kHz 5.1 into 48 kHz stereo is half the frames, two channels, and the header
/// says 48 kHz.
#[test]
fn a_folded_resampled_wave_reads_back_at_the_encodings_rate_and_channels() {
    let frames = 9_600;
    let planes: Vec<Vec<f32>> = (0..6).map(|_| vec![0.25; frames]).collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("down.wav");
    let encoding = Encoding::new(float_wav())
        .channels(ChannelSet::default_for(ChannelLayout::STEREO))
        .resample(Resample::to(SampleRate::SR_48K));
    let landed = write(
        &wave(SampleRate::SR_96K, &planes),
        &ToFile::new(&path, encoding.clone()),
    )
    .unwrap();
    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().sample_rate, 48_000);
    assert_eq!(reader.spec().channels, 2);
    let held = reader.len() as usize / 2;
    assert_eq!(landed.frames(), Samples::new(held));
    // Half the frames, within the converter's own length: the stated length of the resample.
    assert_eq!(
        landed.frames(),
        tutti_dsp::resampled_frames(SampleRate::SR_96K, SampleRate::SR_48K, Samples::new(frames))
    );
}

/// The fold of a wave of another width than the file's is the one an output's is: 5.1 planes
/// written as stereo carry the centre and the surrounds at -3 dB into the front pair, and drop
/// only the LFE: `L + C*f32::MINUS_3_DB + Ls*f32::MINUS_3_DB`.
#[test]
fn surround_planes_written_as_stereo_are_folded_not_truncated() {
    // L R C LFE Ls Rs
    let planes = constants(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
    let frame = first_frame(
        &planes,
        &Encoding::new(float_wav()).channels(ChannelSet::default_for(ChannelLayout::STEREO)),
        2,
    );
    let left = 0.1 + 0.3 * f32::MINUS_3_DB + 0.5 * f32::MINUS_3_DB;
    let right = 0.2 + 0.3 * f32::MINUS_3_DB + 0.6 * f32::MINUS_3_DB;
    assert!((frame[0] - left).abs() < 1e-6, "{} vs {left}", frame[0]);
    assert!((frame[1] - right).abs() < 1e-6, "{} vs {right}", frame[1]);
}

/// The file's stated topology decides the placement: 5.1 into a file whose four channels are
/// L R C LFE puts the centre in its own channel at full level, where the default quad order
/// would fold it into the fronts.
#[test]
fn the_files_topology_places_the_channels() {
    let topology = ChannelTopology::new([
        Speaker::FrontLeft,
        Speaker::FrontRight,
        Speaker::FrontCenter,
        Speaker::LowFrequency,
    ])
    .unwrap();
    let frame = first_frame(
        &constants(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]),
        &Encoding::new(float_wav()).channels(ChannelSet::from(topology)),
        4,
    );
    assert!(
        (frame[2] - 0.3).abs() < 1e-6,
        "the centre keeps its channel"
    );
    assert!((frame[3] - 0.4).abs() < 1e-6, "and the LFE its own");
}

/// A width with no order that would have to drop a channel is refused, not truncated: 5
/// planes into a stereo file.
#[test]
fn a_width_it_cannot_place_is_an_error_not_a_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refused.wav");
    let err = write(
        &wave(SampleRate::SR_48K, &constants(&[0.1; 5])),
        &ToFile::new(
            &path,
            Encoding::new(float_wav()).channels(ChannelSet::default_for(ChannelLayout::STEREO)),
        ),
    )
    .expect_err("five unordered channels have no place in two");
    assert!(matches!(err, EncodeError::Fold(_)), "{err:?}");
    assert!(!path.exists(), "nothing was written");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// Two sets of one width in different orders are folded by speaker: a 5.1 wave whose front pair
/// is swapped, written into the default 5.1, reaches the file with the pair put right.
#[test]
fn a_same_width_reorder_reaches_the_file() {
    let default = ChannelTopology::default_for(ChannelLayout::from(6u16)).unwrap();
    let mut speakers = default.positions().unwrap().to_vec();
    speakers.swap(0, 1);
    let source = ChannelTopology::new(speakers).unwrap();
    let planes = constants(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reordered.wav");
    let wave = Wave::with_topology(source, SampleRate::SR_48K);
    let mut wave = wave;
    wave.append_planes(Planes::from_vecs(&planes).unwrap())
        .unwrap();
    let encoding = Encoding::new(float_wav()).channels(ChannelSet::from(default));
    write(&wave, &ToFile::new(&path, encoding.clone())).unwrap();
    let samples: Vec<f32> = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    assert_eq!(&samples[..6], &[0.2, 0.1, 0.3, 0.4, 0.5, 0.6]);
}

/// Planes already at the file's width are written as they are, whatever the topology says.
#[test]
fn planes_at_the_files_width_are_written_unchanged() {
    let frame = first_frame(
        &constants(&[0.25, -0.5]),
        &Encoding::new(float_wav()).channels(ChannelSet::default_for(ChannelLayout::STEREO)),
        2,
    );
    assert_eq!(frame, vec![0.25, -0.5]);
}

const FRAMES: usize = 256;

/// One constant per channel, so a channel that goes missing shows in the sum.
fn constants(values: &[f32]) -> Vec<Vec<f32>> {
    values.iter().map(|&v| vec![v; FRAMES]).collect()
}

/// The first frame of the file `planes` are written as.
fn first_frame(planes: &[Vec<f32>], encoding: &Encoding, width: u16) -> Vec<f32> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    write(
        &wave(SampleRate::SR_48K, planes),
        &ToFile::new(&path, encoding.clone()),
    )
    .unwrap();
    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, width);
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    assert_eq!(samples.len(), FRAMES * usize::from(width));
    samples[..usize::from(width)].to_vec()
}

// ---- one pipeline: memory, file, stream ----------------------------------------------------

/// `encode` and `write` of one wave with one encoding give equal bytes: the stages are the
/// same, whatever the destination. A fold, a resample and a dither (which the seed fixes) are in
/// the path.
#[test]
fn encode_and_write_of_one_wave_give_equal_bytes() {
    let frames = 6_000;
    let planes: Vec<Vec<f32>> = (0..6)
        .map(|c| {
            (0..frames)
                .map(|i| (((i * 7 + c * 31) % 200) as f32 - 100.0) / 128.0)
                .collect()
        })
        .collect();
    let wave = wave(SampleRate::SR_48K, &planes);
    let encoding = Encoding::new(Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16))))
        .channels(ChannelSet::default_for(ChannelLayout::STEREO))
        .resample(Resample::to(SampleRate::SR_44K1))
        .dither_seed(7);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("same.wav");
    write(&wave, &ToFile::new(&path, encoding.clone())).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        encode(&wave, &encoding).unwrap()
    );
}

/// Two dither seeds give two noise sequences, and one seed gives one: the seed an `Encoding`
/// states is the one the file is dithered with.
#[test]
fn the_dither_seed_reaches_the_files_noise() {
    // Silence into a dithered 16-bit file is the dither alone.
    let silence = wave(SampleRate::SR_48K, &[vec![0.0f32; 4_096]]);
    let format = Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16)));
    let bytes = |seed| encode(&silence, &Encoding::new(format).dither_seed(seed)).unwrap();
    assert!(
        bytes(1)[44..].iter().any(|&b| b != 0),
        "the file holds dither"
    );
    assert_eq!(bytes(1), bytes(1));
    assert_ne!(bytes(1), bytes(2), "two seeds shared one noise sequence");
    assert_eq!(
        bytes(tutti_dsp::DitherNoise::DEFAULT_SEED),
        encode(&silence, &Encoding::new(format)).unwrap(),
        "an encoding that states no seed is dithered with the default one"
    );
}

/// **A chain whose encoder failed mid-file keeps the unfinished file, and the error names
/// it**: the frames written before the failure are in it, the path is untouched, and
/// dropping the chain or discarding the kept file removes it.
#[test]
fn a_chain_that_failed_mid_file_keeps_the_frames_written_and_names_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("take.wav");
    let stereo = ChannelSet::default_for(ChannelLayout::STEREO);
    let open = || {
        Chain::<f32>::new(
            &ToFile::new(&path, Encoding::new(float_wav())),
            &stereo,
            SampleRate::SR_48K,
            Samples::new(3_000),
        )
        .unwrap()
    };
    let good = vec![0.25f32; 2 * 3_000];
    let wrong = [0.0f32; 3];

    let mut chain = open();
    chain
        .block(Interleaved::new(&good, ChannelLayout::STEREO).unwrap())
        .unwrap();
    chain
        .block(Interleaved::new(&wrong, ChannelLayout::from(3u8)).unwrap())
        .expect_err("a block of another width");
    let Err(Kept::Unfinished { unfinished, .. }) = chain.finalize() else {
        panic!("not kept unfinished");
    };
    assert!(!path.exists(), "the path is untouched");
    assert!(unfinished.header_describes_frames);
    let kept = hound::WavReader::open(&unfinished.path).unwrap();
    assert_eq!(
        kept.duration(),
        3_000,
        "the frames written before the failure"
    );
    std::fs::remove_file(&unfinished.path).unwrap();

    drop(open());
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "a chain dropped before finalize leaves nothing"
    );

    let mut chain = open();
    chain
        .block(Interleaved::new(&wrong, ChannelLayout::from(3u8)).unwrap())
        .expect_err("a block of another width");
    let error = chain.finalize().map_err(Kept::discard).unwrap_err();
    assert!(matches!(error, EncodeError::BlockWidth { .. }), "{error}");
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "a caller that discards the kept file leaves nothing"
    );
}

/// An existing path is refused by default, with the bytes already there left alone, and
/// replaced once `replace` is said; the streaming form takes the same destination.
#[test]
fn a_destination_refuses_an_existing_path_unless_it_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    std::fs::write(&path, b"sentinel").unwrap();
    let encoding = Encoding::new(float_wav());
    let source = ChannelSet::default_for(ChannelLayout::MONO);
    let chain = |to: &ToFile| Chain::<f32>::new(to, &source, SampleRate::SR_48K, Samples::new(8));

    let err = chain(&ToFile::new(&path, encoding.clone())).expect_err("the path exists");
    assert!(
        matches!(&err, EncodeError::Exists { path: p } if *p == path),
        "{err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");

    let mut open = chain(&ToFile::new(&path, encoding.clone()).replace()).unwrap();
    open.block(Interleaved::new(&[0.5f32; 8], ChannelLayout::MONO).unwrap())
        .unwrap();
    open.finalize().unwrap().commit().unwrap();
    assert!(std::fs::read(&path).unwrap().starts_with(b"RIFF"));
}

// ---- the file a chain refuses to begin, and the mask it writes -----------------------------

/// A plan that cannot fit a WAV file is refused before any of it is written and before the
/// path is touched: bytes already at the path survive.
///
/// The plan is real: 2^32 mono frames at 16 bits is 8 GiB, past the format's 32-bit sizes,
/// and no frame is written because the refusal is made before the file is opened.
#[test]
fn a_plan_that_cannot_fit_is_refused_before_the_file_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keep.wav");
    std::fs::write(&path, b"sentinel").unwrap();
    let err = Chain::<f32>::new(
        &ToFile::new(
            &path,
            Encoding::new(Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16)))),
        ),
        &ChannelSet::default_for(ChannelLayout::MONO),
        SampleRate::of(1_000.0),
        Samples::new(1 << 32),
    )
    .map(drop)
    .expect_err("2^32 mono 16-bit frames do not fit a WAV file");
    assert!(
        matches!(err, EncodeError::WavTooLarge { limit_frames } if limit_frames < 1 << 32),
        "{err}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"sentinel",
        "a refused file must not touch the path"
    );
}

/// The limit is judged on the frames the file will hold, which a resample to a higher rate
/// raises: 2^31 - 1000 frames fit a mono 16-bit WAV at the source rate, and do not once they
/// are written at twice the rate.
#[test]
fn the_limit_is_judged_at_the_output_rate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    let plan = Samples::new((1 << 31) - 1_000);
    let rate = SampleRate::of(1_000.0);
    let mono = ChannelSet::default_for(ChannelLayout::MONO);
    let encoding = Encoding::new(Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int16))));
    Chain::<f32>::new(&ToFile::new(&path, encoding.clone()), &mono, rate, plan)
        .map(drop)
        .expect("2^31 - 1000 mono 16-bit frames fit");
    let doubled = encoding.resample(Resample::to(SampleRate::of(2_000.0)));
    let err = Chain::<f32>::new(&ToFile::new(&path, doubled.clone()), &mono, rate, plan)
        .map(drop)
        .expect_err("twice the frames do not");
    assert!(matches!(err, EncodeError::WavTooLarge { .. }), "{err}");
}

/// A topology reaches the file: four channels declare the quad mask `0x33`, told they are quad
/// or default to it by their width, six declare 5.1 by their width, and four unassigned
/// channels declare none.
#[test]
fn the_topology_reaches_the_file() {
    let dir = tempfile::tempdir().unwrap();
    for (case, (channels, set, mask)) in [
        (4u16, ChannelSet::from(Bed::Quad.topology()), 0x33u32),
        (4, ChannelSet::default_for(ChannelLayout::from(4u16)), 0x33),
        (
            4,
            ChannelSet::from(ChannelTopology::unassigned(ChannelLayout::QUAD).unwrap()),
            0,
        ),
        (6, ChannelSet::default_for(ChannelLayout::from(6u16)), 0x60F),
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.path().join(format!("m{case}.wav"));
        let layout = ChannelLayout::from(channels);
        let mut chain = Chain::<f32>::new(
            &ToFile::new(
                &path,
                Encoding::new(Format::Wav(Pcm::Int(IntPcm::new(IntDepth::Int24)))),
            ),
            &set,
            SampleRate::of(1_000.0),
            Samples::new(8),
        )
        .unwrap();
        chain
            .block(Interleaved::new(&vec![0.5f32; 8 * usize::from(channels)], layout).unwrap())
            .unwrap();
        chain.finalize().unwrap().commit().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            mask,
            "{channels} channels"
        );
        assert_eq!(
            hound::WavReader::open(&path).unwrap().len(),
            8 * u32::from(channels)
        );
    }
}
