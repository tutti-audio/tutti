use super::*;
use tutti_buffers::I24;
use tutti_units::SampleRate;

/// The workspace's dither seed.
const SEED: u64 = 0x1234_5678;

/// One i16 step in the float domain.
const LSB: f64 = 1.0 / 32768.0;
const AMP_LSB: Amplitude = Amplitude::new(LSB);

fn rate(hz: f64) -> SampleRate {
    SampleRate::new(hz).unwrap()
}

/// A sine of amplitude 0.25 at 997 Hz, far from clipping and incommensurate with every block
/// size used here.
fn tone(n: usize, sample_rate: f64) -> Vec<f64> {
    tutti_fixtures::signal::sine(
        tutti_units::Hz::new(997.0),
        Amplitude::new(0.25),
        rate(sample_rate),
        Samples::new(n),
    )
}

fn stereo<F: Float>(samples: &[F]) -> Interleaved<'_, F> {
    Interleaved::new(samples, ChannelLayout::STEREO).unwrap()
}

fn stereo_mut<S: Sample>(samples: &mut [S]) -> InterleavedMut<'_, S> {
    InterleavedMut::new(samples, ChannelLayout::STEREO).unwrap()
}

fn signal(n: usize) -> Vec<f32> {
    (0..n).map(|i| ((i % 97) as f32 / 97.0) - 0.5).collect()
}

fn tpdf_i16() -> Quantizer<f32> {
    Quantizer::new(Some(DitherShape::Triangular), ChannelLayout::STEREO, SEED)
}

/// One LSB is `2^-(bits - 1)`, from the format's step: 16 bit is 2^-15, 24 bit
/// is 2^-23, and a float format has none.
#[test]
fn the_lsb_is_the_step_of_the_integer_format() {
    assert_eq!(lsb_of::<i16>().get(), 1.0 / 32768.0);
    assert_eq!(lsb_of::<I24>().get(), 1.0 / 8_388_608.0);
    assert_eq!(lsb_of::<f32>(), Amplitude::SILENT);
}

/// A full-scale square, undithered into i16: `+1.0` clips to `MAX` and `-1.0`
/// is `MIN`, as the rule in `Sample` says, and the stage counts exactly the
/// values at or above `+1.0`, across chunks.
#[test]
fn a_full_scale_square_into_i16_is_max_or_min_and_counts_its_positive_peaks() {
    let frames = 3 * 1024 + 17;
    let square: Vec<f32> = (0..frames * 2)
        .map(|i| if (i / 2) % 8 < 4 { 1.0 } else { -1.0 })
        .collect();
    let positive = square.iter().filter(|&&s| s >= 1.0).count() as u64;
    let mut quantizer = Quantizer::<f32>::new(None, ChannelLayout::STEREO, SEED);
    let mut got = vec![0i16; square.len()];
    let clipped = quantizer.quantize(stereo(&square), stereo_mut(&mut got));
    assert_eq!(clipped, positive);
    for (out, s) in got.iter().zip(&square) {
        assert_eq!(*out, if *s >= 1.0 { 32767 } else { -32768 });
    }
}

/// `32767/32768` is the top step, not a clip; NaN reads as 0 and is not a clip.
#[test]
fn the_top_step_and_a_nan_are_not_clips() {
    let block = [32767.0 / 32768.0, f32::NAN, 0.0, -1.0];
    let mut quantizer = Quantizer::<f32>::new(None, ChannelLayout::STEREO, SEED);
    let mut got = [0i16; 4];
    assert_eq!(quantizer.quantize(stereo(&block), stereo_mut(&mut got)), 0);
    assert_eq!(got, [32767, 0, 0, -32768]);
}

/// TPDF is `r1 - r2`, within one LSB, so a silent block rounds to -1, 0 or 1:
/// `|r1 - r2| < 1/2` (probability `3/4`) rounds to 0, and each side past it
/// (probability `1/8`) to one step. So the nonzero fraction is `1/4`, with
/// standard deviation `sqrt(3 / (16 n))` over `n` samples, and the rounded
/// noise's variance is `1/4` in LSBs squared, so its mean is within six
/// standard deviations, `6 * sqrt(1 / (4 n))`, of 0. RPDF or a doubled LSB
/// fails the fraction or the bound.
#[test]
fn tpdf_on_silence_is_one_step_a_quarter_of_the_time_and_averages_to_zero() {
    let silence = vec![0.0f32; 4096 * 2];
    let mut quantizer = tpdf_i16();
    let mut got = Vec::new();
    for _ in 0..64 {
        let mut out = vec![0i16; silence.len()];
        quantizer.quantize(stereo(&silence), stereo_mut(&mut out));
        got.extend(out);
    }
    assert!(got.iter().all(|v| v.abs() <= 1), "past 1 LSB");
    let n = got.len() as f64;
    let nonzero = got.iter().filter(|&&v| v != 0).count() as f64 / n;
    let spread = 6.0 * (3.0 / (16.0 * n)).sqrt();
    assert!(
        (nonzero - 0.25).abs() <= spread,
        "nonzero fraction {nonzero}, not 1/4 within {spread}"
    );
    let mean = got.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let bound = 6.0 * (0.25 / n).sqrt();
    assert!(mean.abs() <= bound, "mean {mean} past {bound}");
}

