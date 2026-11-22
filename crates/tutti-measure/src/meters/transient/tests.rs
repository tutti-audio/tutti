use super::*;
use crate::test_rng::uniform;
use tutti_rt::SplitMix64;

const RATE: f64 = 48_000.0;

fn rate() -> SampleRate {
    SampleRate::of(RATE)
}

fn detect(cfg: &TransientConfig, samples: &[f64]) -> Vec<usize> {
    detect_transients(cfg, Mono::new(samples), rate())
        .unwrap()
        .into_iter()
        .map(Samples::get)
        .collect()
}

/// A click: `width` samples of 1.0 starting at each of `at`.
fn clicks(len: usize, at: &[usize], width: usize) -> Vec<f64> {
    let mut x = vec![0.0; len];
    for &k in at {
        x[k..k + width].fill(1.0);
    }
    x
}

/// A decaying burst: `amplitude * exp(-m / decay) * sin(2 pi hz m / rate + phase)` from `at`.
fn burst(x: &mut [f64], at: usize, hz: f64, phase: f64) {
    for (m, y) in x[at..].iter_mut().enumerate() {
        let m = m as f64;
        *y += 0.8 * (-m / 2400.0).exp() * (core::f64::consts::TAU * hz * m / RATE + phase).sin();
    }
}

/// Eight-sample clicks at known samples are found at exactly those samples.
///
/// The fast envelope steps by `c (1 - f)` per click sample (`c = 1 - exp(-1 / 48)` at 1 ms),
/// which falls with `f`, so its largest rise is the click's first sample: the position is
/// exact, where the first frame the difference passes 0.05 is the third (`f = 0.061`).
#[test]
fn clicks_are_found_at_their_first_sample() {
    let at = [12_000, 30_000, 60_001, 91_234];
    let found = detect(&TransientConfig::new(), &clicks(100_000, &at, 8));
    assert_eq!(found, at);
}

/// A decaying 200 Hz burst whose first sample is its peak (cosine phase) is found at its first
/// sample: every later rise is `c (|x| - f)` under the first's `c * 0.8`. One that starts from
/// zero (sine phase) is found within a quarter period (60 samples) of its start, where `|x|`
/// first peaks.
#[test]
fn decaying_bursts_are_found_at_their_start() {
    let quarter = (RATE / 200.0 / 4.0) as usize;
    let mut x = vec![0.0; 100_000];
    burst(&mut x, 10_000, 200.0, core::f64::consts::FRAC_PI_2);
    burst(&mut x, 50_000, 200.0, 0.0);
    let found = detect(&TransientConfig::new(), &x);
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0], 10_000);
    assert!(
        (50_000..=50_000 + quarter).contains(&found[1]),
        "{}",
        found[1]
    );
}

/// A steady 1 kHz sine of 0.5 and stationary noise of +-0.2 give none past their own start (a
/// signal from silence is an attack at its first samples, within 32).
///
/// The fast envelope of `|0.5 sin|` ripples at 2 kHz through a pole at 159 Hz, by
/// `0.42 * 0.5 / sqrt(1 + (2000 / 159)^2) = 0.017`, under the 0.05 threshold. The noise's
/// rectified level has a standard deviation of 0.058 per sample, so the fast envelope
/// (about 96 independent samples wide) moves by 0.006, a threshold of 8 sigma.
#[test]
fn steady_sine_and_stationary_noise_have_no_transients() {
    let sine: Vec<f64> = (0..96_000)
        .map(|n| 0.5 * (core::f64::consts::TAU * 1000.0 * n as f64 / RATE).sin())
        .collect();
    let at_start = |found: Vec<usize>| found.len() == 1 && found[0] < 32;
    assert!(at_start(detect(&TransientConfig::new(), &sine)));

    let mut rng = SplitMix64::new(5);
    let noise: Vec<f64> = (0..96_000).map(|_| uniform(&mut rng, -0.2, 0.2)).collect();
    assert!(at_start(detect(&TransientConfig::new(), &noise)));
}

/// Two clicks 960 samples (20 ms) apart, inside the 50 ms gap, give the first only; the same
/// two 4800 samples (100 ms) apart give both.
#[test]
fn two_transients_inside_the_gap_give_one() {
    let cfg = TransientConfig::new();
    assert_eq!(
        detect(&cfg, &clicks(40_000, &[10_000, 10_960], 8)),
        [10_000]
    );
    assert_eq!(
        detect(&cfg, &clicks(40_000, &[10_000, 14_800], 8)),
        [10_000, 14_800]
    );
}

