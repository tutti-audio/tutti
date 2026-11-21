use super::*;
use std::f64::consts::TAU;
use tutti_rt::SplitMix64;

/// The head's step for `ratio` in `2^-32` frames, rounded to nearest.
fn counts(ratio: f64) -> u64 {
    (ratio * 4_294_967_296.0 + 0.5) as u64
}

/// Runs `r` over `signals`, one per channel, as a ring-fed caller does: per block, peek
/// `input_needed` frames, process, pop what was consumed. Returns the output per channel
/// and the frames popped.
fn run<F: Float>(
    r: &mut VariableResampler<F>,
    signals: &[Vec<F>],
    blocks: &[(usize, f64)],
) -> (Vec<Vec<F>>, usize) {
    let mut out = vec![Vec::new(); signals.len()];
    let mut popped = 0;
    for &(frames, ratio) in blocks {
        let ratio = VariableRate::of(ratio);
        let needed = r.input_needed(Samples::new(frames), ratio).get();
        let input: Vec<&[F]> = signals
            .iter()
            .map(|s| &s[popped..popped + needed])
            .collect();
        let mut block = vec![vec![F::ZERO; frames]; signals.len()];
        let consumed = r
            .process(
                Planes::new(&input).unwrap(),
                PlanesMut::from_vecs(&mut block).unwrap(),
                ratio,
            )
            .unwrap()
            .get();
        assert!(consumed <= needed && needed <= consumed + 1);
        popped += consumed;
        for (o, b) in out.iter_mut().zip(block) {
            o.extend(b);
        }
    }
    (out, popped)
}

fn sine(f: f64, frames: usize) -> Vec<f64> {
    (0..frames).map(|n| (TAU * f * n as f64).sin()).collect()
}

/// Least-squares fit of `a cos + b sin` at `f` cycles per frame over `y`: the amplitude, and
/// the rms of what the fit leaves.
fn fit(y: &[f64], f: f64) -> (f64, f64) {
    let (mut cc, mut ss, mut cs, mut yc, mut ys) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (n, &v) in y.iter().enumerate() {
        let (s, c) = (TAU * f * n as f64).sin_cos();
        cc += c * c;
        ss += s * s;
        cs += c * s;
        yc += v * c;
        ys += v * s;
    }
    let det = cc * ss - cs * cs;
    let (a, b) = ((yc * ss - ys * cs) / det, (ys * cc - yc * cs) / det);
    let rest: f64 = y
        .iter()
        .enumerate()
        .map(|(n, &v)| {
            let (s, c) = (TAU * f * n as f64).sin_cos();
            (v - a * c - b * s).powi(2)
        })
        .sum();
    (a.hypot(b), (rest / y.len() as f64).sqrt())
}

fn noise(frames: usize, seed: u64) -> Vec<f64> {
    let mut rng = SplitMix64::new(seed);
    (0..frames)
        .map(|_| f64::from_fraction_bits(rng.next_u64()) * 2.0 - 1.0)
        .collect()
}

/// At a ratio of exactly 1 every read sits on an input frame, where the kernel is 1 and
/// every other tap is a sinc zero (`sin(pi m)` to the rounding of `pi m`, at most 1e-15 over
/// 63 taps): the output is the input delayed by the latency, to 1e-13 in `f64`. At `f32`
/// the zeros narrow to 0 or under 1e-15 and the sum is exact to the bound of one rounding.
/// The blocks are of unequal sizes, so the history crosses every call.
#[test]
fn at_unity_the_output_is_the_input_delayed_by_the_latency() {
    let blocks: Vec<(usize, f64)> = [1, 7, 64, 63, 65, 500, 3, 1000]
        .into_iter()
        .map(|n| (n, 1.0))
        .collect();
    let total: usize = blocks.iter().map(|b| b.0).sum();
    let x = noise(total + 64, 7);
    let latency = VariableResampler::<f64>::LATENCY.get();
    let (y, popped) = run(
        &mut VariableResampler::<f64>::new(ChannelLayout::MONO),
        std::slice::from_ref(&x),
        &blocks,
    );
    assert_eq!(popped, total);
    for (n, &v) in y[0].iter().enumerate() {
        let want = n.checked_sub(latency).map_or(0.0, |m| x[m]);
        assert!((v - want).abs() < 1e-13, "frame {n}: {v} vs {want}");
    }
    let x32: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    let (y32, _) = run(
        &mut VariableResampler::<f32>::new(ChannelLayout::MONO),
        std::slice::from_ref(&x32),
        &blocks,
    );
    for (n, &v) in y32[0].iter().enumerate() {
        let want = n.checked_sub(latency).map_or(0.0, |m| x32[m]);
        assert!((v - want).abs() < 1e-6, "f32 frame {n}: {v} vs {want}");
    }
}

