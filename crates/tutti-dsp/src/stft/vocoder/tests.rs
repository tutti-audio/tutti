use super::*;
use crate::CosineWindow;
use crate::Window;
use crate::fft::Complex64;
use core::f64::consts::TAU;
use tutti_units::{Hz, SampleRate};

const RATE: f64 = 48_000.0;
const WINDOW: usize = 2048;
const HOP: usize = WINDOW / 4;

fn geometry() -> ColaGeometry {
    let geometry = StftGeometry::new(
        SampleRate::SR_48K,
        crate::WindowLength::new(Samples::new(WINDOW)).unwrap(),
        crate::HopSize::new(Samples::new(HOP)).unwrap(),
    );
    ColaGeometry::try_from(geometry).unwrap()
}

fn vocoder<F: FftFloat>(width: ChannelLayout) -> PhaseVocoder<F> {
    PhaseVocoder::new(width, geometry())
}

fn controls(stretch: f64, cents: f64) -> PhaseVocoderControls {
    PhaseVocoderControls {
        stretch: StretchFactor::new(stretch),
        pitch: Cents::new(cents),
    }
}

/// `amplitude * sin(2 pi f n / RATE + phase)` for `len` samples.
fn sine<F: FftFloat>(f: f64, amplitude: f64, phase: f64, len: usize) -> Vec<F> {
    (0..len)
        .map(|n| F::from_design(amplitude * (TAU * f * n as f64 / RATE + phase).sin()))
        .collect()
}

/// Streams `source` through `vocoder` in output blocks of `block`, feeding exactly what
/// [`PhaseVocoder::input_needed`] asks (silence past the end of the source) until the source is
/// consumed. `each` runs before every block with its index. Asserts that no block starves.
fn drive<F: FftFloat>(
    vocoder: &mut PhaseVocoder<F>,
    source: &[Vec<F>],
    block: usize,
    mut each: impl FnMut(usize, &mut PhaseVocoder<F>),
) -> Vec<Vec<F>> {
    let mut out = vec![Vec::new(); source.len()];
    let mut at = 0;
    let mut index = 0;
    while at < source[0].len() {
        each(index, vocoder);
        let needed = vocoder.input_needed(Samples::new(block)).get();
        let input: Vec<Vec<F>> = source
            .iter()
            .map(|c| {
                (at..at + needed)
                    .map(|i| c.get(i).copied().unwrap_or(F::ZERO))
                    .collect()
            })
            .collect();
        let mut block_out = vec![vec![F::ZERO; block]; source.len()];
        let counts = vocoder.process(
            Planes::from_vecs(&input).unwrap(),
            PlanesMut::from_vecs(&mut block_out).unwrap(),
        );
        assert_eq!(counts.produced.get(), block, "block {index} starved");
        assert_eq!(
            counts.consumed.get(),
            needed,
            "block {index} took other than asked"
        );
        at += needed;
        index += 1;
        for (o, b) in out.iter_mut().zip(block_out) {
            o.extend(b);
        }
    }
    out
}

fn f64s<F: FftFloat>(x: &[F]) -> Vec<f64> {
    x.iter().map(|&v| v.into()).collect()
}

/// The Hann-windowed DFT of `x` at exactly `f` Hz.
fn tone(x: &[f64], f: f64) -> Complex64 {
    let (re, im) = tutti_fixtures::signal::hann_dft(x, Hz::new(f), SampleRate::SR_48K);
    Complex64::new(re, im)
}

/// The frequency of the sine in `x`, to within `measure` samples of it, by the phase advance
/// of its exact-bin DFT between two segments 4096 samples apart.
fn frequency(x: &[f64], near: f64, measure: usize) -> f64 {
    tutti_fixtures::signal::sine_frequency(
        x,
        Hz::new(near),
        SampleRate::SR_48K,
        Samples::new(measure),
        Samples::new(4096),
    )
    .get()
}