/// A linear fade-in of a 1 kHz sine to 0.5 over 0.5 s gives none, and the same fade over 5 ms
/// gives one.
///
/// For a ramp of slope `g` the two envelopes differ by about `g (tau_slow - tau_fast)` times
/// the mean rectified level (`2 / pi` for a sine): `1.0 * 0.029 * 0.64 = 0.019` at 0.5 s, and
/// with the 0.017 ripple under the 0.05 threshold. At 5 ms the slope is 100 times larger and
/// the fast envelope runs ahead of the slow one by far more.
#[test]
fn a_slow_fade_in_is_not_a_transient() {
    let fade = |len: usize| -> Vec<f64> {
        (0..len + 4800)
            .map(|n| {
                let ramp = (n as f64 / len as f64).min(1.0);
                0.5 * ramp * (core::f64::consts::TAU * 1000.0 * n as f64 / RATE).sin()
            })
            .collect()
    };
    assert_eq!(
        detect(&TransientConfig::new(), &fade(24_000)),
        Vec::<usize>::new()
    );
    assert_eq!(detect(&TransientConfig::new(), &fade(240)).len(), 1);
}

/// Silence has none, a NaN reads as quiet and does not latch, and a click after it is found.
#[test]
fn silence_and_nan_have_none_and_a_click_after_a_nan_is_found() {
    let cfg = TransientConfig::new();
    assert!(detect(&cfg, &[0.0; 10_000]).is_empty());
    let mut x = clicks(10_000, &[6000], 8);
    x[100..200].fill(f64::NAN);
    assert_eq!(detect(&cfg, &x), [6000]);
}

/// A meter fed uneven chunks yields what the one-shot reads, as items and as output.
#[test]
fn a_meter_fed_in_chunks_equals_the_one_shot() {
    let cfg = TransientConfig::new();
    let x = clicks(60_000, &[5000, 20_000, 40_001], 8);
    let whole = detect(&cfg, &x);
    for chunk in [1usize, 7, 511, 4097, x.len()] {
        let mut meter = Measure::<f64>::measure(&cfg, rate()).unwrap();
        let mut items = Vec::new();
        for part in x.chunks(chunk) {
            meter.push(Mono::new(part), &mut items).unwrap();
        }
        let items: Vec<usize> = items.into_iter().map(Samples::get).collect();
        assert_eq!(items, whole, "chunk {chunk}");
        let out: Vec<usize> = meter.finish().into_iter().map(Samples::get).collect();
        assert_eq!(out, whole, "chunk {chunk}");
    }
}

/// An attack that does not stand above the slow envelope is not reported, though the fast
/// envelope rises.
///
/// A 1 kHz sine at 0.8 for 0.3 s, then 0.1 for 20 ms, then 0.3. The fast envelope (mean
/// rectified level `2/pi` of the amplitude) falls from 0.51 to 0.064 and then rises to 0.19, a
/// rise of 0.127 within the 5 ms lookback, over the 0.05 threshold. The slow envelope (30 ms)
/// has decayed from 0.51 only to `0.064 + 0.45 exp(-20/30) = 0.29` at the step, and stays over
/// the fast one (under 0.19 plus ripple 0.01) until it falls to 0.19 much later, so
/// `fast - slow` stays negative and the one start is the only attack. With the same step after
/// the slow envelope has settled at 0.064 (0.3 s at 0.1) the difference reaches about
/// `0.84 * 0.127 = 0.11` and the step is found.
#[test]
fn a_rise_under_the_slow_envelope_is_not_an_attack() {
    let tone = |parts: &[(f64, usize)]| -> Vec<f64> {
        let mut n = 0usize;
        parts
            .iter()
            .flat_map(|&(amp, len)| {
                let start = n;
                n += len;
                (start..start + len)
                    .map(move |i| amp * (core::f64::consts::TAU * 1000.0 * i as f64 / RATE).sin())
            })
            .collect()
    };
    let cfg = TransientConfig::new();
    let under = tone(&[(0.8, 14_400), (0.1, 960), (0.3, 9600)]);
    let found = detect(&cfg, &under);
    assert!(found.len() == 1 && found[0] < 32, "{found:?}");

    let settled = tone(&[(0.1, 14_400), (0.3, 9600)]);
    let found = detect(&cfg, &settled);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!((14_400..14_400 + 240).contains(&found[1]), "{}", found[1]);
}
