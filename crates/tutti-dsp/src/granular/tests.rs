use super::*;
use tutti_buffers::{ChannelLayout, Interleaved};

fn wave<F: Float>(rate: SampleRate, samples: &[F]) -> Arc<Wave<F>> {
    let mut w = Wave::<F>::new(1, rate);
    w.append(Interleaved::new(samples, ChannelLayout::MONO).unwrap())
        .unwrap();
    Arc::new(w)
}

fn render<F: Float>(cloud: &mut Granular<F>, settings: &GrainSettings, frames: usize) -> Vec<F> {
    let mut out = vec![vec![F::ZERO; frames]];
    cloud.process(settings, PlanesMut::from_vecs(&mut out).unwrap());
    out.remove(0)
}

fn hann(n: usize, length: usize) -> f64 {
    0.5 - 0.5 * (2.0 * core::f64::consts::PI * n as f64 / length as f64).cos()
}

/// The window's derived bound: its phasor, set exactly every 64 frames, is within `4 * 64 eps`
/// of the closed form `c`, and the window `(1 - c) / 2` within half of that.
fn window_bound<F: Float>() -> f64 {
    2.0 * 64.0 * Into::<f64>::into(F::EPSILON)
}

const RATE: SampleRate = SampleRate::SR_48K;

/// One grain of a constant `c` is `c w[n]` with the periodic Hann `w`, and silence after it.
#[test]
fn one_grain_of_a_constant_is_the_window_times_the_constant() {
    let source = wave(RATE, &[0.5_f64; 64]);
    let mut cloud = Granular::new(source, RATE, MaxGrains::of(4), 0);
    // One onset a second: the next is 48000 frames on.
    let settings = GrainSettings::new(Hz::new(1.0), Seconds::new(8.0 / 48_000.0));
    let out = render(&mut cloud, &settings, 16);
    for (n, &y) in out.iter().enumerate() {
        let want = if n < 8 { 0.5 * hann(n, 8) } else { 0.0 };
        assert!(
            (y - want).abs() <= 0.5 * window_bound::<f64>(),
            "frame {n}: {y} against {want}"
        );
    }
}

/// A grain at rate `r` over a tone of `omega` radians a source frame is the tone at `r omega`
/// under the window. The cubic's error is within `omega^3 / 4` (it is exact on quadratics), and
/// a phase that drifts with a wrong ratio is far outside it by frame 2000.
#[test]
fn a_grain_of_a_sine_at_rate_r_is_the_sine_at_r_times_the_frequency() {
    let omega = 2.0 * core::f64::consts::PI * 0.01;
    let tone: Vec<f64> = (0..8192).map(|n| (omega * n as f64).sin()).collect();
    let cents = Cents::new(700.0);
    let ratio = cents.to_pitch_ratio();
    let length = 4096;
    let mut cloud = Granular::new(wave(RATE, &tone), RATE, MaxGrains::of(2), 0);
    let settings =
        GrainSettings::new(Hz::new(1.0), Seconds::new(length as f64 / 48_000.0)).rate(cents);
    let out = render(&mut cloud, &settings, length);
    let bound = (omega * ratio).powi(3) / 4.0 + window_bound::<f64>();
    for (n, &y) in out.iter().enumerate() {
        let want = hann(n, length) * (omega * ratio * n as f64).sin();
        assert!((y - want).abs() <= bound, "frame {n}: {y} against {want}");
    }
}

/// A ramp recorded at half the output rate, started 10 source frames in, is read half a source
/// frame an output frame: `w[n] (10 + n / 2) / 1000`. A cubic is exact on a line.
#[test]
fn a_source_at_half_the_rate_is_read_at_half_a_frame_a_frame_from_its_position() {
    let source_rate = SampleRate::new(24_000.0).unwrap();
    let ramp: Vec<f64> = (0..256).map(|n| n as f64 / 1000.0).collect();
    let source = wave(source_rate, &ramp);
    let duration = source.duration().get();
    let mut cloud = Granular::new(source, RATE, MaxGrains::of(1), 0);
    let settings = GrainSettings::new(Hz::new(1.0), Seconds::new(32.0 / 48_000.0))
        .position(Normalized::of(10.0 / 256.0));
    assert!((duration - 256.0 / 24_000.0).abs() < 1e-15);
    let out = render(&mut cloud, &settings, 32);
    for (n, &y) in out.iter().enumerate() {
        let want = hann(n, 32) * (10.0 + n as f64 / 2.0) / 1000.0;
        assert!((y - want).abs() <= 1e-9, "frame {n}: {y} against {want}");
    }
}