/// The head after `n` output frames of a constant ratio is `n` steps of `round(r 2^32)`
/// counts, so the frames consumed in all are `floor(n r)` (the step's rounding moves it by
/// at most `n 2^-33` frames, 1e-4 here, against a distance to the next integer of 8e-4).
#[test]
fn the_frames_consumed_are_the_floor_of_frames_times_the_ratio() {
    let ratio = 1.000_05;
    let blocks: Vec<(usize, f64)> = std::iter::repeat_n((1000, ratio), 199)
        .chain([(983, ratio)])
        .collect();
    assert_eq!(blocks.iter().map(|b| b.0).sum::<usize>(), 199_983);
    let x = vec![0.0; 200_100];
    let (_, popped) = run(
        &mut VariableResampler::<f64>::new(ChannelLayout::MONO),
        &[x],
        &blocks,
    );
    assert_eq!(popped, 199_992);
}

/// A sine of `f` cycles per input frame, read at a ratio that changes every block, is the
/// sine at the head's position less the latency: `y[n] = sin(2 pi f (h[n] - 32))` with `h`
/// the head in frames, the sum of the quantised steps, once the head's window is clear of the signal's start. Two channels carry different
/// frequencies, so a plane is not read for another. The kernel's ripple (0.001 dB, 1.2e-4)
/// and its images (-100 dB, 1e-5) bound the error, with 1e-9 for the `f64` sums.
#[test]
fn a_sine_is_read_at_the_position_the_ratios_sum_to_less_the_latency() {
    let freqs = [0.4, 0.17];
    let ratios = [1.0, 1.000_05, 0.999_93, 1.0123, 0.9871, 1.05, 0.95];
    let blocks: Vec<(usize, f64)> = (0..14).map(|b| (300 + 17 * b, ratios[b % 7])).collect();
    let signals: Vec<Vec<f64>> = freqs.iter().map(|&f| sine(f, 7000)).collect();
    let (y, _) = run(
        &mut VariableResampler::<f64>::new(ChannelLayout::STEREO),
        &signals,
        &blocks,
    );
    let mut head = 0.0;
    let mut n = 0;
    for &(frames, ratio) in &blocks {
        for _ in 0..frames {
            // Until the kernel's whole window (32 frames each side) is past the start of the
            // signal, the read sees the silence before it.
            for (c, &f) in freqs.iter().enumerate().filter(|_| head >= 64.0) {
                let want = (TAU * f * (head - 32.0)).sin();
                assert!(
                    (y[c][n] - want).abs() < 1.3e-4 + 1e-9,
                    "channel {c}, frame {n}: {} vs {want}",
                    y[c][n]
                );
            }
            head += counts(ratio) as f64 / 4_294_967_296.0;
            n += 1;
        }
    }
}

/// Measured against the stated figures at a ratio that walks every phase: a sine at any
/// frequency up to the passband edge comes out within the ripple of unity, and what is not
/// the sine (its images, with the table's interpolation) is under the stopband figure.
#[test]
fn the_passband_ripple_and_the_stopband_attenuation_meet_the_figures() {
    let ratio = 1.0123;
    for edge_fraction in [0.05, 0.2, 0.4, 0.6, 0.7, 0.8, 0.85] {
        let f = edge_fraction / 2.0;
        let x = sine(f, 40_000);
        let (y, _) = run(
            &mut VariableResampler::<f64>::new(ChannelLayout::MONO),
            &[x],
            &[(20_000, ratio)],
        );
        // The sine at `f` per input frame is at `f * ratio` per output frame; skip the
        // start-up the zero history is.
        let (amplitude, rest) = fit(&y[0][200..], f * ratio);
        let ripple = (20.0 * amplitude.log10()).abs();
        let images = 20.0 * (rest * 2f64.sqrt()).log10();
        assert!(
            ripple <= VariableResampler::<f64>::PASSBAND_RIPPLE,
            "{edge_fraction}: {ripple} dB"
        );
        assert!(
            images <= -VariableResampler::<f64>::STOPBAND_ATTENUATION,
            "{edge_fraction}: images at {images} dB"
        );
    }
}

