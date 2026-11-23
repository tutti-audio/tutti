use super::*;
use crate::AboveNyquist;
use crate::analyses::yin::{PitchEstimate, yin_track};
use crate::test_rng::uniform;
use tutti_fixtures::signal::sine;
use tutti_rt::SplitMix64;
use tutti_units::{Amplitude, Hz, Samples};

const RATE: SampleRate = SampleRate::of(16_000.0);
/// A 10 ms hop at 16 kHz.
const HOP: usize = 160;

fn hop() -> PitchHop {
    PitchHop::of(Samples::new(HOP))
}

/// The YIN buffer of the default band at 16 kHz: twice the longest period,
/// `16000 / 55 = 290`.
const FRAME: usize = 580;

fn track(cfg: &PyinConfig, x: &[f32]) -> Vec<PyinFrame> {
    pyin_track(&cfg.hop(hop()), Mono::new(x), RATE).unwrap()
}

fn cents_between(a: f64, b: f64) -> f64 {
    1200.0 * libm::log2(a / b)
}

/// The frames whose whole buffer lies in samples `from..to`.
fn frames_within(from: usize, to: usize, frames: usize) -> impl Iterator<Item = usize> {
    (0..frames).filter(move |i| i * HOP >= from && i * HOP + FRAME <= to)
}

/// `a1 cos(w n) + cos(2 w n)` scaled to peak under 1, with `a1` per sample.
fn two_harmonics(f0: f64, a1: impl Fn(usize) -> f64, len: usize) -> Vec<f32> {
    let w = core::f64::consts::TAU * f0 / RATE.get();
    (0..len)
        .map(|n| {
            let t = n as f64 * w;
            (0.4 * (a1(n) * libm::cos(t) + libm::cos(2.0 * t))) as f32
        })
        .collect()
}

/// A steady sine is voiced at its frequency on every frame. The bound is the
/// grid's half step (5 cents) plus the parabola's error on d': the weight
/// 1 / tau of the cumulative mean tilts the dip by at most half a sample over
/// tau, in cents `1731 * 0.5 / tau^2` (0.65 at 440 Hz, 1.66 at 700 Hz).
#[test]
fn a_steady_sine_is_its_frequency_within_half_a_grid_step() {
    for freq in [110.0, 300.0, 440.0, 700.0] {
        let tau = RATE.get() / freq;
        let bound = 5.0 + 1731.2 * 0.5 / (tau * tau);
        let frames = track(
            &PyinConfig::new(),
            &sine(
                Hz::new(freq),
                Amplitude::new(0.5),
                RATE,
                Samples::new(16_000),
            ),
        );
        assert_eq!(frames.len(), (16_000 - FRAME) / HOP + 1);
        for (i, f) in frames.iter().enumerate() {
            let got = f.frequency.unwrap_or_else(|| panic!("{freq} Hz frame {i}"));
            let off = cents_between(got.get(), freq).abs();
            assert!(off <= bound, "{freq} Hz frame {i}: {off} cents");
            assert!(f.voiced.get() > 0.99, "{freq} Hz frame {i}: {:?}", f.voiced);
        }
    }
}

/// Silence and white noise have no dip under the prior's mass: every frame is
/// unvoiced.
#[test]
fn silence_and_white_noise_are_unvoiced() {
    let mut rng = SplitMix64::new(3);
    let noise: Vec<f32> = (0..16_000)
        .map(|_| uniform(&mut rng, -0.5f32, 0.5))
        .collect();
    for (name, x) in [("silence", vec![0.0f32; 16_000]), ("noise", noise)] {
        for (i, f) in track(&PyinConfig::new(), &x).iter().enumerate() {
            assert_eq!(f.frequency, None, "{name} frame {i}");
        }
    }
}

