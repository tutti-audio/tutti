use super::*;
use tutti_units::Db;

const RATE: f64 = 44100.0;

/// A 10 ms decaying 1 kHz burst at each of `times` (seconds), scaled by the
/// paired gain, over `secs` seconds.
fn burst_train(times: &[(f64, f64)], secs: f64) -> Vec<f64> {
    let mut out = vec![0.0f64; (secs * RATE) as usize];
    for &(t, gain) in times {
        let start = (t * RATE).round() as usize;
        for i in 0..441 {
            if let Some(s) = out.get_mut(start + i) {
                let phase = 2.0 * std::f64::consts::PI * 1000.0 * i as f64 / RATE;
                *s += gain * (phase.sin() * (-(i as f64) / 100.0).exp());
            }
        }
    }
    out
}

/// Clicks at a constant tempo, the first at `offset` seconds.
fn clicks(bpm: f64, offset: f64, secs: f64) -> Vec<f64> {
    let period = 60.0 / bpm;
    let times: Vec<_> = (0..)
        .map(|k| offset + k as f64 * period)
        .take_while(|&t| t < secs - 0.1)
        .map(|t| (t, 1.0))
        .collect();
    burst_train(&times, secs)
}

fn geometry() -> StftGeometry {
    TempoConfig::new()
        .framing
        .geometry(SampleRate::of(RATE))
        .unwrap()
}

/// Positions in samples of the clicks of a train.
fn click_samples(bpm: f64, offset: f64, secs: f64) -> Vec<f64> {
    let period = 60.0 / bpm;
    (0..)
        .map(|k| (offset + k as f64 * period) * RATE)
        .take_while(|&t| t < (secs - 0.1) * RATE)
        .collect()
}

/// The distance from each beat to its nearest click, in samples.
fn miss(grid: &BeatGrid, clicks: &[f64]) -> Vec<f64> {
    grid.beats
        .iter()
        .map(|b| {
            clicks
                .iter()
                .map(|c| (b.get() as f64 - c).abs())
                .fold(f64::MAX, f64::min)
        })
        .collect()
}

fn tempo_of(x: &[f64]) -> Tempo {
    estimate_tempo(&TempoConfig::new(), Mono::new(x), SampleRate::of(RATE)).unwrap()
}

const FRAME_RATE: f64 = RATE / 512.0;

/// The tempo the prior should pick for a click train at `bpm`, as the
/// tempi it may read as: the train's own and its integer divisions down to
/// the range floor, best prior weight first, plus the runner-up when its
/// weight is within 25% of the best: the prior cannot separate them, because the
/// autocorrelation at the longer lag is lower by the curve's overlap and by
/// where the period falls between frames, each worth several percent.
fn expected_readings(bpm: f64) -> Vec<f64> {
    let weight = |b: f64| (-0.5 * (b / 120.0).log2().powi(2)).exp();
    let mut options: Vec<f64> = (1..=3)
        .map(|m| bpm / m as f64)
        .filter(|&b| b >= 60.0)
        .collect();
    options.sort_by(|a, b| weight(*b).total_cmp(&weight(*a)));
    let keep = if options.len() > 1 && weight(options[1]) > 0.75 * weight(options[0]) {
        2
    } else {
        1
    };
    options.truncate(keep);
    options
}

/// The error a reading may have: a third of a frame of period. The tempo is
/// `60 * frame_rate / lag`, so a lag error `d` frames moves it by
/// `bpm^2 * d / (60 * frame_rate)`. A parabola through a peak sampled
/// between two frames is good to about 0.2 frames at worst (measured over
/// the sweep below), which leaves this bound a margin of about 40%.
fn tolerance(bpm: f64) -> f64 {
    bpm * bpm / (3.0 * 60.0 * FRAME_RATE)
}

/// Click trains from 60 to 200 BPM in steps of 7, and 90, 120, 128 and 174,
/// read at their tempo, or at the octave the prior prefers, within a third
/// of a frame of period (0.2 to 1.5 BPM across the range; the measured
/// worst is 0.2 frames). A confidence is a coefficient: at most one, and
/// high for a steady train.
#[test]
fn click_trains_read_at_their_tempo() {
    let mut tempi: Vec<f64> = (60..=200).step_by(7).map(f64::from).collect();
    tempi.extend([90.0, 120.0, 128.0, 174.0]);
    for bpm in tempi {
        let tempo = tempo_of(&clicks(bpm, 0.3, 20.0));
        let readings = expected_readings(bpm);
        assert!(
            readings
                .iter()
                .any(|&r| (tempo.bpm.get() - r).abs() <= tolerance(r)),
            "{bpm} BPM read as {}, wanted one of {readings:?}",
            tempo.bpm
        );
        assert!(tempo.confidence.get() > 0.4 && tempo.confidence.get() <= 1.0);
    }
    let strong = tempo_of(&clicks(120.0, 0.3, 20.0));
    assert!(strong.confidence.get() > 0.9, "{:?}", strong.confidence);
}