/// The power outside the fundamental's main lobe over the power inside it, in dB, of the
/// 4-term Blackman-Harris spectrum of `x` (92 dB sidelobes, a main lobe 4 bins either side).
fn impurity_db(x: &[f64]) -> f64 {
    let m = x.len();
    let window = CosineWindow::BLACKMAN_HARRIS.coefficients(m);
    let plan = RealFft::<f64>::new(m);
    let mut spectrum = vec![Complex64::new(0.0, 0.0); plan.bins()];
    let windowed: Vec<f64> = x.iter().zip(&window).map(|(&v, &w)| v * w).collect();
    plan.forward(Mono::new(&windowed), &mut spectrum, &mut plan.new_scratch());
    let power: Vec<f64> = spectrum.iter().map(|c| c.norm_sqr()).collect();
    let peak = (1..power.len())
        .max_by(|&a, &b| power[a].total_cmp(&power[b]))
        .unwrap();
    let (lo, hi) = (peak.saturating_sub(5), (peak + 5).min(power.len() - 1));
    let inside: f64 = power[lo..=hi].iter().sum();
    let outside: f64 = power.iter().sum::<f64>() - inside;
    10.0 * (outside / inside).log10()
}

/// An FFT roundtrip's relative error in `f32`: each transform grows rounding like
/// `eps * log2(window)`, and a frame is one forward and one inverse.
fn f32_transform_error() -> f64 {
    2.0 * (WINDOW as f64).log2() * f64::from(f32::EPSILON)
}

/// The leakage of a Hann window of `length` samples at the image of a tone at `f`.
fn hann_image(f: f64, length: usize) -> f64 {
    tutti_fixtures::signal::hann_image_leak(Hz::new(f), Samples::new(length), SampleRate::SR_48K)
}

/// Feeds `source` as the graph does: `block` input frames per `block` output frames, whatever
/// the vocoder needs, and asserts every block is complete and taken whole.
fn drive_blocks<F: FftFloat>(
    vocoder: &mut PhaseVocoder<F>,
    source: &[Vec<F>],
    block: usize,
) -> Vec<Vec<F>> {
    let mut out = vec![Vec::new(); source.len()];
    for start in (0..source[0].len()).step_by(block) {
        let end = (start + block).min(source[0].len());
        let input: Vec<Vec<F>> = source.iter().map(|c| c[start..end].to_vec()).collect();
        let mut block_out = vec![vec![F::ZERO; end - start]; source.len()];
        let counts = vocoder.process(
            Planes::from_vecs(&input).unwrap(),
            PlanesMut::from_vecs(&mut block_out).unwrap(),
        );
        assert_eq!(
            counts.produced.get(),
            end - start,
            "block at {start} starved"
        );
        assert_eq!(
            counts.consumed.get(),
            end - start,
            "block at {start} left input behind"
        );
        for (o, b) in out.iter_mut().zip(block_out) {
            o.extend(b);
        }
    }
    out
}

/// At unity the vocoder is a delay by `latency()`: a Hann-squared COLA frame sum of untouched
/// spectra reproduces the signal, so `out[n + latency] = in[n]` to the transform's rounding,
/// at `f64` here (`1e-12` is `log2(2048) * 2^-53` with four orders of margin).
#[test]
fn unity_is_the_input_delayed_by_the_latency() {
    let input = sine::<f64>(1234.5, 0.5, 0.3, 30_000);
    let mut v = vocoder::<f64>(ChannelLayout::MONO);
    let out = drive(&mut v, std::slice::from_ref(&input), 300, |_, _| {});
    let delay = v.latency().get();
    assert_eq!(delay, WINDOW + 1);
    for n in 0..20_000 {
        assert!((out[0][n + delay] - input[n]).abs() < 1e-12, "sample {n}");
    }
}

/// A steady sine comes out at its input amplitude: the COLA window sum is exact, so the only
/// error is the `f32` transforms' rounding (`2 log2(window)` epsilons, relative).
#[test]
fn unity_keeps_a_steady_sine_at_its_amplitude() {
    let mut v = vocoder::<f32>(ChannelLayout::MONO);
    let out = drive(
        &mut v,
        &[sine::<f32>(1000.0, 0.5, 0.0, 60_000)],
        512,
        |_, _| {},
    );
    let x = f64s(&out[0]);
    let amplitude = tone(&x[8192..8192 + 16384], 1000.0).norm() / (16384.0 / 4.0);
    assert!(
        (amplitude / 0.5 - 1.0).abs() < f32_transform_error(),
        "{amplitude}"
    );
}