/// A component past the edge is not within the figures: at 0.95 of Nyquist the gain is
/// 0.63 dB down and the images 23 dB below, so the edge is not a loose bound.
#[test]
fn a_component_past_the_edge_is_outside_the_figures() {
    let x = sine(0.475, 40_000);
    let (y, _) = run(
        &mut VariableResampler::<f64>::new(ChannelLayout::MONO),
        &[x],
        &[(20_000, 1.0123)],
    );
    let (amplitude, rest) = fit(&y[0][200..], 0.475 * 1.0123);
    assert!((20.0 * amplitude.log10()).abs() > 10.0 * VariableResampler::<f64>::PASSBAND_RIPPLE);
    assert!(20.0 * (rest * 2f64.sqrt()).log10() > -VariableResampler::<f64>::STOPBAND_ATTENUATION);
}

/// The ratio is a [`VariableRate`]: `0.95..=1.05`, refusing NaN and everything outside it. At
/// unity `n` output frames take `n` input; at the top, 999 steps of 1.05 from 0 are 1048.95, so
/// the last window starts at 1048 (1049 frames) and the head after 1000 steps is 1050.
#[test]
fn the_ratio_is_range_checked_and_sets_the_input_needed() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: VariableRate::new,
        get: f64::from,
        of: Some(VariableRate::of),
        valid: &[0.95, 1.0, 1.000_05, 1.05],
        invalid: &[0.949_999, 1.050_001, 0.0, 2.0, f64::NAN, f64::INFINITY],
    });
    assert_eq!(VariableRate::MIN.get(), 0.95);
    assert_eq!(VariableRate::MAX.get(), 1.05);
    let r = VariableResampler::<f32>::new(ChannelLayout::MONO);
    let n = Samples::new(1000);
    assert_eq!(r.input_needed(n, VariableRate::UNITY), n);
    assert_eq!(r.input_needed(n, VariableRate::MAX), Samples::new(1050));
}

/// A view of another width, or an input short of what `input_needed` says, is refused and
/// changes nothing: the call that follows reads what it would have read.
#[test]
fn a_refused_call_changes_nothing() {
    let mut r = VariableResampler::<f32>::new(ChannelLayout::STEREO);
    let one = [0.0f32; 300];
    let mut out = [0.0f32; 100];
    let mono = [&one[..]];
    let err = r
        .process(
            Planes::new(&mono).unwrap(),
            PlanesMut::new(&mut [&mut out[..]]).unwrap(),
            VariableRate::UNITY,
        )
        .unwrap_err();
    assert_eq!(
        err,
        ResampleError::WidthMismatch {
            expected: 2,
            got: 1
        }
    );
    let two = [&one[..99], &one[..99]];
    let (mut a, mut b) = ([0.0f32; 100], [0.0f32; 100]);
    let err = r
        .process(
            Planes::new(&two).unwrap(),
            PlanesMut::new(&mut [&mut a[..], &mut b[..]]).unwrap(),
            VariableRate::UNITY,
        )
        .unwrap_err();
    assert_eq!(
        err,
        ResampleError::BufferMismatch {
            channels: 2,
            frames_in: 100,
            frames_out: 100
        }
    );
    assert_eq!(
        r.input_needed(Samples::new(100), VariableRate::UNITY),
        Samples::new(100)
    );
}

/// The table is `PHASES` rows of weights and differences, each row's weights summing to 1 so
/// a constant keeps its level at every phase.
#[test]
fn a_constant_keeps_its_level_at_every_phase() {
    let r = VariableResampler::<f64>::new(ChannelLayout::MONO);
    for row in r.table.chunks(2 * TAPS) {
        let sum: f64 = row[..TAPS].iter().sum();
        assert!((sum - 1.0).abs() < 1e-14, "{sum}");
    }
    assert_eq!(r.table.len(), PHASES * 2 * TAPS);
}