/// The confidence is normalised by the whole curve, so a clip short against
/// the period reads lower than a long one of the same train.
#[test]
fn a_short_clip_reads_less_confident() {
    let short = tempo_of(&clicks(60.0, 0.3, 4.0)).confidence.get();
    let long = tempo_of(&clicks(60.0, 0.3, 20.0)).confidence.get();
    assert!(short < 0.85 && long > 0.9 && short < long, "{short} {long}");
}

/// Every beat lands within one hop of a click, and no click is missed by
/// more than the two the tracker may trim at the ends. The test grids
/// start at 0.3 s, so the first beat is a real one.
#[test]
fn beats_land_within_a_hop_of_the_clicks() {
    let hop = geometry().hop().get() as f64;
    for bpm in [90.0, 120.0, 128.0, 174.0] {
        let clicks_at = click_samples(bpm, 0.3, 20.0);
        let x = clicks(bpm, 0.3, 20.0);
        let grid = track_beats(&TempoConfig::new(), Mono::new(&x), SampleRate::of(RATE)).unwrap();
        let worst = miss(&grid, &clicks_at).into_iter().fold(0.0, f64::max);
        assert!(worst <= hop, "{bpm} BPM: a beat is {worst} samples off");
        assert!(
            grid.beats.len() + 2 >= clicks_at.len(),
            "{bpm} BPM: {} beats for {} clicks",
            grid.beats.len(),
            clicks_at.len()
        );
        assert!(
            expected_readings(bpm)
                .iter()
                .any(|&r| (grid.bpm.get() - r).abs() <= tolerance(r)),
            "{bpm}: {}",
            grid.bpm
        );
    }
}

/// 90 BPM clicks searched in 40..80 BPM read as 45: the period doubled, the
/// nearest multiple the range holds. The estimate and every alternative stay
/// inside the range.
#[test]
fn the_estimate_stays_inside_the_range() {
    let cfg = TempoConfig::new().range(TempoRange::new(Bpm::of(40.0), Bpm::of(80.0)).unwrap());
    let x = clicks(90.0, 0.3, 24.0);
    let tempo = estimate_tempo(&cfg, Mono::new(&x), SampleRate::of(RATE)).unwrap();
    assert!((tempo.bpm.get() - 45.0).abs() < 1.0, "{}", tempo.bpm);
    for bpm in std::iter::once(tempo.bpm).chain(tempo.alternatives.iter().map(|a| a.bpm)) {
        assert!((40.0..=80.0).contains(&bpm.get()), "{bpm} is outside");
    }
}

/// A tempo the parabola pushes past the edge of the range is held at it:
/// impulses 24.8 frames apart at 100 frames per second are 120.97 BPM, and
/// the search stops at 120.
#[test]
fn a_refined_tempo_is_clamped_to_the_range() {
    let mut curve = vec![0.0f64; 1500];
    let mut at = 3.0;
    while (at as usize) < curve.len() {
        curve[at as usize] = 1.0;
        at += 24.8;
    }
    let cfg = TempoConfig::new().range(TempoRange::new(Bpm::of(60.0), Bpm::of(120.0)).unwrap());
    let tempo =
        estimate_tempo_from_novelty(&cfg, &curve, SampleRate::of(RATE), Samples::new(441)).unwrap();
    assert_eq!(tempo.bpm, Bpm::of(120.0));
}