/// A block longer than the scratch is dithered through to its last frame: every output
/// value of a silent block is a dithered one (at most 1 LSB), none left as it was.
#[test]
fn a_block_longer_than_the_scratch_is_quantized_to_its_last_frame() {
    for frames in [0usize, 1, 1023, 1024, 1025, 5000] {
        let mut out = vec![i16::MAX; frames * 2];
        tpdf_i16().quantize(stereo(&vec![0.0; frames * 2]), stereo_mut(&mut out));
        assert!(out.iter().all(|v| v.abs() <= 1), "frames {frames}");
    }
}

/// The noise does not depend on how the writes are cut: one block of N equals
/// pieces of any size, across chunk boundaries too.
#[test]
fn the_noise_continues_across_writes() {
    let input = signal(3000 * 2);
    let mut whole = vec![0i16; input.len()];
    tpdf_i16().quantize(stereo(&input), stereo_mut(&mut whole));
    let mut cut = vec![0i16; input.len()];
    let mut quantizer = tpdf_i16();
    let piece = 2 * 700 + 2;
    for (part, out) in input.chunks(piece).zip(cut.chunks_mut(piece)) {
        quantizer.quantize(stereo(part), stereo_mut(out));
    }
    assert_eq!(whole, cut);
}

/// An f64 signal runs the same stage into i24, whose full-scale step is 2^-23.
#[test]
fn an_f64_signal_converts_into_i24_by_the_rule() {
    let block = [0.5f64, -0.5, 1.0, -1.0];
    let mut quantizer = Quantizer::<f64>::new(None, ChannelLayout::STEREO, SEED);
    let mut got = [I24::SILENCE; 4];
    assert_eq!(quantizer.quantize(stereo(&block), stereo_mut(&mut got)), 1);
    let got: Vec<i32> = got.iter().map(|v| v.get()).collect();
    assert_eq!(got, [4_194_304, -4_194_304, 8_388_607, -8_388_608]);
}

/// A float target has no step: dither adds nothing and the conversion alone runs.
#[test]
fn a_float_target_gets_no_noise() {
    let input = signal(256);
    let mut got = vec![0.0f64; input.len()];
    tpdf_i16().quantize(stereo(&input), stereo_mut(&mut got));
    let want: Vec<f64> = input.iter().map(|&v| f64::from(v)).collect();
    assert_eq!(got, want);
}

/// Two seeds give two noise sequences; one seed gives one.
#[test]
fn a_seed_picks_the_noise() {
    let silence = vec![0.0f32; 4096 * 2];
    let run = |seed| {
        let mut out = vec![0i16; silence.len()];
        Quantizer::<f32>::new(Some(DitherShape::Triangular), ChannelLayout::STEREO, seed)
            .quantize(stereo(&silence), stereo_mut(&mut out));
        out
    };
    assert_eq!(run(1), run(1));
    assert_ne!(run(1), run(2));
}

/// A view of another width than the quantizer's is refused before anything is written.
#[test]
#[should_panic(expected = "layout")]
fn a_view_of_the_wrong_width_panics() {
    let mono = Interleaved::new(&[0.0f32; 8], ChannelLayout::MONO).unwrap();
    let mut out = [0i16; 8];
    let out = InterleavedMut::new(&mut out, ChannelLayout::MONO).unwrap();
    tpdf_i16().quantize(mono, out);
}

/// A full-scale overload (a constant 2.0, which clips every value) holds the loop's state to
/// the error bound, `2 LSB`, and a zero input after it recovers: once the history has flushed
/// (`taps` values), each error is a rounding error of at most half an LSB (no dither is added
/// here) and `y = e * NTF`, so `|y| <= (1 + sum |c_k|) / 2` LSB, never the overload's size.
#[test]
fn an_overload_keeps_the_state_bounded_and_the_loop_recovers() {
    let shaping = NoiseShaping::new(rate(44_100.0));
    let taps = shaping.order();
    let mut feedback = ErrorFeedback::<f64>::new(&shaping, 1);
    let mut out = vec![0i16; 4096];
    let run = |feedback: &mut ErrorFeedback<f64>, x: f64, out: &mut [i16]| {
        let xs = vec![x; out.len()];
        let view = || Interleaved::new(&xs, ChannelLayout::MONO).unwrap();
        feedback.process(
            AMP_LSB,
            view(),
            view(),
            InterleavedMut::new(out, ChannelLayout::MONO).unwrap(),
        );
    };
    run(&mut feedback, 2.0, &mut out);
    assert!(out.iter().all(|&y| y == i16::MAX));
    assert!(
        feedback.peak() <= 2.0 * LSB,
        "peak {}",
        feedback.peak() / LSB
    );
    // Silence: the first `taps` values still see the overload, the rest do not.
    run(&mut feedback, 0.0, &mut out);
    let gain: f64 = 1.0 + shaping.taps().iter().map(|c| c.abs()).sum::<f64>();
    let after = &out[taps..];
    let worst = after
        .iter()
        .map(|&y| f64::from(y).abs())
        .fold(0.0, f64::max);
    assert!(
        worst <= 0.5 * gain,
        "recovered peak {worst} vs {}",
        0.5 * gain
    );
    assert!(feedback.peak() <= 2.0 * LSB);
}

