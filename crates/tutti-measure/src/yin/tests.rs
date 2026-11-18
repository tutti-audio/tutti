use super::*;

const SR: SampleRate = SampleRate::SR_44K1;

fn standard() -> YinConfig {
    YinConfig::new()
}

fn frame(cfg: &YinConfig) -> usize {
    cfg.buffer_size(SR).unwrap().get()
}

fn pitch_of(cfg: &YinConfig, x: &[f32]) -> Result<PitchEstimate> {
    yin(cfg, Mono::new(x), SR)
}

fn track_of(cfg: &YinConfig, x: &[f32], hop: usize) -> Result<Vec<PitchEstimate>> {
    yin_track(cfg, Mono::new(x), SR, PitchHop::of(Samples(hop)))
}

fn sine(sample_rate: f64, freq: f32, seconds: f32) -> Vec<f32> {
    let n = (sample_rate as f32 * seconds) as usize;
    tutti_fixtures::signal::sine(
        tutti_units::Hz(freq),
        tutti_units::Amplitude(0.5),
        tutti_units::SampleRate::of(sample_rate),
        tutti_units::Samples(n),
    )
}

/// No band is written from an inverted, empty, non-positive or NaN pair,
/// so there is no silent runtime nothing.
#[test]
fn an_inverted_range_is_not_a_band() {
    assert_eq!(
        FrequencyBand::try_from(Hz(2000.0)..=Hz(50.0)),
        Err(Error::EmptyFrequencyRange {
            min: Hz(2000.0),
            max: Hz(50.0),
        })
    );
    for (lo, hi) in [(50.0, 50.0), (-10.0, 2000.0), (f32::NAN, 2000.0)] {
        assert!(FrequencyBand::new(Hz(lo), Hz(hi)).is_none(), "{lo} {hi}");
    }
}

#[test]
fn a_maximum_above_nyquist_is_refused() {
    assert_eq!(
        yin(
            &YinConfig::new().band(FrequencyBand::new(Hz(50.0), Hz(5000.0)).unwrap()),
            Mono::new(&[0.0f32; 4096]),
            SampleRate::SR_8K,
        ),
        Err(Error::AboveNyquist {
            freq: Hz(5000.0),
            nyquist: Hz(4000.0),
        })
    );
}

/// A short buffer is an error, not a silent "unvoiced".
#[test]
fn a_short_buffer_reports_why() {
    let cfg = standard();
    let samples = vec![0.0f32; 64];

    assert_eq!(
        pitch_of(&cfg, &samples),
        Err(Error::InsufficientInput {
            needed: cfg.buffer_size(SR).unwrap(),
            got: Samples(64),
        })
    );
}

#[test]
fn silence_is_unvoiced() {
    let cfg = standard();
    let samples = vec![0.0f32; frame(&cfg) * 2];

    let estimate = pitch_of(&cfg, &samples).unwrap();
    assert!(!estimate.is_voiced());
    assert_eq!(estimate.frequency(), Hz(0.0));
    assert_eq!(estimate.confidence(), Confidence::NONE);
    assert!(estimate.pitch().is_none());
}

/// A non-finite sample in the window is an unvoiced frame, never a panic.
#[test]
fn a_non_finite_sample_is_unvoiced_not_a_panic() {
    let cfg = standard();
    for bad in [f32::NAN, f32::INFINITY] {
        let mut samples = sine(44100.0, 440.0, 1.0);
        samples.truncate(frame(&cfg) + 10);
        samples[3] = bad;
        let estimate = pitch_of(&cfg, &samples).unwrap();
        assert!(!estimate.is_voiced(), "{bad} produced a pitch");
    }
}

/// The property `yin_track` relies on: one detector driven across many
/// frames agrees with a fresh detector per frame.
///
/// `yin` builds a fresh detector every call, so the idempotence test below
/// does *not* exercise the reused path — this one does. Without it, the
/// module doc's soundness argument for reusing a detector across a track
/// was unpinned.
#[test]
fn a_reused_detector_matches_fresh_ones() {
    let cfg = standard();
    let frame = frame(&cfg);

    // A sweep, so successive frames differ and a leaked carry would show.
    let samples: Vec<f32> = (0..frame * 6)
        .map(|i| {
            let t = i as f32 / 44100.0;
            let freq = 220.0 + 440.0 * t;
            (2.0 * core::f32::consts::PI * freq * t).sin() * 0.5
        })
        .collect();

    let reused = track_of(&cfg, &samples, frame).unwrap();

    for (i, expected) in reused.iter().enumerate() {
        let start = i * frame;
        let fresh = pitch_of(&cfg, &samples[start..start + frame]).unwrap();
        assert_eq!(fresh, *expected, "frame {i} differs from a fresh detector");
    }
}

#[test]
fn the_same_input_always_gives_the_same_estimate() {
    let cfg = standard();
    let a = sine(44100.0, 440.0, 0.1);
    let b = sine(44100.0, 220.0, 0.1);

    let first = pitch_of(&cfg, &a).unwrap();
    let _ = pitch_of(&cfg, &b).unwrap();
    assert_eq!(first, pitch_of(&cfg, &a).unwrap());
}