/// An impulse at unity comes out as an impulse at exactly `latency()` frames later, and
/// nowhere else, in the blocks a graph effect sees: input and output in step, any size.
#[test]
fn an_impulse_comes_out_centred_at_the_latency_in_any_block_size() {
    for block in [1, 97, 512, 2048, 5000] {
        let mut impulse = vec![0.0f32; 12_000];
        impulse[4000] = 1.0;
        let mut v = vocoder::<f32>(ChannelLayout::MONO);
        let out = drive_blocks(&mut v, &[impulse], block);
        let at = 4000 + v.latency().get();
        // Four overlapped frames, each in error by the transforms' rounding.
        let bound = 4.0 * f32_transform_error();
        for (n, &y) in out[0].iter().enumerate() {
            let want = if n == at { 1.0 } else { 0.0 };
            assert!(
                (f64::from(y) - want).abs() < bound,
                "block {block}, sample {n}: {y}"
            );
        }
    }
}

/// `N` input frames yield `N r` output frames, to within a hop: the frames step through the
/// input by `hop / r`, so the frames finished by the time the input is spent differ from
/// `N r` by the frame in flight.
#[test]
fn n_input_frames_yield_n_times_the_ratio_in_output_frames() {
    let n = 48_000;
    for ratio in [0.5, 0.8, 1.0, 1.5, 2.0, 4.0] {
        let mut v = vocoder::<f32>(ChannelLayout::MONO);
        v.retarget(&controls(ratio, 0.0));
        let out = drive(&mut v, &[sine::<f32>(440.0, 0.5, 0.0, n)], 256, |_, _| {});
        let got = out[0].len() as f64;
        assert!(
            (got - n as f64 * ratio).abs() <= HOP as f64 + 256.0,
            "ratio {ratio}: {got}"
        );
    }
}

/// A sine at `f` through `cents` comes out at `f 2^(cents/1200)`, by the phase advance of its
/// exact-bin DFT between two segments 4096 frames apart. The estimator's only systematic error
/// is the window's leakage of the tone's negative-frequency image (`hann_image`), once in the
/// measurement and once in the vocoder's own frames, each a phase error of at most that many
/// radians on two frames: `2 eps` rad over the step, as a frequency `2 eps fs / (2 pi step)`.
#[test]
fn a_shift_lands_at_the_input_times_two_to_the_cents_over_1200() {
    const MEASURE: usize = 16_384;
    for (f, cents, stretch) in [
        (1000.0, 1200.0, 1.0),
        (1000.0, -700.0, 1.0),
        (1000.0, 50.0, 1.0),
        (440.0, 50.0, 1.5),
        (1500.0, -1200.0, 0.8),
    ] {
        let mut v = vocoder::<f64>(ChannelLayout::MONO);
        v.retarget(&controls(stretch, cents));
        let out = drive(&mut v, &[sine::<f64>(f, 0.5, 0.0, 60_000)], 256, |_, _| {});
        let want = f * (cents / 1200.0f64).exp2();
        let got = frequency(&out[0][12_288..], want, MEASURE);
        let bound = 2.0
            * (hann_image(f, WINDOW) * RATE / (TAU * HOP as f64)
                + hann_image(want, MEASURE) * RATE / (TAU * 4096.0));
        assert!(
            (got - want).abs() < bound,
            "{f} Hz {cents} cents: {got} for {want} (bound {bound})"
        );
    }
}

/// The cubic read's position is the step times the frames read, one product, whatever the
/// output was split into: a stretched and shifted stream (700 cents, no dyadic ratio) in output
/// blocks of 64 and of 512 is the same bit for bit, pulled and at a fixed rate.
#[test]
fn a_shifted_stream_is_bit_identical_in_any_output_blocks() {
    let source = [sine::<f32>(440.0, 0.5, 0.0, 20_000)];
    let pulled = |block| {
        let mut v = vocoder::<f32>(ChannelLayout::MONO);
        v.retarget(&controls(1.5, 700.0));
        drive(&mut v, &source, block, |_, _| {})
    };
    let fixed = |block| {
        let mut v = PhaseVocoder::<f32>::fixed_rate(ChannelLayout::MONO, geometry());
        v.retarget(&controls(1.0, 700.0));
        drive_blocks(&mut v, &source, block)
    };
    for (name, small, large) in [
        ("pulled", pulled(64), pulled(512)),
        ("fixed rate", fixed(64), fixed(512)),
    ] {
        let len = small[0].len().min(large[0].len());
        assert!(len > 10_000, "a long enough stream to compare: {len}");
        for n in 0..len {
            assert_eq!(
                small[0][n].to_bits(),
                large[0][n].to_bits(),
                "{name}, frame {n}: {} in 64s, {} in 512s",
                small[0][n],
                large[0][n]
            );
        }
    }
}