/// Periodic Hann grains of 64 frames every 32 sum to 1 wherever two overlap: from frame 32 to
/// the last onset's start plus 32. Whole-frame intervals: 32768 Hz over 1024 Hz.
#[test]
fn hann_grains_at_half_overlap_of_a_constant_sum_to_the_constant() {
    let rate = SampleRate::new(32_768.0).unwrap();
    let mut cloud = Granular::new(wave(rate, &[0.75_f64; 4096]), rate, MaxGrains::of(4), 0);
    let settings = GrainSettings::new(Hz::new(1024.0), Seconds::new(64.0 / 32_768.0));
    let out = render(&mut cloud, &settings, 512);
    for (n, &y) in out.iter().enumerate().take(480).skip(32) {
        assert!(
            (y - 0.75).abs() <= 2.0 * 0.75 * window_bound::<f64>(),
            "frame {n}: {y}"
        );
    }
}

/// Blocks of any size make the same cloud: the scheduler places onsets by frame, not by block.
#[test]
fn a_block_split_does_not_move_a_grain() {
    let rate = SampleRate::new(32_768.0).unwrap();
    let source = wave(rate, &[0.75_f64; 4096]);
    let settings = GrainSettings::new(Hz::new(1024.0), Seconds::new(64.0 / 32_768.0));
    let mut cloud = Granular::new(source, rate, MaxGrains::of(4), 0);
    let mut out = Vec::new();
    for size in [1, 31, 33, 100, 347] {
        out.extend(render(&mut cloud, &settings, size));
    }
    // The grains started at frames 0, 32, 64, ...: the first 512 frames of the same sum.
    for (n, &y) in out.iter().enumerate().take(480).skip(32) {
        assert!(
            (y - 0.75).abs() <= 2.0 * 0.75 * window_bound::<f64>(),
            "frame {n}: {y}"
        );
    }
}

/// With one slot, a grain of 100 frames every 10 over 200 frames: onsets at 0, 10, .. 190 are 20;
/// the grains started are those at 0 and 100, so 18 are dropped, and none waits.
#[test]
fn an_onset_that_finds_no_free_slot_is_counted_and_not_started() {
    let source = wave(RATE, &[1.0_f64; 1024]);
    let mut cloud = Granular::new(source, RATE, MaxGrains::of(1), 0);
    let settings = GrainSettings::new(Hz::new(4_800.0), Seconds::new(100.0 / 48_000.0));
    render(&mut cloud, &settings, 200);
    assert_eq!(cloud.dropped(), 18);
    cloud.reset();
    assert_eq!((cloud.dropped(), cloud.active()), (0, 0));
}

fn onsets(seed: u64, settings: &GrainSettings, count: usize) -> Vec<GrainStart> {
    let mut scheduler = GrainScheduler::new(seed);
    let mut list = vec![scheduler.restart(Samples::ZERO, settings, RATE).unwrap()];
    while list.len() < count {
        list.push(scheduler.next(settings, RATE).unwrap());
    }
    list
}

fn jittered() -> GrainSettings {
    GrainSettings::new(Hz::new(50.0), Seconds::new(0.02))
        .position(Normalized::of(0.5))
        .position_jitter(Normalized::of(0.25))
        .timing_jitter(Normalized::of(0.5))
}

/// A seed names one stream of onset frames and positions; another seed names another.
#[test]
fn the_same_seed_gives_the_same_onsets_and_another_seed_others() {
    let a = onsets(7, &jittered(), 64);
    assert_eq!(a, onsets(7, &jittered(), 64));
    assert_ne!(a, onsets(8, &jittered(), 64));
    let mut scheduler = GrainScheduler::new(7);
    scheduler.restart(Samples::ZERO, &jittered(), RATE);
    scheduler.next(&jittered(), RATE);
    scheduler.reset();
    assert_eq!(
        scheduler.restart(Samples::ZERO, &jittered(), RATE),
        Some(a[0])
    );
}