/// A 60 BPM train with a half-strength click between its beats is read as
/// 120 BPM, the octave the prior favours, with 60 among the alternatives. A
/// 0.1-strength click is too weak to make the 120 BPM period worth its
/// extra weight, so the read stays at 60. Centring the prior on 60, or
/// narrowing it, settles the first case either way.
#[test]
fn the_prior_settles_the_octave() {
    let train = |soft: f64| {
        let mut times: Vec<(f64, f64)> = (0..20).map(|k| (0.3 + k as f64, 1.0)).collect();
        times.extend((0..19).map(|k| (0.8 + k as f64, soft)));
        burst_train(&times, 21.0)
    };
    let half = train(0.5);

    let by_default =
        estimate_tempo(&TempoConfig::new(), Mono::new(&half), SampleRate::of(RATE)).unwrap();
    assert!(
        (by_default.bpm.get() - 120.0).abs() < 1.5,
        "{}",
        by_default.bpm
    );
    assert!(
        by_default
            .alternatives
            .iter()
            .any(|a| (a.bpm.get() - 60.0).abs() < 1.5),
        "{:?}",
        by_default.alternatives
    );

    let weak = estimate_tempo(
        &TempoConfig::new(),
        Mono::new(&train(0.1)),
        SampleRate::of(RATE),
    )
    .unwrap();
    assert!((weak.bpm.get() - 60.0).abs() < 1.5, "{}", weak.bpm);

    let centred = TempoConfig::new()
        .prior_centre(Bpm::of(60.0))
        .prior_spread(PriorSpread::of(1.0));
    let low = estimate_tempo(&centred, Mono::new(&half), SampleRate::of(RATE)).unwrap();
    assert!((low.bpm.get() - 60.0).abs() < 1.5, "{}", low.bpm);

    let narrow = TempoConfig::new()
        .prior_centre(Bpm::of(120.0))
        .prior_spread(PriorSpread::of(0.1));
    let high = estimate_tempo(&narrow, Mono::new(&train(0.1)), SampleRate::of(RATE)).unwrap();
    assert!((high.bpm.get() - 120.0).abs() < 1.5, "{}", high.bpm);

    let none = TempoConfig::new().alternatives(Alternatives::of(0));
    let bare = estimate_tempo(&none, Mono::new(&half), SampleRate::of(RATE)).unwrap();
    assert!(bare.alternatives.is_empty());
}

/// Extra loud clicks 0.2 s after every third beat pull an untethered
/// tracker off the grid and leave a tight one on it.
#[test]
fn tightness_holds_the_grid_against_distractors() {
    let mut times: Vec<(f64, f64)> = (0..38).map(|k| (0.3 + 0.5 * k as f64, 1.0)).collect();
    times.extend((0..12).map(|k| (0.5 + 1.5 * k as f64, 1.5)));
    let x = burst_train(&times, 20.0);
    let grid_clicks = click_samples(120.0, 0.3, 20.0);
    let hop = geometry().hop().get() as f64;
    let run = |tightness: f64| {
        let cfg = TempoConfig::new().tightness(Tightness::new(tightness).unwrap());
        track_beats(&cfg, Mono::new(&x), SampleRate::of(RATE)).unwrap()
    };
    let tight = miss(&run(100.0), &grid_clicks);
    assert!(tight.iter().all(|&m| m <= hop), "{tight:?}");
    let loose = miss(&run(0.0), &grid_clicks);
    assert!(loose.iter().any(|&m| m > hop), "{loose:?}");
}

/// Silence has no tempo, nor does a level that never changes, nor does a
/// curve with no spread.
#[test]
fn a_signal_without_change_has_no_tempo() {
    let cfg = TempoConfig::new();
    for x in [vec![0.0f64; 441_000], vec![0.5f64; 441_000]] {
        assert_eq!(
            estimate_tempo(&cfg, Mono::new(&x), SampleRate::of(RATE)),
            Err(TempoEstimateError::NoSignal)
        );
        assert_eq!(
            track_beats(&cfg, Mono::new(&x), SampleRate::of(RATE)),
            Err(TempoEstimateError::NoSignal)
        );
    }
    // A constant, and a constant with one ulp of rounding on every 7th value.
    let flat = vec![0.3f64; 2000];
    let mut ulp = vec![1.0f64; 2000];
    for v in ulp.iter_mut().step_by(7) {
        *v += f64::EPSILON;
    }
    for curve in [flat, ulp] {
        assert_eq!(
            estimate_tempo_from_novelty(&cfg, &curve, SampleRate::of(RATE), Samples::new(512)),
            Err(TempoEstimateError::NoSignal)
        );
    }
}