/// The source frames consumed per output frame over a long run is `read_rate()`: `1 / stretch`
/// as clamped, whatever the pitch, the extremes of both included. The frames in flight put a
/// window and a block between the run's counts and the rate, a small part of the 96 000 frames.
#[test]
fn the_read_rate_is_the_consumption_measured_over_a_long_run() {
    let pairs = [
        (1.0, 0.0),
        (1.5, 700.0),
        (0.8, -300.0),
        (0.25, -2400.0),
        (0.25, 2400.0),
        (4.0, 2400.0),
        (4.0, -2400.0),
        (f64::NAN, 100.0),
    ];
    for (stretch, cents) in pairs {
        let mut v = vocoder::<f32>(ChannelLayout::MONO);
        v.retarget(&controls(stretch, cents));
        let rate = v.read_rate().get();
        let expected = 1.0 / StretchFactor::new(stretch).get().clamp(0.25, 4.0);
        let expected = if stretch.is_nan() { 1.0 } else { expected };
        assert_eq!(rate, expected, "{stretch}, {cents}");
        let produced = 96_000;
        let mut fed = 0;
        let mut made = 0;
        while made < produced {
            let needed = v.input_needed(Samples::new(256)).get();
            let input = vec![vec![0.1f32; needed]];
            let mut out = vec![vec![0.0f32; 256]];
            let counts = v.process(
                Planes::from_vecs(&input).unwrap(),
                PlanesMut::from_vecs(&mut out).unwrap(),
            );
            fed += counts.consumed.get();
            made += counts.produced.get();
        }
        let measured = fed as f64 / made as f64;
        let slack = (WINDOW + 4 * 256) as f64 / made as f64;
        assert!(
            (measured - rate).abs() <= slack * rate.max(1.0),
            "{stretch}, {cents}: consumed {measured} per frame, read_rate {rate}"
        );
    }
}

/// A non-finite input sample does not latch: the output is finite again once the frames that
/// held it have gone out, with no reset between, whatever the controls.
///
/// The sample is input frame 5000 of a stream that starts `latency` frames in; the frames
/// that hold it end by 5000 + latency + window of the input, and the output is the input
/// stretched by up to 2, so the output is clear by `2 (5000 + 2 window) + latency`.
#[test]
fn a_non_finite_input_sample_does_not_latch() {
    for (stretch, cents) in [(1.0, 0.0), (1.5, 0.0), (0.8, 300.0), (2.0, -500.0)] {
        for value in [f32::NAN, f32::INFINITY] {
            let mut input = sine::<f32>(1000.0, 0.5, 0.0, 40_000);
            input[5000] = value;
            let mut v = vocoder::<f32>(ChannelLayout::STEREO);
            v.retarget(&controls(stretch, cents));
            let out = drive(&mut v, &[input.clone(), input], 256, |_, _| {});
            assert!(
                out[0].iter().any(|x| !x.is_finite()),
                "{stretch}, {cents}: the sample reaches the output"
            );
            let clear = 2 * (5000 + 2 * WINDOW) + v.latency().get();
            for channel in &out {
                assert!(
                    channel[clear..].iter().all(|x| x.is_finite()),
                    "{stretch}, {cents}, {value}: non-finite output past {clear}"
                );
            }
        }
    }
}

