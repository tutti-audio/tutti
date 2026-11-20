use super::*;
use crate::fold_uneven;
use tutti_dsp::{HopSize, WindowLength};

const RATE: SampleRate = SampleRate::SR_22K05;

fn geometry(window: usize, hop: usize) -> StftGeometry {
    StftGeometry::new(
        RATE,
        WindowLength::new(Samples::new(window)).unwrap(),
        HopSize::new(Samples::new(hop)).unwrap(),
    )
}

fn framing(window: usize, hop: usize) -> Framing {
    Framing::new(Samples::new(window), Samples::new(hop))
}

fn sine(freq: f64, len: usize, amp: f64) -> Vec<f64> {
    (0..len)
        .map(|i| amp * (std::f64::consts::TAU * freq * i as f64 / 22_050.0).sin())
        .collect()
}

/// Largest error of the interpolated peak position, in bins, over a sweep
/// of a sine across one bin.
fn worst_offset_error(interp: impl Fn(f64, f64, f64) -> (f64, f64)) -> f64 {
    let g = geometry(4096, 4096);
    let (window, mut fft) = (g.window_coefficients(), FftScratch::new());
    let mut worst = 0.0f64;
    for step in 0..40 {
        let bin = 100.0 + f64::from(step) / 40.0;
        let x = sine(bin * 22_050.0 / 4096.0, 4096, 0.5);
        let mut spectrum = vec![Complex64::default(); 2049];
        fft.forward(Mono::new(&x), &window, &mut spectrum);
        let mags: Vec<f64> = spectrum.iter().map(|b| b.norm()).collect();
        let top = (1..2048).fold(1, |b, i| if mags[i] > mags[b] { i } else { b });
        let (offset, _) = interp(mags[top - 1], mags[top], mags[top + 1]);
        worst = worst.max((top as f64 + offset - bin).abs());
    }
    worst
}

/// Parabolic interpolation of the log magnitude places a Hann peak within
/// 0.02 bin (measured worst 0.016) wherever it falls in a bin; the raw
/// peak bin is up to half a bin off.
#[test]
fn interpolation_places_a_peak_to_a_fiftieth_of_a_bin() {
    assert!(worst_offset_error(interpolate) <= INTERP_BIAS_BINS);
    assert!(worst_offset_error(|_, m, _| (0.0, m)) > 10.0 * INTERP_BIAS_BINS);
}

/// The bin error bound above, as a fraction of a bin.
const INTERP_BIAS_BINS: f64 = 0.02;

fn run(x: &[f64], cfg: &ChromaConfig) -> Chroma {
    chroma(cfg, Mono::new(x), RATE).unwrap()
}

fn cfg() -> ChromaConfig {
    ChromaConfig::new()
}

/// Silence is all zero and finite, a sine under -90 dBFS is silence and one
/// above is not, and no frame is NaN.
#[test]
fn a_silent_frame_is_zero_not_nan() {
    let silent = run(&vec![0.0; 8192], &cfg());
    assert!(silent.grid().as_slice().iter().all(|&v| v == 0.0));
    assert_eq!(silent.tuning(), Cents::new(0.0));
    let quiet = run(&sine(440.0, 8192, 1e-5), &cfg());
    assert!(quiet.grid().as_slice().iter().all(|&v| v == 0.0));
    let audible = run(&sine(440.0, 8192, 1e-3), &cfg());
    assert!(audible.grid().as_slice().iter().any(|&v| v > 0.0));
}

/// A pitched frame's largest column is exactly 1, whatever the level.
#[test]
fn a_frame_is_normalized_by_its_largest_column() {
    for level in [0.1, 0.9] {
        let c = run(&sine(440.0, 8192, level), &cfg());
        for row in c.grid().rows() {
            assert_eq!(row.iter().copied().fold(0.0, f64::max), 1.0);
            assert!(row.iter().all(|&v| (0.0..=1.0).contains(&v)));
        }
    }
}

/// A sine at G4 (392 Hz) votes for G (itself and its octave, 196 Hz) and
/// C (a third of it, 130.7 Hz); the fourth subharmonic, 98 Hz, is under the
/// band's 100 Hz floor and does not vote. With decay 0.6 the energies are
/// G = 1 + 0.6, C = 0.6^2, so C reads `0.36 / 1.6 = 0.225`.
#[test]
fn a_peak_votes_for_its_subharmonics_down_to_the_band_floor() {
    let c = run(&sine(392.0, 8192, 0.5), &cfg()).mean();
    assert!((c[7] - 1.0).abs() < 1e-9);
    assert!((c[0] - 0.225).abs() < 0.01, "{c:?}");
    for (i, &v) in c.iter().enumerate() {
        if i != 0 && i != 7 {
            assert!(v < 0.01, "{c:?}");
        }
    }
}