/// The output is the same bit for bit however the writes are cut: the history and the dither
/// source both continue across writes and chunk boundaries.
#[test]
fn the_shaped_output_does_not_depend_on_the_write_sizes() {
    let shaping = NoiseShaping::new(rate(48_000.0));
    let x = tone(5000, 48_000.0);
    let run = |piece: usize| {
        let mut quantizer =
            Quantizer::<f64>::new(Some(DitherShape::Triangular), ChannelLayout::MONO, SEED)
                .shaped(&shaping);
        let mut out = vec![0i16; x.len()];
        for (part, out) in x.chunks(piece).zip(out.chunks_mut(piece)) {
            quantizer.quantize(
                Interleaved::new(part, ChannelLayout::MONO).unwrap(),
                InterleavedMut::new(out, ChannelLayout::MONO).unwrap(),
            );
        }
        out
    };
    let whole = run(x.len());
    for piece in [1, 7, 700, 1024, 1025] {
        assert_eq!(whole, run(piece), "piece {piece}");
    }
}

/// Runs `feedback` over `x` (interleaved at `layout`, no dither) into an `i16` buffer.
fn run_layout(feedback: &mut ErrorFeedback<f64>, layout: ChannelLayout, x: &[f64]) -> Vec<i16> {
    let mut out = vec![0i16; x.len()];
    let view = || Interleaved::new(x, layout).unwrap();
    feedback.process(
        AMP_LSB,
        view(),
        view(),
        InterleavedMut::new(&mut out, layout).unwrap(),
    );
    out
}

/// A NaN, then an infinity, leave nothing in the history (a non-finite error is dropped),
/// and the finite signal after them is shaped as before: the history stays finite and within
/// `2 LSB`, and once it has flushed `|y - x| <= (1 + sum |c_k|) / 2` LSB (rounding error of at
/// most half an LSB, `y - x = e * NTF`).
#[test]
fn a_nan_and_an_infinity_do_not_latch_in_the_history() {
    let shaping = NoiseShaping::new(rate(44_100.0));
    let mut feedback = ErrorFeedback::<f64>::new(&shaping, 1);
    let mut x = vec![0.1; 8];
    x[2] = f64::NAN;
    x[5] = f64::INFINITY;
    run_layout(&mut feedback, ChannelLayout::MONO, &x);
    assert!(feedback.peak().is_finite() && feedback.peak() <= 2.0 * LSB);
    let finite = tone(4096, 44_100.0);
    let out = run_layout(&mut feedback, ChannelLayout::MONO, &finite);
    assert!(feedback.peak().is_finite() && feedback.peak() <= 2.0 * LSB);
    let gain = 1.0 + shaping.taps().iter().map(|c| c.abs()).sum::<f64>();
    for (&y, &x) in out.iter().zip(&finite).skip(shaping.order()) {
        let err = (f64::from(y) * LSB - x).abs() / LSB;
        assert!(err <= 0.5 * gain, "error {err} LSB past {}", 0.5 * gain);
    }
}

/// Stereo with the left channel overloaded and the right silent: each channel has its own
/// history, so the right channel stays at its rounding error of exactly zero (a zero input
/// quantizes to zero with nothing fed back) and the left holds the error bound. Then a
/// silent left channel recovers within the same per-channel bound as the mono case.
#[test]
fn each_channel_keeps_its_own_history() {
    let shaping = NoiseShaping::new(rate(44_100.0));
    let mut feedback = ErrorFeedback::<f64>::new(&shaping, 2);
    let loud: Vec<f64> = (0..4096).flat_map(|_| [2.0, 0.0]).collect();
    let out = run_layout(&mut feedback, ChannelLayout::STEREO, &loud);
    assert!(out.chunks(2).all(|f| f[0] == i16::MAX && f[1] == 0));
    assert_eq!(feedback.peak_of(1), 0.0);
    assert!(feedback.peak_of(0) <= 2.0 * LSB && feedback.peak_of(0) > 0.0);
    let quiet = vec![0.0; 2 * 4096];
    let out = run_layout(&mut feedback, ChannelLayout::STEREO, &quiet);
    let gain = 1.0 + shaping.taps().iter().map(|c| c.abs()).sum::<f64>();
    for f in out.chunks(2).skip(shaping.order()) {
        assert!(f64::from(f[0]).abs() <= 0.5 * gain);
        assert_eq!(f[1], 0);
    }
    assert_eq!(feedback.peak_of(1), 0.0);
}