/// A sine through a non-integer stretch stays a sine: the power outside the fundamental's lobe,
/// over the power inside it, is as low as the analyser can resolve. The analyser's own floor
/// is that of the exact input sine (its Blackman-Harris window's sidelobes, about -87 dB here);
/// the output may sit 3 dB above it. The old intake, dropping and repeating whole samples,
/// reached -19 dB at 1.5 and -9 dB at 0.5.
#[test]
fn a_stretched_sine_is_as_pure_as_the_analyser_can_tell() {
    let f = 4999.3;
    let floor = impurity_db(&f64s(&sine::<f32>(f, 0.5, 0.0, 16_384)));
    assert!(floor < -80.0, "{floor}");
    for ratio in [0.5, 1.5, 2.0] {
        let mut v = vocoder::<f32>(ChannelLayout::MONO);
        v.retarget(&controls(ratio, 0.0));
        let out = drive(&mut v, &[sine::<f32>(f, 0.5, 0.0, 96_000)], 256, |_, _| {});
        let impurity = impurity_db(&f64s(&out[0][8192..8192 + 16_384]));
        assert!(
            impurity < floor + 3.0,
            "ratio {ratio}: {impurity} dB over a floor of {floor}"
        );
    }
}

/// The two channels carry one phase solution: equal channels come out equal and opposite
/// channels opposite, bit for bit, through a stretch and a shift.
#[test]
fn equal_and_opposite_channels_stay_so_bit_for_bit() {
    let one = sine::<f32>(1234.5, 0.5, 0.0, 30_000);
    let other = sine::<f32>(2468.0, 0.25, 1.0, 30_000);
    let mix: Vec<f32> = one.iter().zip(&other).map(|(a, b)| a + b).collect();
    let negated: Vec<f32> = mix.iter().map(|&x| -x).collect();
    for (stretch, cents) in [(1.5, 0.0), (0.8, 300.0), (1.0, -500.0)] {
        let mut v = vocoder::<f32>(ChannelLayout::STEREO);
        v.retarget(&controls(stretch, cents));
        let out = drive(&mut v, &[mix.clone(), mix.clone()], 256, |_, _| {});
        assert!(out[0].iter().any(|&x| x != 0.0));
        assert_eq!(out[0], out[1], "L = R at {stretch}, {cents}");
        let mut v = vocoder::<f32>(ChannelLayout::STEREO);
        v.retarget(&controls(stretch, cents));
        let out = drive(&mut v, &[mix.clone(), negated.clone()], 256, |_, _| {});
        let flipped: Vec<f32> = out[1].iter().map(|&x| -x).collect();
        assert_eq!(out[0], flipped, "L = -R at {stretch}, {cents}");
    }
}

/// Each partial's inter-channel phase offset survives a 1.5 stretch: the rotation is common to
/// the channels, so only rounding is left. The exact-bin DFT's cross-leakage between partials
/// 756 bins apart is `hann_image`-small (under `1e-9`), so the bound is the `f32` transforms'.
#[test]
fn a_stretch_keeps_each_partials_inter_channel_phase() {
    const MEASURE: usize = 16_384;
    let partials = [(1000.3, 0.6), (3217.9, -1.1)];
    let channel = |offsets: [f64; 2]| -> Vec<f32> {
        let a = sine::<f32>(partials[0].0, 0.5, offsets[0], 60_000);
        let b = sine::<f32>(partials[1].0, 0.25, offsets[1], 60_000);
        a.iter().zip(&b).map(|(x, y)| x + y).collect()
    };
    let mut v = vocoder::<f32>(ChannelLayout::STEREO);
    v.retarget(&controls(1.5, 0.0));
    let out = drive(
        &mut v,
        &[channel([0.0, 0.0]), channel([partials[0].1, partials[1].1])],
        256,
        |_, _| {},
    );
    let (left, right) = (f64s(&out[0]), f64s(&out[1]));
    for (f, offset) in partials {
        let at = 20_000;
        let measured =
            (tone(&right[at..at + MEASURE], f) * tone(&left[at..at + MEASURE], f).conj()).arg();
        let error =
            (measured - offset + core::f64::consts::PI).rem_euclid(TAU) - core::f64::consts::PI;
        assert!(
            error.abs() < f32_transform_error(),
            "{f} Hz: {measured} for {offset}"
        );
    }
}