/// Onset `k` is the whole frame of `k T + j T u` with `u` in `[-1, 1)`, `T = 960`: its distance
/// from `k T` is within `j T` and a frame, its position within `0.25` of `0.5`, and the
/// displacements average to zero (`n` uniform draws of variance `(j T)^2 / 3`, checked to 5
/// standard deviations, and the floor's half frame).
#[test]
fn jittered_onsets_stay_within_their_spread_and_keep_the_mean_density() {
    let n = 20_000;
    let list = onsets(3, &jittered(), n);
    let (t, j) = (960.0, 0.5);
    let mut sum = 0.0;
    for (k, o) in list.iter().enumerate() {
        let displacement = o.frame.get() as f64 - k as f64 * t;
        assert!(
            displacement.abs() <= t * j + 1.0,
            "onset {k}: {displacement}"
        );
        assert!((o.position.get() - 0.5).abs() <= 0.25);
        sum += displacement;
    }
    let sigma = t * j / (3.0 * n as f64).sqrt();
    assert!(
        (sum / n as f64 + 0.5).abs() < 5.0 * sigma,
        "mean {}",
        sum / n as f64
    );
}

/// With no jitter onset `k` is at `floor(k * 48000 / 9)` for every `k`, the whole-frame ones
/// included (`k = 195` is 1040000), where `k * (48000 / 9)` is a rounding under a whole frame.
#[test]
fn onsets_without_jitter_are_at_the_floor_of_k_intervals_for_every_k() {
    let settings = GrainSettings::new(Hz::new(9.0), Seconds::new(0.01));
    for (k, o) in onsets(0, &settings, 1_000).iter().enumerate() {
        assert_eq!(o.frame.get(), k * 48_000 / 9, "onset {k}");
    }
}

/// Half an hour of onsets, 7 a second and 1000 a second, with no rendering: the last is at the
/// closed form's frame, which a running sum of f64 intervals does not keep.
#[test]
fn an_onset_half_an_hour_in_is_at_the_closed_forms_frame() {
    for density in [7_u64, 1_000] {
        let settings = GrainSettings::new(Hz::new(density as f64), Seconds::new(0.01));
        let count = (1_800 * density) as usize;
        let mut scheduler = GrainScheduler::new(0);
        let mut last = scheduler.restart(Samples::ZERO, &settings, RATE).unwrap();
        for _ in 1..count {
            last = scheduler.next(&settings, RATE).unwrap();
        }
        let k = (count - 1) as u128;
        assert_eq!(
            last.frame.get() as u128,
            k * 48_000 / density as u128,
            "density {density}"
        );
    }
}

/// A change of density starts a run from the last onset's nominal time: 100 Hz at 48 kHz is 480
/// frames, then 50 Hz from onset 3 at 1440 is 960.
#[test]
fn a_density_change_continues_from_the_last_onset() {
    let fast = GrainSettings::new(Hz::new(100.0), Seconds::new(0.01));
    let slow = GrainSettings::new(Hz::new(50.0), Seconds::new(0.01));
    let mut scheduler = GrainScheduler::new(0);
    scheduler.restart(Samples::ZERO, &fast, RATE);
    let frames: Vec<usize> = [fast, fast, fast, slow, slow]
        .iter()
        .map(|s| scheduler.next(s, RATE).unwrap().frame.get())
        .collect();
    assert_eq!(frames, [480, 960, 1440, 2400, 3360]);
}

/// A phasor window of grains from 5000 to a million frames (a turn per frame of `1e-3` to `6e-6`
/// radians, where the rounded `cos` of the turn is `1` or a few units off it) is within the derived
/// bound of the closed form at every frame, in `f32` where the bound is `1.5e-5` and in `f64`.
#[test]
fn a_long_grains_window_stays_within_the_derived_bound_of_the_hann() {
    fn run<F: Float>(length: usize) {
        let mut cloud = Granular::new(wave(RATE, &vec![F::ONE; length]), RATE, MaxGrains::of(1), 0);
        let settings = GrainSettings::new(Hz::new(0.1), Seconds::new(length as f64 / 48_000.0));
        let out = render(&mut cloud, &settings, length);
        let bound = 0.5 * window_bound::<F>();
        let worst = out
            .iter()
            .enumerate()
            .map(|(n, &y)| (Into::<f64>::into(y) - hann(n, length)).abs())
            .fold(0.0, f64::max);
        assert!(
            worst <= bound,
            "length {length}: worst {worst} against {bound}"
        );
    }
    for length in [5_000, 12_800, 20_000, 50_000, 1_000_000] {
        run::<f32>(length);
    }
    run::<f64>(1_000_000);
}

/// A density that is not above zero starts nothing.
#[test]
fn a_density_of_zero_starts_no_grain() {
    let settings = GrainSettings::new(Hz::new(0.0), Seconds::new(0.01));
    let mut cloud = Granular::new(wave(RATE, &[1.0_f32; 64]), RATE, MaxGrains::of(2), 0);
    assert!(render(&mut cloud, &settings, 128).iter().all(|&x| x == 0.0));
}