/// A NaN or infinity in the input is refused, in samples or in a curve.
#[test]
fn non_finite_input_is_refused() {
    let cfg = TempoConfig::new();
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut x = clicks(120.0, 0.3, 10.0);
        x[20_000] = bad;
        assert_eq!(
            estimate_tempo(&cfg, Mono::new(&x), SampleRate::of(RATE)),
            Err(TempoEstimateError::NonFiniteInput)
        );
        assert_eq!(
            track_beats(&cfg, Mono::new(&x), SampleRate::of(RATE)),
            Err(TempoEstimateError::NonFiniteInput)
        );
        let mut curve = vec![0.0f64; 1000];
        curve[10] = 1.0;
        curve[500] = bad;
        assert_eq!(
            estimate_tempo_from_novelty(&cfg, &curve, SampleRate::of(RATE), Samples::new(512)),
            Err(TempoEstimateError::NonFiniteInput)
        );
        assert_eq!(
            track_beats_from_novelty(
                &cfg,
                &curve,
                SampleRate::of(RATE),
                Samples::new(512),
                Some(Bpm::of(120.0))
            ),
            Err(TempoEstimateError::NonFiniteInput)
        );
    }
}

/// The tracker follows a tempo that rises from 120 to 122 BPM over the clip,
/// where a fixed grid at the mean tempo would end 1800 samples out; the
/// estimate is the mean.
#[test]
fn a_slow_drift_is_followed() {
    let mut t = 0.3;
    let mut times = Vec::new();
    while t < 19.9 {
        times.push((t, 1.0f64));
        t += 60.0 / (120.0 + 2.0 * t / 20.0);
    }
    let x = burst_train(&times, 20.0);
    let truth: Vec<f64> = times.iter().map(|&(t, _)| t * RATE).collect();
    let cfg = TempoConfig::new();
    let grid = track_beats(&cfg, Mono::new(&x), SampleRate::of(RATE)).unwrap();
    let hop = geometry().hop().get() as f64;
    let worst = miss(&grid, &truth).into_iter().fold(0.0, f64::max);
    assert!(worst <= hop, "a beat is {worst} samples off");
    assert!((grid.bpm.get() - 121.0).abs() < 1.5, "{}", grid.bpm);
}

/// The tempo, confidence and beats do not depend on the signal's level.
#[test]
fn the_result_does_not_depend_on_gain() {
    let x = clicks(128.0, 0.3, 20.0);
    let cfg = TempoConfig::new();
    let reference = track_beats(&cfg, Mono::new(&x), SampleRate::of(RATE)).unwrap();
    let tempo = estimate_tempo(&cfg, Mono::new(&x), SampleRate::of(RATE)).unwrap();
    for gain in [0.01f64, 8.0] {
        let y: Vec<f64> = x.iter().map(|s| s * gain).collect();
        let other = estimate_tempo(&cfg, Mono::new(&y), SampleRate::of(RATE)).unwrap();
        assert!((other.bpm.get() - tempo.bpm.get()).abs() < 1e-3);
        assert!((other.confidence.get() - tempo.confidence.get()).abs() < 1e-3);
        let grid = track_beats(&cfg, Mono::new(&y), SampleRate::of(RATE)).unwrap();
        assert_eq!(grid.beats, reference.beats, "gain {gain}");
    }
}

/// An impulse every 40 frames at 100 frames per second is 0.4 s: exactly
/// 150 BPM. The beats sit on the impulses, `frame * hop` samples in.
#[test]
fn a_caller_curve_is_read_in_its_own_frames() {
    let mut curve = vec![0.0f64; 1000];
    for i in (0..1000).step_by(40) {
        curve[i] = 1.0;
    }
    let cfg = TempoConfig::new();
    let tempo =
        estimate_tempo_from_novelty(&cfg, &curve, SampleRate::of(RATE), Samples::new(441)).unwrap();
    assert!((tempo.bpm.get() - 150.0).abs() < 0.01, "{}", tempo.bpm);

    let grid =
        track_beats_from_novelty(&cfg, &curve, SampleRate::of(RATE), Samples::new(441), None)
            .unwrap();
    assert!(grid.beats.len() >= 20);
    assert!(grid.beats.iter().all(|b| b.get() % (40 * 441) == 0));
    assert!(
        grid.beats
            .windows(2)
            .all(|w| w[1].get() - w[0].get() == 40 * 441)
    );
    assert!((grid.bpm.get() - 150.0).abs() < 0.01);
    assert_eq!(
        grid.time(1, SampleRate::of(RATE)),
        Some(Seconds::new(
            0.4 * (grid.beats[1].get() / (40 * 441)) as f64
        ))
    );
    assert_eq!(grid.time(grid.beats.len(), SampleRate::of(RATE)), None);

    // A supplied tempo is used as given, even at 100 BPM against a curve
    // that holds 150: the beats follow the period, not the curve.
    let forced = track_beats_from_novelty(
        &cfg,
        &curve,
        SampleRate::of(RATE),
        Samples::new(441),
        Some(Bpm::of(100.0)),
    )
    .unwrap();
    assert_eq!(forced.bpm, Bpm::of(100.0));
}