/// Partials outside the band do not fold.
#[test]
fn peaks_outside_the_band_are_not_folded() {
    for freq in [80.0, 6000.0] {
        let c = run(&sine(freq, 8192, 0.5), &cfg());
        assert!(c.grid().as_slice().iter().all(|&v| v == 0.0), "{freq}");
    }
}

/// A subharmonic's weight is the decay per order: at 0.3 the G4 sine's C
/// (a third of it) reads `0.09 / 1.3`.
#[test]
fn the_harmonic_decay_weighs_the_subharmonic_votes() {
    let cfg = cfg().harmonic_decay(HarmonicDecay::of(0.3));
    let c = run(&sine(392.0, 8192, 0.5), &cfg).mean();
    assert!((c[7] - 1.0).abs() < 1e-9);
    assert!((c[0] - 0.09 / 1.3).abs() < 0.01, "{c:?}");
}

/// A peak under the floor, relative to the frame's strongest, is ignored:
/// a sine 50 dB under another votes at a -80 dB floor and not at -40.
#[test]
fn peaks_under_the_floor_do_not_vote() {
    let strong = sine(440.0, 8192, 0.5);
    let x: Vec<f64> = strong
        .iter()
        .zip(sine(330.0, 8192, 0.5 * 10f64.powf(-50.0 / 20.0)))
        .map(|(a, b)| a + b)
        .collect();
    let only = cfg().harmonics(Harmonics::of(1));
    let ignored = run(&x, &only.peak_floor(PeakFloor::of(Db::new(-40.0)))).mean();
    assert_eq!(ignored[4], 0.0);
    let heard = run(&x, &only.peak_floor(PeakFloor::of(Db::new(-80.0)))).mean();
    assert!(heard[4] > 0.0, "{heard:?}");
}

/// The band limits which peaks fold: a 440 Hz sine is heard in a band
/// around it and not in one above it.
#[test]
fn the_band_selects_the_peaks_that_fold() {
    let x = sine(440.0, 8192, 0.5);
    let inside = cfg().band(FrequencyBand::new(Hz::new(300.0), Hz::new(600.0)).unwrap());
    assert_eq!(run(&x, &inside).mean()[9], 1.0);
    let above = cfg().band(FrequencyBand::new(Hz::new(600.0), Hz::new(2000.0)).unwrap());
    assert!(run(&x, &above).grid().as_slice().iter().all(|&v| v == 0.0));
}

/// A sine's tuning is its distance from the grid, within the interpolation
/// bias at its frequency: 0.02 bin is `1200 log2(1 + 0.02 w / f)` cents,
/// `w = 22050 / 4096` Hz. The mean wraps: +45 and -55 are the same.
#[test]
fn the_tuning_of_a_sine_is_its_distance_from_the_grid() {
    let width = 22_050.0 / 4096.0;
    let tolerance = 1200.0 * (1.0 + INTERP_BIAS_BINS * width / 440.0).log2();
    for cents in [-40.0, -20.0, 0.0, 20.0, 40.0, 55.0] {
        let freq = 440.0 * (cents / 1200.0f64).exp2();
        let got = run(&sine(freq, 8192, 0.5), &cfg()).tuning().get();
        let want = if cents > 50.0 { cents - 100.0 } else { cents };
        assert!((got - want).abs() <= tolerance, "{cents}: {got}");
    }
}

/// The tuning is subtracted before a peak is rounded to a class: a sine
/// 30 cents above A folds to A at +30 and to A# at -30 (60 cents up).
#[test]
fn a_fixed_tuning_moves_the_fold() {
    let x = sine(440.0 * (30.0f64 / 1200.0).exp2(), 8192, 0.5);
    let at = |cents: f64| {
        let cfg = cfg().tuning(Tuning::Fixed(TuningOffset::new(Cents::new(cents)).unwrap()));
        let c = run(&x, &cfg);
        assert_eq!(c.tuning(), Cents::new(cents));
        c.mean()
    };
    assert_eq!(at(30.0)[9], 1.0);
    assert_eq!(at(-30.0)[10], 1.0);
}