/// The output level of a steady sine across a change of the controls: the blocks of 128
/// frames (four periods of 1500 Hz, so a block's RMS is exact) stay within 1 dB of the sine's.
/// 1 dB is two overlapped frames' phase mismatch of `acos(10^(-1/20)) = 0.48` rad; the flush
/// the old unit did at a step cost 11.6 dB.
fn assert_level_holds(first: PhaseVocoderControls, second: PhaseVocoderControls) {
    let mut v = vocoder::<f32>(ChannelLayout::MONO);
    v.retarget(&first);
    let out = drive(
        &mut v,
        &[sine::<f32>(1500.0, 0.5, 0.0, 60_000)],
        256,
        |i, v| {
            if i == 80 {
                v.retarget(&second);
            }
        },
    );
    let steady = 0.5 / 2.0f64.sqrt();
    for (b, block) in out[0][6000..].as_chunks::<128>().0.iter().enumerate() {
        let rms = (block.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / 128.0).sqrt();
        let db = 20.0 * (rms / steady).log10();
        assert!(db.abs() < 1.0, "block {b} after 6000 is {db} dB");
    }
}

#[test]
fn a_ratio_step_leaves_no_hole() {
    assert_level_holds(controls(0.8, 0.0), controls(1.0667, 0.0));
    assert_level_holds(controls(1.0, 0.0), controls(1.05, 0.0));
}

#[test]
fn a_pitch_step_leaves_no_hole() {
    assert_level_holds(controls(1.0, 0.0), controls(1.0, 300.0));
    assert_level_holds(controls(1.5, -200.0), controls(1.2, 100.0));
}

/// Feeding exactly what `input_needed` says fills every block, whatever the controls do
/// between blocks, the extremes included; the call and the fill agree.
#[test]
fn input_needed_fills_every_block_under_changing_controls() {
    let steps = [
        controls(1.0, 0.0),
        controls(0.25, -2400.0),
        controls(4.0, 2400.0),
        controls(0.25, 2400.0),
        controls(4.0, -2400.0),
        controls(1.3, 17.0),
        controls(f64::NAN, f64::NAN),
        controls(0.7, 700.0),
    ];
    let mut v = vocoder::<f32>(ChannelLayout::STEREO);
    let source = sine::<f32>(700.0, 0.5, 0.0, 80_000);
    drive(&mut v, &[source.clone(), source], 333, |i, v| {
        v.retarget(&steps[i / 7 % steps.len()]);
    });
}

/// A control that is not a number reads as unity, so it asks the input for what unity asks.
#[test]
fn a_non_finite_control_reads_as_unity() {
    let mut v = vocoder::<f32>(ChannelLayout::MONO);
    let at_unity = v.input_needed(Samples::new(5000));
    v.retarget(&PhaseVocoderControls {
        stretch: StretchFactor::new(f64::NAN),
        pitch: Cents::new(f64::INFINITY),
    });
    assert_eq!(v.input_needed(Samples::new(5000)), at_unity);
}

/// A reset forgets the stream: silence in gives silence out, the loud tail of before gone.
#[test]
fn a_reset_leaves_silence() {
    let mut v = vocoder::<f32>(ChannelLayout::MONO);
    drive(
        &mut v,
        &[sine::<f32>(900.0, 0.9, 0.0, 20_000)],
        256,
        |_, _| {},
    );
    v.reset();
    let out = drive_blocks(&mut v, &[vec![0.0f32; 8192]], 256);
    assert!(out[0].iter().all(|&x| x == 0.0));
}

/// In fixed-rate mode every call takes `n` input frames and gives `n` output frames, for any
/// `n` and the pitch anywhere in range, however it jumps between calls: it is changed at every
/// call, the extremes alternating with a sweep across the range in steps of 337 cents.
#[test]
fn fixed_rate_takes_n_and_gives_n_under_any_pitch_at_any_block_size() {
    for block in [1, 64, 480, 5000] {
        let mut v = PhaseVocoder::<f32>::fixed_rate(ChannelLayout::MONO, geometry());
        let source = sine::<f32>(1500.0, 0.5, 0.0, 120_000);
        for (i, start) in (0..source.len() - block).step_by(block).enumerate() {
            let cents = match i % 4 {
                0 => -2400.0 + ((i * 337) % 4801) as f64,
                1 => -2400.0,
                2 => 2400.0 - ((i * 337) % 4801) as f64,
                _ => 2400.0,
            };
            v.retarget(&controls(1.0, cents));
            let input = vec![source[start..start + block].to_vec()];
            let mut out = vec![vec![0.0f32; block]];
            let counts = v.process(
                Planes::from_vecs(&input).unwrap(),
                PlanesMut::from_vecs(&mut out).unwrap(),
            );
            assert_eq!(
                (counts.produced.get(), counts.consumed.get()),
                (block, block),
                "block {block}, call {i} at {cents} cents"
            );
        }
    }
}