/// Unstructured noise has no clear tempo: the confidence is low. White
/// novelty over about 860 frames has an autocorrelation coefficient of
/// standard deviation `1 / sqrt(860)`, 0.034, at each lag beyond the
/// window's overlap; the strongest of the 60 searched lags is under
/// four deviations (0.14), so a bound of 0.3 is eight deviations out and
/// does not rest on the seed.
#[test]
fn noise_has_a_low_confidence() {
    use crate::test_rng::uniform;
    use tutti_rt::SplitMix64;
    let mut rng = SplitMix64::new(7);
    let x: Vec<f64> = (0..441_000).map(|_| uniform(&mut rng, -0.5, 0.5)).collect();
    let tempo = tempo_of(&x);
    assert!(tempo.confidence.get() < 0.3, "{:?}", tempo.confidence);
}

/// Settings and inputs that cannot work say so.
#[test]
fn unusable_settings_are_refused() {
    let cfg = TempoConfig::new();
    for (min, max) in [(100.0, 100.0), (120.0, 90.0)] {
        assert!(matches!(
            TempoRange::try_from(Bpm::of(min)..=Bpm::of(max)),
            Err(EmptyTempoRange { .. })
        ));
    }
    // A zero, negative, NaN or infinite bound, or a centre, is not a
    // `Bpm`: the config is never handed one.
    for raw in [0.0, -5.0, f64::NAN, f64::INFINITY] {
        assert!(Bpm::new(raw).is_none(), "{raw}");
    }

    let curve = vec![0.0f64; 1000];
    assert_eq!(
        estimate_tempo_from_novelty(&cfg, &curve, SampleRate::of(RATE), Samples::new(0)),
        Err(TempoEstimateError::Framing(FramingError::ZeroHop))
    );
    // Under one window, and under two periods of the slowest tempo.
    assert!(matches!(
        estimate_tempo(&cfg, Mono::new(&[0.0f64; 100]), SampleRate::of(RATE)),
        Err(TempoEstimateError::InsufficientInput { .. })
    ));
    let mut short = vec![0.0f64; 100];
    short[3] = 1.0;
    assert!(matches!(
        estimate_tempo_from_novelty(&cfg, &short, SampleRate::of(RATE), Samples::new(512)),
        Err(TempoEstimateError::InsufficientInput { .. })
    ));
    // At a hop this wide no whole-frame lag falls in 100 to 101 BPM.
    let narrow = cfg.range(TempoRange::new(Bpm::of(100.0), Bpm::of(101.0)).unwrap());
    let mut long = vec![0.0f64; 4000];
    long[3] = 1.0;
    assert!(matches!(
        estimate_tempo_from_novelty(&narrow, &long, SampleRate::of(RATE), Samples::new(512)),
        Err(TempoEstimateError::EmptyTempoRange(EmptyTempoRange { .. }))
    ));
}

/// A window much wider than the hop still puts beats within one hop of the
/// clicks: at 4096/512 flux peaks `3 * 4096 / 4 - 256 = 2816` samples after
/// a frame starts. The
/// bound is one hop: half for placing the peak on the frame grid, half for
/// the click's own width (its energy is within the first 300 samples, and
/// an onset's rise can peak a few hundred samples late at a window this
/// wide).
#[test]
fn a_wide_window_still_places_beats_within_a_hop() {
    let wide = TempoConfig::new().framing(Framing::new(Samples::new(4096), Samples::new(512)));
    let clicks_at = click_samples(120.0, 0.3, 20.0);
    let grid = track_beats(
        &wide,
        Mono::new(&clicks(120.0, 0.3, 20.0)),
        SampleRate::of(RATE),
    )
    .unwrap();
    let worst = miss(&grid, &clicks_at).into_iter().fold(0.0, f64::max);
    assert!(worst <= 512.0, "a beat is {worst} samples off");
    assert!(grid.beats.len() + 2 >= clicks_at.len());
}