#[test]
fn a_track_is_voiced_throughout_a_steady_tone() {
    let cfg = standard();
    let samples = sine(44100.0, 440.0, 0.5);

    let track = track_of(&cfg, &samples, 512).unwrap();
    assert!(!track.is_empty());

    let voiced = track.iter().filter(|p| p.is_voiced()).count();
    assert!(
        voiced > track.len() / 2,
        "{voiced} of {} voiced",
        track.len()
    );
}

#[test]
fn periods_derive_from_the_range_and_cannot_contradict_it() {
    let cfg = YinConfig::new()
        .band(FrequencyBand::new(Hz(100.0), Hz(1000.0)).unwrap())
        .plan(SR)
        .unwrap();
    assert_eq!(cfg.min_period(), Samples(44)); // 44100 / 1000
    assert_eq!(cfg.max_period(), Samples(441)); // 44100 / 100
    assert_eq!(cfg.buffer_size(), Samples(882));
    assert!(cfg.min_period() < cfg.max_period());
}

#[test]
fn note_names_use_both_spellings() {
    let pitch = Pitch {
        frequency: Hz(277.18),
        confidence: Confidence(0.9),
        note: Note::new(tutti_music::PitchClass::CSharp, 4),
        cents_offset: Cents(0.0),
    };
    assert_eq!(pitch.note_name(), "C#4");
    assert_eq!(pitch.note_name_flat(), "Db4");
}

#[test]
fn median_filter_removes_a_lone_outlier() {
    let voiced = |f: f32| {
        PitchEstimate::Voiced(Pitch {
            frequency: Hz(f),
            confidence: Confidence(0.9),
            note: Note::A4,
            cents_offset: Cents(0.0),
        })
    };
    let track = vec![
        voiced(440.0),
        voiced(440.0),
        voiced(880.0), // octave error
        voiced(440.0),
        voiced(440.0),
    ];

    let smoothed = median_filter(&track, FrameCount(3));
    assert_eq!(smoothed[2].frequency(), Hz(440.0), "outlier replaced");

    // A window of 1 or wider than the track is a no-op.
    assert_eq!(median_filter(&track, FrameCount(1)), track);
    assert_eq!(median_filter(&track, FrameCount(99)), track);
}

/// Accuracy across the range, not just at A440.
#[test]
fn tracks_frequencies_across_the_range() {
    let cfg = standard();

    for freq in [100.0f32, 220.0, 440.0, 880.0, 1000.0] {
        let samples = sine(44100.0, freq, 0.1);
        let estimate = pitch_of(&cfg, &samples).unwrap();
        let pitch = estimate
            .pitch()
            .unwrap_or_else(|| panic!("{freq} Hz unvoiced"));

        let error = ((pitch.frequency.get() - freq) / freq).abs() * 100.0;
        assert!(
            error < 2.0,
            "expected {freq} Hz, got {} ({error}% off)",
            pitch.frequency.get()
        );
    }
}

/// The extremes a configured range has to reach: a low guitar E and a high
/// soprano C. Both need the range widened past the standard default.
#[test]
fn reaches_the_ends_of_a_widened_range() {
    let cfg = YinConfig::new().band(FrequencyBand::new(Hz(40.0), Hz(2000.0)).unwrap());

    // E2, the guitar's low string.
    let low = pitch_of(&cfg, &sine(44100.0, 82.41, 0.2)).unwrap();
    let pitch = low.pitch().expect("E2 should be voiced");
    assert!(
        ((pitch.frequency.get() - 82.41) / 82.41).abs() * 100.0 < 3.0,
        "expected ~82.41 Hz, got {}",
        pitch.frequency.get()
    );
    assert_eq!(pitch.note, Note::new(tutti_music::PitchClass::E, 2));

    // C6.
    let high = pitch_of(&cfg, &sine(44100.0, 1046.5, 0.1)).unwrap();
    let pitch = high.pitch().expect("C6 should be voiced");
    assert!(
        ((pitch.frequency.get() - 1046.5) / 1046.5).abs() * 100.0 < 3.0,
        "expected ~1046.5 Hz, got {}",
        pitch.frequency.get()
    );
}

/// Jump penalization attenuates an octave error without deleting it.
#[test]
fn penalize_jumps_attenuates_implausible_leaps() {
    let voiced = |f: f32, c: f32| {
        let (note, cents) = Note::nearest_to(Hz(f)).unwrap();
        PitchEstimate::Voiced(Pitch {
            frequency: Hz(f),
            confidence: Confidence(c),
            note,
            cents_offset: cents,
        })
    };

    let track = vec![voiced(440.0, 0.9), voiced(880.0, 0.9), voiced(440.0, 0.9)];
    let smoothed = penalize_jumps(&track, Confidence(1.0));

    // The octave leap loses confidence; the frames around it do not.
    assert!(smoothed[1].confidence() < track[1].confidence());
    assert_eq!(smoothed[0].confidence(), track[0].confidence());
    // But it survives — the estimate may still be right.
    assert!(smoothed[1].is_voiced());

    // A zero penalty is the identity, and a steady track is untouched.
    assert_eq!(penalize_jumps(&track, Confidence::NONE), track);
    let steady = vec![voiced(440.0, 0.9), voiced(445.0, 0.9)];
    assert_eq!(penalize_jumps(&steady, Confidence(1.0)), steady);
}
