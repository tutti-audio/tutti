use super::*;

const RATE: SampleRate = SampleRate::SR_44K1;

fn stft(samples: &[f64], config: StftConfig) -> Result<Stft, TransformError> {
    super::stft(&config, Mono::new(samples), SampleRange::All, RATE)
}

fn stft_magnitude(samples: &[f64], config: StftConfig) -> Result<StftMagnitude, TransformError> {
    super::stft_magnitude(&config, Mono::new(samples), SampleRange::All, RATE)
}

fn stft_polar(samples: &[f64], config: StftConfig) -> Result<StftPolar, TransformError> {
    super::stft_polar(&config, Mono::new(samples), SampleRange::All, RATE)
}

fn span(start: usize, end: usize) -> Region {
    Region::new(Samples::new(start), Samples::new(end)).expect("an ascending span")
}

fn istft(transform: &Stft) -> Vec<f64> {
    super::istft(transform)
}

fn tone(len: usize, freq: f64, sample_rate: f64) -> Vec<f64> {
    tutti_fixtures::signal::sine(
        tutti_units::Hz::new(freq),
        tutti_units::Amplitude::new(0.5),
        tutti_units::SampleRate::of(sample_rate),
        tutti_units::Samples::new(len),
    )
}

#[test]
fn a_tone_lands_in_the_expected_bin() {
    let samples = tone(8192, 440.0, 44100.0);
    let result = stft(
        &samples,
        StftConfig::new(Samples::new(2048usize), Samples::new(512usize)),
    )
    .unwrap();

    let peak = (0..result.bins().bins().get())
        .max_by(|&a, &b| {
            result
                .magnitude_at(FrameIndex(1), BinIndex(a))
                .get()
                .partial_cmp(&result.magnitude_at(FrameIndex(1), BinIndex(b)).get())
                .unwrap()
        })
        .unwrap();

    let expected = result.geometry().bin_frequency(BinIndex(peak));
    assert!(
        (expected.get() - 440.0).abs() < 25.0,
        "peak bin maps to {expected}, expected ~440 Hz"
    );
}

/// The round trip the type split exists to protect.
#[test]
fn stft_round_trips_through_istft() {
    let samples = tone(8192, 440.0, 44100.0);
    let transform = stft(
        &samples,
        StftConfig::new(Samples::new(2048usize), Samples::new(512usize)),
    )
    .unwrap();
    let resynth = istft(&transform);

    // Compare the steady-state interior: the edges ramp in and out because
    // fewer windows overlap there.
    let window = transform.geometry().window().get();
    for i in window..resynth.len() - window {
        assert!(
            (samples[i] - resynth[i]).abs() < 1e-9,
            "sample {i}: {} != {}",
            samples[i],
            resynth[i]
        );
    }
}

/// A decimated request breaks COLA, so the invertible entry point refuses
/// it rather than producing non-overlapping frames for the inverse.
#[test]
fn a_decimated_request_is_refused_by_the_invertible_path() {
    let samples = tone(200_000, 440.0, 44100.0);
    let request = StftConfig::new(Samples::new(2048usize), Samples::new(512usize)).hop(
        HopPolicy::TargetFrames {
            frames: FrameCount(64),
            min_hop: Samples::new(1usize),
        },
    );

    // The hop this resolves to is far wider than the window.
    assert!(matches!(
        stft(&samples, request),
        Err(TransformError::Dsp(
            tutti_dsp::StftError::HopExceedsWindow { .. }
        )) | Err(TransformError::Dsp(
            tutti_dsp::StftError::NotColaCompliant { .. }
        ))
    ));

    // The display path accepts it and yields a type with no inverse.
    let display = stft_magnitude(&samples, request).unwrap();
    let frames = display.frames().get();
    assert!(
        (32..=128).contains(&frames),
        "{frames} frames for a target of 64"
    );
}

/// **The request's window shape reaches the resolved geometry**, on both
/// paths — otherwise `window_fn` would be accepted, stored, and ignored,
/// which is the shape of failure that leaves an analysis quietly on Hann.
#[test]
fn a_requests_window_shape_reaches_its_geometry() {
    let len = Samples::new(200_000);
    let request = StftConfig::new(Samples::new(2048usize), Samples::new(512usize))
        .window_fn(CosineWindow::HAMMING);

    assert_eq!(
        request.resolve(RATE, len).expect("valid").window_fn(),
        CosineWindow::HAMMING
    );
    assert_eq!(
        request
            .resolve_cola(RATE, len)
            .expect("Hamming COLAs at 4x")
            .window_fn(),
        CosineWindow::HAMMING
    );
}