/// A tone, a gap, a tone: frames whose buffer is wholly in a part take its
/// state, and the track changes state exactly twice, each change between the
/// last frame wholly in one part and the first wholly in the next, that is
/// within `ceil(FRAME / HOP) = 4` frames of the gap's edge.
#[test]
fn a_gap_is_unvoiced_between_voiced_tones() {
    let (gap_from, gap_to, len) = (6400, 11_200, 19_200);
    let mut x = sine(Hz::new(440.0), Amplitude::new(0.5), RATE, Samples::new(len));
    x[gap_from..gap_to].fill(0.0);
    let frames = track(&PyinConfig::new(), &x);
    for i in frames_within(0, gap_from, frames.len()) {
        assert!(frames[i].frequency.is_some(), "tone frame {i}");
    }
    for i in frames_within(gap_from, gap_to, frames.len()) {
        assert!(frames[i].frequency.is_none(), "gap frame {i}");
    }
    for i in frames_within(gap_to, len, frames.len()) {
        assert!(frames[i].frequency.is_some(), "tone frame {i}");
    }
    let changes: Vec<usize> = (1..frames.len())
        .filter(|&i| frames[i].frequency.is_some() != frames[i - 1].frequency.is_some())
        .collect();
    assert_eq!(changes.len(), 2, "{changes:?}");
    let edge = |sample: usize| sample / HOP;
    assert!(changes[0].abs_diff(edge(gap_from)) <= 4, "{changes:?}");
    assert!(changes[1].abs_diff(edge(gap_to)) <= 4, "{changes:?}");
}

/// A tone at 220 Hz whose second harmonic is the strong partial: d' at half
/// the period is `2 a1^2 / (1 + a1^2)` for a fundamental of amplitude `a1`
/// against a unit second harmonic, 0.077 at `a1 = 0.2`, under YIN's 0.1. Plain
/// YIN at 0.1 reports the octave on every such frame. A strong fundamental
/// comes first (`a1 = 1`: d' has no dip at half the period), and pYIN keeps it
/// through the weak stretch: there the thresholds under 0.077 (about 0.4 of the
/// prior) still select the fundamental, and the octave is 1200 cents away,
/// five transition widths, each step of which crosses bins at the emission
/// floor.
#[test]
fn the_track_holds_the_fundamental_where_plain_yin_reports_the_octave() {
    let switch_at = 6400;
    let len = 16_000;
    let x = two_harmonics(220.0, |n| if n < switch_at { 1.0 } else { 0.2 }, len);
    let band = FrequencyBand::PYIN;
    let plain = yin_track(&YinConfig::new().band(band), Mono::new(&x), RATE, hop()).unwrap();
    let frames = track(&PyinConfig::new(), &x);
    let weak: Vec<usize> = frames_within(switch_at, len, frames.len()).collect();
    assert!(weak.len() > 50);
    for &i in &weak {
        let PitchEstimate::Voiced(p) = plain[i] else {
            panic!("plain frame {i} unvoiced");
        };
        assert!(
            cents_between(p.frequency.get(), 440.0).abs() < 30.0,
            "plain YIN frame {i}: {}",
            p.frequency
        );
    }
    for (i, f) in frames.iter().enumerate() {
        if i * HOP + FRAME <= switch_at || i * HOP >= switch_at {
            let got = f.frequency.unwrap_or_else(|| panic!("frame {i} unvoiced"));
            assert!(
                cents_between(got.get(), 220.0).abs() <= 6.0,
                "frame {i}: {got}"
            );
        }
    }
}

/// A chirp from 200 Hz at 100 Hz/s: the instantaneous frequency is
/// `200 + 100 t`, which moves at most `1731 * 100 / 200 = 866` cents a second.
/// A frame reads `FRAME` samples (36.25 ms) whose estimate is the frequency at
/// the middle of them within the drift over half that span, 15.7 cents, plus
/// the grid and the parabola (6 cents). Between frames the track moves by less
/// than the transition width.
#[test]
fn a_glide_is_followed_within_the_transition_band() {
    let len = 16_000;
    let phase = |n: usize| {
        let t = n as f64 / RATE.get();
        core::f64::consts::TAU * (200.0 * t + 50.0 * t * t)
    };
    let x: Vec<f32> = (0..len)
        .map(|n| (0.5 * libm::sin(phase(n))) as f32)
        .collect();
    let cfg = PyinConfig::new();
    let frames = track(&cfg, &x);
    let mut last: Option<f64> = None;
    for (i, f) in frames.iter().enumerate() {
        let got = f
            .frequency
            .unwrap_or_else(|| panic!("frame {i} unvoiced"))
            .get();
        let centre = (i * HOP + FRAME / 2) as f64 / RATE.get();
        let want = 200.0 + 100.0 * centre;
        assert!(
            cents_between(got, want).abs() <= 15.7 + 6.0,
            "frame {i}: {got} against {want}"
        );
        if let Some(previous) = last {
            assert!(cents_between(got, previous).abs() <= 250.0, "frame {i}");
        }
        last = Some(got);
    }
}

