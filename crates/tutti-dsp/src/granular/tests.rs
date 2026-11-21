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

/// A rate with whole-frame intervals at power-of-two densities.
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
        assert!((y - want).abs() < 1e-15, "frame {n}: {y} against {want}");
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
    let bound = (omega * ratio).powi(3) / 4.0;
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
        assert!((y - want).abs() < 1e-9, "frame {n}: {y} against {want}");
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
        assert!((y - 0.75).abs() < 1e-14, "frame {n}: {y}");
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
        assert!((y - 0.75).abs() < 1e-14, "frame {n}: {y}");
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

fn onsets(seed: u64, settings: &GrainSettings, count: usize) -> Vec<Onset> {
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

/// Intervals are `T (1 + j u)` with `u` in `[-1, 1)`, so each is within `T j` of `T = 960`
/// frames, and positions are within `0.25` of `0.5`; the mean interval over `n` onsets has the
/// variance `(T j)^2 / (3 n)` of uniform draws, checked to 5 standard deviations.
#[test]
fn jittered_onsets_stay_within_their_spread_and_keep_the_mean_density() {
    let n = 20_000;
    let list = onsets(3, &jittered(), n);
    let (t, j) = (960.0, 0.5);
    let times: Vec<f64> = list.iter().map(|o| o.frame.get() as f64).collect();
    for pair in times.windows(2) {
        // A frame is the floor of a time, so a gap is within a frame of the interval.
        assert!((pair[1] - pair[0] - t).abs() <= t * j + 1.0);
    }
    for o in &list {
        assert!((o.position.get() - 0.5).abs() <= 0.25);
    }
    let mean = (times[n - 1] - times[0]) / (n - 1) as f64;
    let sigma = t * j / (3.0 * (n - 1) as f64).sqrt();
    assert!(
        (mean - t).abs() < 5.0 * sigma + 1.0 / (n - 1) as f64,
        "mean {mean}"
    );
}

/// With no jitter an onset is at the floor of `k` intervals: 48000 / 7 frames, never at a
/// multiple of 7 where the sum may sit a rounding under a whole frame.
#[test]
fn onsets_without_jitter_are_at_the_floor_of_whole_intervals() {
    let settings = GrainSettings::new(Hz::new(7.0), Seconds::new(0.01));
    let list = onsets(0, &settings, 100);
    for (k, o) in list.iter().enumerate().filter(|(k, _)| k % 7 != 0) {
        assert_eq!(
            o.frame.get(),
            (k as f64 * 48_000.0 / 7.0).floor() as usize,
            "onset {k}"
        );
    }
}

/// A density that is not above zero starts nothing.
#[test]
fn a_density_of_zero_starts_no_grain() {
    let settings = GrainSettings::new(Hz::new(0.0), Seconds::new(0.01));
    let mut cloud = Granular::new(wave(RATE, &[1.0_f32; 64]), RATE, MaxGrains::of(2), 0);
    assert!(render(&mut cloud, &settings, 128).iter().all(|&x| x == 0.0));
}