/// A window needing more overlap than the request gives is refused by the
/// invertible path and accepted by the display path — the same split
/// decimation already gets, for the same reason.
///
/// 2048/512 is 4x: fine for Hann, short of Blackman's 8x.
#[test]
fn a_window_that_needs_more_overlap_is_refused_only_where_it_matters() {
    let samples = tone(200_000, 440.0, 44100.0);
    let request = StftConfig::new(Samples::new(2048usize), Samples::new(512usize))
        .window_fn(CosineWindow::BLACKMAN);

    assert!(matches!(
        stft(&samples, request),
        Err(TransformError::Dsp(
            tutti_dsp::StftError::NotColaCompliant {
                window_fn: CosineWindow::BLACKMAN,
                ..
            }
        ))
    ));

    // Analysable, just not invertible.
    assert!(stft_magnitude(&samples, request).is_ok());

    // And at the overlap Blackman asks for, it inverts.
    let deeper = StftConfig::new(Samples::new(2048usize), Samples::new(256usize))
        .window_fn(CosineWindow::BLACKMAN);
    assert!(stft(&samples, deeper).is_ok());
}

#[test]
fn normalized_magnitudes_carry_their_peak() {
    let samples = tone(4096, 440.0, 44100.0);
    let polar = stft_polar(
        &samples,
        StftConfig::new(Samples::new(1024usize), Samples::new(256usize)),
    )
    .unwrap();

    let raw = polar.magnitudes();
    let normalized = raw.normalize();

    assert_eq!(normalized.peak(), raw.peak());
    let max = normalized
        .grid()
        .as_slice()
        .iter()
        .copied()
        .fold(0.0f64, f64::max);
    assert!((max - 1.0).abs() < 1e-12, "normalized peak should be 1.0");
}

#[test]
fn a_range_analyzes_only_its_span() {
    let samples = tone(16384, 440.0, 44100.0);

    let whole = stft(
        &samples,
        StftConfig::new(Samples::new(2048usize), Samples::new(512usize)),
    )
    .unwrap();
    let part = super::stft(
        &StftConfig::new(Samples::new(2048usize), Samples::new(512usize)),
        Mono::new(&samples),
        SampleRange::Span(span(0, 8192)),
        RATE,
    )
    .unwrap();

    assert!(part.frames().get() < whole.frames().get());
    // A span past the end clamps rather than panicking.
    let clamped = super::stft(
        &StftConfig::new(Samples::new(2048usize), Samples::new(512usize)),
        Mono::new(&samples),
        SampleRange::Span(span(0, 99_999)),
        RATE,
    )
    .unwrap();
    assert_eq!(clamped.frames(), whole.frames());
}

#[test]
fn masking_requires_a_matching_shape() {
    let samples = tone(8192, 440.0, 44100.0);
    let transform = stft(
        &samples,
        StftConfig::new(Samples::new(2048usize), Samples::new(512usize)),
    )
    .unwrap();

    let wrong = Grid::filled(Complex64::default(), FrameCount(2), tutti_dsp::BinCount(3));
    assert_eq!(
        transform.resynthesize_masked::<f64>(&wrong),
        Err(TransformError::GridShapeDisagreement)
    );

    // A neutral mask is the identity.
    let neutral = Grid::filled(
        Complex64::new(1.0, 0.0),
        transform.bins().frames(),
        transform.bins().bins(),
    );
    let masked: Vec<f64> = transform.resynthesize_masked(&neutral).unwrap();
    let plain = istft(&transform);
    for (a, b) in masked.iter().zip(&plain) {
        assert!((a - b).abs() < 1e-12);
    }
}

#[test]
fn an_empty_mask_erases_the_signal() {
    let samples = tone(8192, 440.0, 44100.0);
    let transform = stft(
        &samples,
        StftConfig::new(Samples::new(2048usize), Samples::new(512usize)),
    )
    .unwrap();

    let silence = Grid::filled(
        Complex64::default(),
        transform.bins().frames(),
        transform.bins().bins(),
    );
    let erased: Vec<f64> = transform.resynthesize_masked(&silence).unwrap();
    for s in erased {
        assert!(s.abs() < 1e-9);
    }
}

#[test]
fn input_shorter_than_one_window_yields_no_frames() {
    let samples = tone(100, 440.0, 44100.0);
    let result = stft(
        &samples,
        StftConfig::new(Samples::new(2048usize), Samples::new(512usize)),
    )
    .unwrap();

    assert_eq!(result.frames(), FrameCount(0));
    assert!(istft(&result).is_empty());
}