/// A meter fed chunks of 1, an odd count, a window, a hop-and-a-bit and the
/// whole buffer at the batch's tuning gives the batch's last frame, mean
/// and frame count to the bit.
#[test]
fn a_meter_at_a_fixed_tuning_is_the_one_shot() {
    let x: Vec<f64> = sine(440.0, 12_000, 0.5)
        .iter()
        .zip(sine(660.0, 12_000, 0.3))
        .map(|(a, b)| a + b)
        .collect();
    let batch = run(&x, &cfg());
    let fixed = cfg().tuning(Tuning::Fixed(TuningOffset::new(batch.tuning()).unwrap()));
    // At the batch's tuning, the one-shot is the meter's reference.
    assert_eq!(run(&x, &fixed).grid(), batch.grid());
    for chunk in [1usize, 777, 4096, 5000, x.len()] {
        let mut meter = fixed.measure(RATE).unwrap();
        let mut items = Vec::new();
        for part in x.chunks(chunk) {
            meter.push(Mono::new(part), &mut items).unwrap();
        }
        assert_eq!(items.len(), batch.grid().frames().get(), "chunk {chunk}");
        assert!(
            items
                .iter()
                .zip(batch.grid().rows())
                .all(|(a, b)| a.bins == b)
        );
        assert_eq!(meter.mean(), batch.mean(), "chunk {chunk}");
        assert_eq!(meter.tuning(), batch.tuning(), "chunk {chunk}");
    }
    let mut meter = fixed.measure(RATE).unwrap();
    meter.push(Mono::new(&x), &mut Vec::new()).unwrap();
    meter.reset();
    assert_eq!(meter.mean(), [0.0; CLASSES]);
    assert_eq!(meter.tuning(), batch.tuning());
}

/// Under an estimated tuning a meter's reading depends on what it has
/// seen, never on how the stream was cut: chunks of 1, an odd count and the
/// whole buffer give the same mean and tuning bit for bit.
#[test]
fn an_estimating_meter_does_not_depend_on_the_chunking() {
    let x: Vec<f64> = sine(440.0 * (25.0f64 / 1200.0).exp2(), 12_000, 0.5)
        .iter()
        .zip(sine(660.0, 12_000, 0.3))
        .map(|(a, b)| a + b)
        .collect();
    let read = |chunk: usize| {
        let mut meter = cfg().measure(RATE).unwrap();
        for part in x.chunks(chunk) {
            meter.push(Mono::new(part), &mut Vec::new()).unwrap();
        }
        (meter.mean(), meter.tuning())
    };
    let whole = read(x.len());
    assert!(whole.1.get() != 0.0, "the estimate read the peaks");
    for chunk in [1usize, 777] {
        assert_eq!(read(chunk), whole, "chunk {chunk}");
    }
}

/// With an estimated tuning the meter reads the peaks seen so far.
#[test]
fn a_running_estimate_follows_the_peaks_seen() {
    let x = sine(440.0 * (25.0f64 / 1200.0).exp2(), 4096, 0.5);
    let mut meter = cfg().measure(RATE).unwrap();
    assert_eq!(meter.tuning(), Cents::new(0.0));
    let mut frames = Vec::new();
    meter.push(Mono::new(&x), &mut frames).unwrap();
    assert_eq!(frames.len(), 1);
    assert!((meter.tuning().get() - 25.0).abs() < 1.0);
}

/// A refused chunk leaves the carry alone, and `reset` reads like a fresh
/// meter.
#[test]
fn a_refused_chunk_keeps_the_carry_and_reset_reads_fresh() {
    let x = sine(440.0 * (25.0f64 / 1200.0).exp2(), 4096, 0.5);
    let mut meter = cfg().measure(RATE).unwrap();
    meter.push(Mono::new(&x), &mut Vec::new()).unwrap();
    let (mean, tuning) = (meter.mean(), meter.tuning());
    let mut nan = x.clone();
    nan[7] = f64::NAN;
    assert_eq!(
        meter.push(Mono::new(&nan), &mut Vec::new()),
        Err(Error::NonFiniteInput)
    );
    assert_eq!((meter.mean(), meter.tuning()), (mean, tuning));

    meter.reset();
    assert_eq!(meter.mean(), [0.0; CLASSES]);
    assert_eq!(meter.tuning(), Cents::new(0.0));
}

