use super::*;

const RATE: SampleRate = SampleRate::SR_44K1;

fn stft(samples: &[f32], request: StftRequest, fft: &mut FftScratch) -> Result<Stft> {
    super::stft(request, Mono::new(samples), RATE, fft)
}

fn stft_magnitude(
    samples: &[f32],
    request: StftRequest,
    fft: &mut FftScratch,
) -> Result<StftMagnitude> {
    super::stft_magnitude(request, Mono::new(samples), RATE, fft)
}

fn stft_polar(samples: &[f32], request: StftRequest, fft: &mut FftScratch) -> Result<StftPolar> {
    super::stft_polar(request, Mono::new(samples), RATE, fft)
}

fn istft(transform: &Stft, fft: &mut FftScratch) -> Vec<f32> {
    super::istft(transform, fft)
}

fn tone(len: usize, freq: f32, sample_rate: f32) -> Vec<f32> {
    tutti_fixtures::signal::sine(
        tutti_types::Hz(freq),
        tutti_types::Amplitude(0.5),
        tutti_types::SampleRate::of(f64::from(sample_rate)),
        tutti_types::Samples(len),
    )
}

#[test]
fn a_tone_lands_in_the_expected_bin() {
    let samples = tone(8192, 440.0, 44100.0);
    let mut fft = FftScratch::new();
    let result = stft(
        &samples,
        StftRequest::new(Samples::new(2048usize), Samples::new(512usize)),
        &mut fft,
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
    let mut fft = FftScratch::new();
    let transform = stft(
        &samples,
        StftRequest::new(Samples::new(2048usize), Samples::new(512usize)),
        &mut fft,
    )
    .unwrap();
    let resynth = istft(&transform, &mut fft);

    // Compare the steady-state interior: the edges ramp in and out because
    // fewer windows overlap there.
    let window = transform.geometry().window().get();
    for i in window..resynth.len() - window {
        assert!(
            (samples[i] - resynth[i]).abs() < 1e-3,
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
    let mut fft = FftScratch::new();
    let request = StftRequest::new(Samples::new(2048usize), Samples::new(512usize)).hop(
        HopPolicy::TargetFrames {
            frames: FrameCount(64),
            min_hop: Samples::new(1usize),
        },
    );

    // The hop this resolves to is far wider than the window.
    assert!(matches!(
        stft(&samples, request, &mut fft),
        Err(Error::Dsp(tutti_dsp::StftError::HopExceedsWindow { .. }))
            | Err(Error::Dsp(tutti_dsp::StftError::NotColaCompliant { .. }))
    ));

    // The display path accepts it and yields a type with no inverse.
    let display = stft_magnitude(&samples, request, &mut fft).unwrap();
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
    let len = Samples(200_000);
    let request = StftRequest::new(Samples::new(2048usize), Samples::new(512usize))
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
    let mut fft = FftScratch::new();
    let request = StftRequest::new(Samples::new(2048usize), Samples::new(512usize))
        .window_fn(CosineWindow::BLACKMAN);

    assert!(matches!(
        stft(&samples, request, &mut fft),
        Err(Error::Dsp(tutti_dsp::StftError::NotColaCompliant {
            window_fn: CosineWindow::BLACKMAN,
            ..
        }))
    ));

    // Analysable, just not invertible.
    assert!(stft_magnitude(&samples, request, &mut fft).is_ok());

    // And at the overlap Blackman asks for, it inverts.
    let deeper = StftRequest::new(Samples::new(2048usize), Samples::new(256usize))
        .window_fn(CosineWindow::BLACKMAN);
    assert!(stft(&samples, deeper, &mut fft).is_ok());
}

#[test]
fn normalized_magnitudes_carry_their_peak() {
    let samples = tone(4096, 440.0, 44100.0);
    let mut fft = FftScratch::new();
    let polar = stft_polar(
        &samples,
        StftRequest::new(Samples::new(1024usize), Samples::new(256usize)),
        &mut fft,
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
        .fold(0.0f32, f32::max);
    assert!((max - 1.0).abs() < 1e-5, "normalized peak should be 1.0");
}

#[test]
fn a_range_analyzes_only_its_span() {
    let samples = tone(16384, 440.0, 44100.0);
    let mut fft = FftScratch::new();

    let whole = stft(
        &samples,
        StftRequest::new(Samples::new(2048usize), Samples::new(512usize)),
        &mut fft,
    )
    .unwrap();
    let part = stft(
        &samples,
        StftRequest::new(Samples::new(2048usize), Samples::new(512usize)).range(
            SampleRange::Span {
                start: Samples::new(0usize),
                end: Samples::new(8192usize),
            },
        ),
        &mut fft,
    )
    .unwrap();

    assert!(part.frames().get() < whole.frames().get());
    // A span past the end clamps rather than panicking.
    let clamped = stft(
        &samples,
        StftRequest::new(Samples::new(2048usize), Samples::new(512usize)).range(
            SampleRange::Span {
                start: Samples::new(0usize),
                end: Samples::new(99_999usize),
            },
        ),
        &mut fft,
    )
    .unwrap();
    assert_eq!(clamped.frames(), whole.frames());
}

#[test]
fn masking_requires_a_matching_shape() {
    let samples = tone(8192, 440.0, 44100.0);
    let mut fft = FftScratch::new();
    let transform = stft(
        &samples,
        StftRequest::new(Samples::new(2048usize), Samples::new(512usize)),
        &mut fft,
    )
    .unwrap();

    let wrong = Grid::filled(Complex32::default(), FrameCount(2), tutti_dsp::BinCount(3));
    assert_eq!(
        transform.resynthesize_masked::<f32>(&wrong, &mut fft),
        Err(Error::GridShapeDisagreement)
    );

    // A neutral mask is the identity.
    let neutral = Grid::filled(
        Complex32::new(1.0, 0.0),
        transform.bins().frames(),
        transform.bins().bins(),
    );
    let masked: Vec<f32> = transform.resynthesize_masked(&neutral, &mut fft).unwrap();
    let plain = istft(&transform, &mut fft);
    for (a, b) in masked.iter().zip(&plain) {
        assert!((a - b).abs() < 1e-6);
    }
}

#[test]
fn an_empty_mask_erases_the_signal() {
    let samples = tone(8192, 440.0, 44100.0);
    let mut fft = FftScratch::new();
    let transform = stft(
        &samples,
        StftRequest::new(Samples::new(2048usize), Samples::new(512usize)),
        &mut fft,
    )
    .unwrap();

    let silence = Grid::filled(
        Complex32::default(),
        transform.bins().frames(),
        transform.bins().bins(),
    );
    let erased: Vec<f32> = transform.resynthesize_masked(&silence, &mut fft).unwrap();
    for s in erased {
        assert!(s.abs() < 1e-6);
    }
}

#[test]
fn input_shorter_than_one_window_yields_no_frames() {
    let samples = tone(100, 440.0, 44100.0);
    let mut fft = FftScratch::new();
    let result = stft(
        &samples,
        StftRequest::new(Samples::new(2048usize), Samples::new(512usize)),
        &mut fft,
    )
    .unwrap();

    assert_eq!(result.frames(), FrameCount(0));
    assert!(istft(&result, &mut fft).is_empty());
}