/// The fixed-rate latency is the window, a frame, and the lead: one frame of step, the cubic
/// read's reach and a frame of rounding at the lowest ratio of 1/4.
#[test]
fn fixed_rate_latency_is_the_window_and_the_lead() {
    let v = PhaseVocoder::<f32>::fixed_rate(ChannelLayout::MONO, geometry());
    assert_eq!(v.latency().get(), WINDOW + 1 + 4 * HOP + 12 + 1);
}

/// The stretch of a fixed-rate vocoder is held at unity.
#[test]
fn fixed_rate_ignores_the_stretch() {
    let mut v = PhaseVocoder::<f32>::fixed_rate(ChannelLayout::MONO, geometry());
    let at_unity = v.input_needed(Samples::new(5000));
    v.retarget(&controls(2.0, 0.0));
    assert_eq!(v.input_needed(Samples::new(5000)), at_unity);
}

/// Streams `source` (as many frames as it holds) through a seated vocoder at a stretch and a
/// shift, `discard` taking the first analysis frame without synthesis and `process` taking it
/// in full; both then go on in blocks of 256 output frames. The output from the discarded
/// frame on.
fn seated_run<F: FftFloat>(source: &[Vec<F>], discard: bool) -> (Vec<Vec<F>>, Samples) {
    let mut v = vocoder::<F>(ChannelLayout::STEREO);
    v.seat();
    v.retarget(&controls(1.3, 100.0));
    let mut out = vec![Vec::new(); 2];
    let mut at = 0;
    // The window of history the first frame analyses.
    let history = [source[0][..WINDOW].to_vec(), source[1][..WINDOW].to_vec()];
    let first = Planes::from_vecs(&history).unwrap();
    let counts = if discard {
        v.discard(first, Samples::new(1))
    } else {
        let mut sink = vec![vec![F::ZERO; 1]; 2];
        v.process(first, PlanesMut::from_vecs(&mut sink).unwrap())
    };
    assert_eq!(counts.produced.get(), 1);
    at += counts.consumed.get();
    for channel in &mut out {
        channel.push(F::ZERO);
    }
    let mut consumed = counts.consumed;
    while at + 4096 < source[0].len() {
        let needed = v.input_needed(Samples::new(256)).get();
        let input: Vec<Vec<F>> = source.iter().map(|c| c[at..at + needed].to_vec()).collect();
        let mut block = vec![vec![F::ZERO; 256]; 2];
        let counts = v.process(
            Planes::from_vecs(&input).unwrap(),
            PlanesMut::from_vecs(&mut block).unwrap(),
        );
        assert_eq!(counts.produced.get(), 256);
        at += counts.consumed.get();
        consumed = Samples::new(consumed.get() + counts.consumed.get());
        for (o, b) in out.iter_mut().zip(block) {
            o.extend(b);
        }
    }
    (out, consumed)
}

fn two_channels<F: FftFloat>(len: usize) -> Vec<Vec<F>> {
    vec![
        sine::<F>(1234.5, 0.5, 0.3, len),
        sine::<F>(2345.6, 0.25, 1.1, len),
    ]
}

/// The phase history a discarded frame leaves is the one the full path leaves: the stream's
/// output from the end of the ramp on is bit for bit the same, which it is only if every phase
/// accumulator and every cursor agrees (a stretch of 1.3 and a shift of 100 cents make each
/// frame depend on the last), and the frames taken are the same. Inside the ramp the discard
/// differs, so the test can tell it from a full frame.
fn a_discarded_frame_leaves_the_phases_of_the_full_path<F: FftFloat>() {
    let source = two_channels::<F>(40_000);
    let (full, taken) = seated_run(&source, false);
    let (cheap, cheap_taken) = seated_run(&source, true);
    assert_eq!(taken, cheap_taken);
    for c in 0..2 {
        assert!(
            full[c][1..WINDOW] != cheap[c][1..WINDOW],
            "channel {c}: the ramp holds the synthesis the discard skipped"
        );
        assert!(
            full[c][WINDOW..] == cheap[c][WINDOW..],
            "channel {c}: steady output differs after the discard"
        );
    }
}