/// The framing is validated against what the fold can afford, and the band
/// against the bins it holds, where the rate binds.
#[test]
fn the_framing_and_band_are_validated() {
    let at = |window, hop| Measure::<f64>::measure(&cfg().framing(framing(window, hop)), RATE);
    assert!(matches!(at(32, 8), Err(Error::WindowOutOfRange { .. })));
    assert!(matches!(
        at(1 << 19, 1 << 17),
        Err(Error::WindowOutOfRange { .. })
    ));
    assert!(matches!(at(4096, 32), Err(Error::TooManyOverlaps { .. })));
    assert!(at(4096, 64).is_ok());
    assert!(matches!(at(0, 8), Err(Error::ZeroWindow | Error::ZeroHop)));
    assert!(matches!(
        at(4096, 0),
        Err(Error::ZeroWindow | Error::ZeroHop)
    ));
    for (lo, hi) in [
        (500.0, 100.0),
        (0.0, 1000.0),
        (f64::NAN, 1000.0),
        (100.0, f64::NAN),
        (100.0, f64::INFINITY),
    ] {
        assert!(
            matches!(
                FrequencyBand::try_from(Hz::new(lo)..=Hz::new(hi)),
                Err(Error::EmptyFrequencyRange { .. })
            ),
            "{lo} {hi}"
        );
    }
    // The rate decides what the terminals accept: a band of under three
    // bins, or one above Nyquist.
    let x = sine(440.0, 8192, 0.5);
    let narrow = cfg().band(FrequencyBand::new(Hz::new(100.0), Hz::new(110.0)).unwrap());
    assert!(matches!(
        chroma(&narrow, Mono::new(&x), RATE),
        Err(Error::EmptyFrequencyRange { .. })
    ));
    let wide = cfg().band(FrequencyBand::new(Hz::new(100.0), Hz::new(12_000.0)).unwrap());
    assert!(matches!(
        chroma(&wide, Mono::new(&x), RATE),
        Err(Error::AboveNyquist { .. })
    ));
    assert!(matches!(
        Measure::<f64>::measure(&wide, RATE),
        Err(Error::AboveNyquist { .. })
    ));
    // A low rate clamps the default band to Nyquist instead of failing.
    let low = cfg()
        .framing(framing(1024, 256))
        .plan(SampleRate::SR_8K)
        .unwrap();
    assert_eq!(low.max_freq, Hz::new(4000.0));
}

/// Bad input is an error, and a refused call computes nothing.
#[test]
fn bad_input_is_refused() {
    let mut x = sine(440.0, 8192, 0.5);
    let one_shot = |x: &[f64], cfg: &ChromaConfig| chroma(cfg, Mono::new(x), RATE);
    x[100] = f64::NAN;
    assert_eq!(one_shot(&x, &cfg()).unwrap_err(), Error::NonFiniteInput);
    x[100] = f64::INFINITY;
    assert_eq!(one_shot(&x, &cfg()).unwrap_err(), Error::NonFiniteInput);
    x[100] = 0.25;
    assert_eq!(
        one_shot(&x[..4095], &cfg()).unwrap_err(),
        Error::InsufficientInput {
            needed: Samples::new(4096),
            got: Samples::new(4095)
        }
    );
    let tiny = cfg().framing(framing(64, 1));
    let long = vec![0.0; MAX_FRAMES.get() + 64];
    assert!(matches!(
        one_shot(&long, &tiny),
        Err(Error::TooManyFrames { .. })
    ));
    // A NaN between two frames is in no window but is still refused.
    let sparse = cfg().framing(framing(64, 128));
    let mut gap = sine(440.0, 512, 0.5);
    gap[100] = f64::NAN;
    assert_eq!(one_shot(&gap, &sparse).unwrap_err(), Error::NonFiniteInput);
}

/// A 16-bit buffer folds at its nominal level: the chroma of the integer
/// copy of a sine peaks on the sine's class.
#[test]
fn an_integer_buffer_folds_at_its_nominal_level() {
    let x: Vec<i16> = sine(440.0, 8192, 0.5)
        .iter()
        .map(|&v| (v * f64::from(i16::MAX)) as i16)
        .collect();
    let c = chroma(&cfg(), Mono::new(&x), RATE).unwrap();
    assert_eq!(c.mean()[9], 1.0);
}

/// The trait fold over uneven chunks (1, 7, 64 samples and the rest) yields
/// the chroma rows of `chroma` as its items, and their mean and the
/// tuning as its output, at a fixed tuning.
#[test]
fn the_trait_fold_over_uneven_chunks_equals_the_chroma_rows() {
    let x = sine(440.0, 30_000, 0.5);
    let fixed = cfg().tuning(Tuning::Fixed(TuningOffset::new(Cents::new(0.0)).unwrap()));
    let batch = run(&x, &fixed);
    let (items, summary) = fold_uneven(&fixed, RATE, x.len(), |meter, at, out| {
        meter.push(Mono::new(&x[at]), out)
    })
    .unwrap();
    assert_eq!(items.len(), batch.grid().frames().get());
    assert!(items.len() > 3);
    assert!(
        items
            .iter()
            .zip(batch.grid().rows())
            .all(|(a, b)| a.bins == b)
    );
    let hop = batch.framing().hop().get();
    assert!(
        items
            .iter()
            .enumerate()
            .all(|(i, f)| f.frame == Samples::new(i * hop)),
        "each frame names the sample it starts at"
    );
    assert_eq!(summary.mean, batch.mean());
    assert_eq!(summary.tuning, batch.tuning());
}