/// Near-silence has no tempo even though flux is relative: a click train
/// at -100 dBFS is refused, at -80 dBFS it reads.
#[test]
fn near_silence_has_no_tempo() {
    let quiet = |db: f64| -> Vec<f64> {
        // The click train is `f64`: the gain narrows once.
        let gain = Db::new(db).to_amplitude().get();
        clicks(120.0, 0.3, 20.0).iter().map(|s| s * gain).collect()
    };
    let cfg = TempoConfig::new();
    assert_eq!(
        estimate_tempo(&cfg, Mono::new(&quiet(-100.0)), SampleRate::of(RATE)),
        Err(TempoEstimateError::NoSignal)
    );
    assert_eq!(
        track_beats(&cfg, Mono::new(&quiet(-100.0)), SampleRate::of(RATE)),
        Err(TempoEstimateError::NoSignal)
    );
    let tempo = estimate_tempo(&cfg, Mono::new(&quiet(-80.0)), SampleRate::of(RATE)).unwrap();
    assert!((tempo.bpm.get() - 120.0).abs() < 1.5);
}

/// A supplied tempo whose period no curve can hold is an
/// `UntrackableTempo`, not an allocation of the period's size and not
/// an empty grid; a usable one tracks.
#[test]
fn a_supplied_tempo_must_fit_the_curve() {
    let mut curve = vec![0.0f64; 1000];
    for i in (0..1000).step_by(40) {
        curve[i] = 1.0;
    }
    let cfg = TempoConfig::new();
    let hop = Samples::new(441);
    // 100 frames per second: a curve of 1000 frames spans 10 s, so the
    // slowest tempo is 6 BPM (a period of the whole curve) and the fastest
    // 3000 (a period of two frames).
    for bad in [1e-6, 5.0, 3001.0, 1e12] {
        assert_eq!(
            track_beats_from_novelty(&cfg, &curve, SampleRate::of(RATE), hop, Some(Bpm::of(bad))),
            Err(TempoEstimateError::UntrackableTempo { bpm: Bpm::of(bad) }),
            "{bad}"
        );
    }
    for good in [6.0, 150.0, 3000.0] {
        assert!(
            track_beats_from_novelty(&cfg, &curve, SampleRate::of(RATE), hop, Some(Bpm::of(good)))
                .is_ok(),
            "{good}"
        );
    }
}

/// A range too slow or too fast to be a tempo is refused when it is set,
/// and one the curve cannot hold is refused when it is read, not
/// overflowed.
#[test]
fn an_absurd_range_is_refused() {
    let cfg = TempoConfig::new();
    for (min, max) in [(1e-300, 90.0), (0.5, 90.0), (60.0, 1e9), (60.0, 1001.0)] {
        assert!(
            matches!(
                TempoRange::try_from(Bpm::of(min)..=Bpm::of(max)),
                Err(EmptyTempoRange { .. })
            ),
            "{min}..{max}"
        );
    }
    // The slowest range allowed needs 2 * (5168 + 2) frames of curve.
    let slow = cfg.range(TempoRange::new(Bpm::of(1.0), Bpm::of(200.0)).unwrap());
    let curve: Vec<f64> = (0..2000).map(|i| (i % 43) as f64).collect();
    assert!(matches!(
        estimate_tempo_from_novelty(&slow, &curve, SampleRate::of(RATE), Samples::new(512)),
        Err(TempoEstimateError::InsufficientInput { .. })
    ));
}

/// The tempo and beat meters fed uneven chunks read what `estimate_tempo` and
/// `track_beats` read of the whole buffer.
#[test]
fn the_tempo_and_beat_meters_over_uneven_chunks_equal_the_whole_buffer() {
    let x = clicks(120.0, 0.0, 8.0);
    let rate = SampleRate::of(RATE);
    let cfg = TempoConfig::new();
    let tempo = estimate_tempo(&cfg, Mono::new(&x), rate).unwrap();
    let (_, read) = crate::fold_uneven(&cfg, rate, x.len(), |meter, at, out| {
        meter.push(Mono::new(&x[at]), out)
    })
    .unwrap();
    assert_eq!(read, Ok(tempo));
    let grid = track_beats(&cfg, Mono::new(&x), rate).unwrap();
    let (_, beats) = crate::fold_uneven(&BeatConfig::new(cfg), rate, x.len(), |meter, at, out| {
        meter.push(Mono::new(&x[at]), out)
    })
    .unwrap();
    assert_eq!(beats, Ok(grid));
}