#[test]
fn a_discarded_frame_leaves_the_phases_of_the_full_path_f32() {
    a_discarded_frame_leaves_the_phases_of_the_full_path::<f32>();
}

#[test]
fn a_discarded_frame_leaves_the_phases_of_the_full_path_f64() {
    a_discarded_frame_leaves_the_phases_of_the_full_path::<f64>();
}

/// A seated stream is the history at unity from the end of the ramp on: output frame `k` is
/// input frame `k` (`1e-12` as in the latency test), with no silence ahead of the history.
#[test]
fn a_seated_stream_is_the_history_itself_after_its_ramp() {
    let input = sine::<f64>(1234.5, 0.5, 0.3, 30_000);
    let mut v = vocoder::<f64>(ChannelLayout::MONO);
    v.seat();
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4096 < input.len() {
        let needed = v.input_needed(Samples::new(300)).get();
        let mut block = vec![0.0; 300];
        let counts = v.process(
            Planes::new(&[&input[at..at + needed]]).unwrap(),
            PlanesMut::new(&mut [&mut block]).unwrap(),
        );
        assert_eq!(counts.produced.get(), 300);
        at += counts.consumed.get();
        out.extend(block);
    }
    for k in WINDOW..20_000 {
        assert!((out[k] - input[k]).abs() < 1e-12, "frame {k}");
    }
    assert!(
        out[..HOP]
            .iter()
            .zip(&input)
            .any(|(y, x)| (y - x).abs() > 1e-3),
        "the first frames are a ramp"
    );
}

/// A discard mid-stream drops the overlap-add of the hop it skips: after processing a steady
/// sine, discarding one frame, and processing past the energy ring's length, the output from
/// one window after the discard on is the full path's (a vocoder that did not discard, at the
/// same input offset) to the unity test's `1e-12`. A skipped hop whose slots kept the earlier
/// frames' sum and energy would add them into the hop the ring reuses a ring-length later.
#[test]
fn a_discard_mid_stream_leaves_the_output_the_full_path_gives_one_window_on() {
    let input = sine::<f64>(1234.5, 0.5, 0.3, 40_000);
    let run = |discard: bool| {
        let mut v = vocoder::<f64>(ChannelLayout::MONO);
        v.seat();
        let mut out = Vec::new();
        let mut at = 0;
        let block = |v: &mut PhaseVocoder<f64>, out: &mut Vec<f64>, at: &mut usize, n: usize| {
            let needed = v.input_needed(Samples::new(n)).get();
            let mut y = vec![0.0; n];
            let counts = v.process(
                Planes::new(&[&input[*at..*at + needed]]).unwrap(),
                PlanesMut::new(&mut [&mut y]).unwrap(),
            );
            *at += counts.consumed.get();
            out.extend(y);
        };
        for _ in 0..16 {
            block(&mut v, &mut out, &mut at, 512);
        }
        let needed = v.input_needed(Samples::new(HOP)).get();
        let counts = if discard {
            v.discard(
                Planes::new(&[&input[at..at + needed]]).unwrap(),
                Samples::new(HOP),
            )
        } else {
            let mut y = vec![0.0; HOP];
            v.process(
                Planes::new(&[&input[at..at + needed]]).unwrap(),
                PlanesMut::new(&mut [&mut y]).unwrap(),
            )
        };
        assert_eq!(counts.produced.get(), HOP);
        at += counts.consumed.get();
        out.extend(vec![0.0; HOP]);
        for _ in 0..20 {
            block(&mut v, &mut out, &mut at, 512);
        }
        out
    };
    let (full, cheap) = (run(false), run(true));
    let from = 16 * 512 + HOP + WINDOW;
    assert!(
        full.len() > from + 2 * WINDOW,
        "past the energy ring's length"
    );
    for k in from..full.len() {
        assert!((full[k] - cheap[k]).abs() < 1e-12, "frame {k}");
    }
}