/// The prior's weights sum to 1 and their mean is the configured mean, up to
/// the grid: each cell's mass sits at its upper edge, so the mean is high by
/// at most the grid step 0.01. The paper's three means.
#[test]
fn the_threshold_weights_are_a_distribution_with_the_configured_mean() {
    for mean in [0.1, 0.15, 0.2] {
        let weights = threshold_weights(mean);
        let total: f64 = weights.iter().sum();
        assert!((total - 1.0).abs() < 1e-12, "mean {mean}: {total}");
        let got: f64 = weights
            .iter()
            .enumerate()
            .map(|(i, w)| w * (i + 1) as f64 / THRESHOLDS as f64)
            .sum();
        assert!((got - mean).abs() <= 0.01, "mean {mean}: {got}");
    }
}

/// Every row of the decoder's transition matrix, at the default grid, sums to
/// 1: the moves out of any state, edge bins included, are a distribution.
#[test]
fn every_row_of_the_transition_matrix_sums_to_one() {
    let (bins, model) = PyinConfig::new().model().unwrap();
    assert_eq!(bins, 481);
    let states = (bins * 2) as u16;
    for from in 0..states {
        let sum: f64 = (0..states)
            .filter_map(|to| model.log_probability(from, to))
            .map(f64::exp)
            .sum();
        assert!((sum - 1.0).abs() < 1e-12, "row {from}: {sum}");
    }
}

#[test]
fn a_band_above_nyquist_a_short_buffer_and_a_huge_grid_are_refused() {
    let wide = FrequencyBand::new(Hz::new(55.0), Hz::new(9000.0)).unwrap();
    assert_eq!(
        pyin_track(
            &PyinConfig::new().band(wide).hop(hop()),
            Mono::new(&[0.0f32; 4096]),
            RATE
        ),
        Err(PitchError::Nyquist(AboveNyquist {
            freq: Hz::new(9000.0),
            nyquist: Hz::new(8000.0),
        }))
    );
    assert_eq!(
        pyin_track(
            &PyinConfig::new().hop(hop()),
            Mono::new(&[0.0f32; 100]),
            RATE
        ),
        Err(PitchError::InsufficientInput {
            needed: Samples::new(FRAME),
            got: Samples::new(100),
        })
    );
    let tiny = FrequencyBand::new(Hz::new(1e-100), Hz::new(1000.0)).unwrap();
    assert!(matches!(
        PyinConfig::new().band(tiny).model(),
        Err(PitchError::PitchGridTooLarge { .. })
    ));
}

/// A meter of a 220 Hz tone tracks it within half a grid step in any chunking,
/// and refuses a band above Nyquist, a NaN and more frames than a track decodes.
#[test]
fn the_pyin_meter_tracks_the_tone_and_refuses_what_it_cannot_hold() {
    let cfg = PyinConfig::new().hop(hop());
    let x: Vec<f64> = sine(
        Hz::new(220.0),
        Amplitude::new(0.5),
        RATE,
        Samples::new(4000),
    )
    .iter()
    .map(|&s| f64::from(s))
    .collect();
    let track = crate::measure::chunk_independent(&cfg, RATE, &x).unwrap();
    let voiced: Vec<f64> = track
        .iter()
        .filter_map(|f| f.frequency)
        .map(Hz::get)
        .collect();
    assert!(
        voiced.len() * 2 > track.len(),
        "{} of {}",
        voiced.len(),
        track.len()
    );
    assert!(voiced.iter().all(|&f| cents_between(f, 220.0).abs() < 5.5));

    let wide = FrequencyBand::new(Hz::new(55.0), Hz::new(9000.0)).unwrap();
    assert!(matches!(
        Measure::<f64>::measure(&PyinConfig::new().band(wide), RATE),
        Err(MeterError::Pitch(PitchError::Nyquist(_)))
    ));

    let mut meter = Measure::<f64>::measure(&cfg, RATE).unwrap();
    assert_eq!(
        meter.push(Mono::new(&[f64::INFINITY]), &mut Vec::new()),
        Err(MeterError::NonFiniteInput)
    );
    let dense = PyinConfig::new().hop(PitchHop::of(Samples::new(1)));
    let mut meter = Measure::<f64>::measure(&dense, RATE).unwrap();
    let long = vec![0.0f64; FRAME + MAX_FRAMES.get()];
    assert!(matches!(
        meter.push(Mono::new(&long), &mut Vec::new()),
        Err(MeterError::TooManyFrames(_))
    ));
}
