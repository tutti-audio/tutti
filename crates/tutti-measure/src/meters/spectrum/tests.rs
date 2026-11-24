use super::*;
use crate::fold_uneven;
use crate::test_rng::uniform;
use crate::{AboveNyquist, FramingError};
use std::f64::consts::TAU;
use tutti_dsp::{HopSize, WindowLength};
use tutti_fixtures::signal::sine;
use tutti_rt::SplitMix64;
use tutti_units::Hz;
use tutti_units::Samples;
use tutti_units::{Amplitude, SampleRate};

const RATE: f64 = 48_000.0;

/// The `f32` test signals' rounding error budget, in dB: a relative amplitude
/// error of `1e-4` (about 100x `eps * log2(N)` for the sizes here) is
/// `20 log10(1 + 1e-4)`.
const FFT_TOL: f64 = 1e-3;

fn band(min: f64, max: f64) -> FrequencyBand {
    FrequencyBand::new(Hz::new(min), Hz::new(max)).unwrap()
}

fn geometry(size: usize, hop: usize) -> StftGeometry {
    StftGeometry::new(
        SampleRate::of(RATE),
        WindowLength::new(Samples::new(size)).unwrap(),
        HopSize::new(Samples::new(hop)).unwrap(),
    )
}

fn config(size: usize, hop: usize) -> SpectrumConfig {
    SpectrumConfig::new().framing(Framing::new(Samples::new(size), Samples::new(hop)))
}

fn plan(cfg: &SpectrumConfig) -> SpectrumPlan {
    cfg.plan(SampleRate::of(RATE)).unwrap()
}

fn meter(cfg: &SpectrumConfig) -> SpectrumMeter<f32> {
    cfg.measure(SampleRate::of(RATE)).unwrap()
}

/// Pushes a finite block and returns whether it completed a frame.
fn push_frames(state: &mut SpectrumMeter<f32>, block: &[f32]) -> bool {
    let mut frames = Vec::new();
    state.push(Mono::new(block), &mut frames).unwrap();
    !frames.is_empty()
}

/// Feeds `signal` a hop at a time (so at most one frame completes per
/// call), returning the state and the levels after each frame.
fn run(cfg: &SpectrumConfig, signal: &[f32]) -> (SpectrumMeter<f32>, Vec<Vec<Db>>) {
    let mut state = meter(cfg);
    let mut frames = Vec::new();
    for block in signal.chunks(plan(cfg).geometry.hop().get()) {
        if push_frames(&mut state, block) {
            frames.push(state.levels().to_vec());
        }
    }
    (state, frames)
}

fn argmax(levels: &[Db]) -> usize {
    (0..levels.len())
        .max_by(|&a, &b| levels[a].get().total_cmp(&levels[b].get()))
        .unwrap()
}

/// `len` samples of an `amp` sine at `freq` hertz, from phase 0.
fn tone(len: usize, freq: f64, amp: f64) -> Vec<f32> {
    sine(
        Hz::new(freq),
        Amplitude::new(amp),
        SampleRate::of(RATE),
        Samples::new(len),
    )
}

/// A sine centred on a bin reads `20 log10(a)` there. The periodic Hann's
/// coherent gain is 1/2, so `|X| = a N / 4` and `4 |X|^2 / (N/2)^2 = a^2`;
/// the window puts half the amplitude (6.0206 dB down) on each neighbour
/// and nothing beyond them.
#[test]
fn a_bin_centred_sine_reads_its_amplitude() {
    let size = 1536; // 1 kHz is bin 32 at 48 kHz
    let cfg = config(size, size / 4);
    let (state, frames) = run(&cfg, &tone(size * 3, 1000.0, 0.5));
    let want = Db::from_amplitude(Amplitude::new(0.5)).get();
    for levels in &frames {
        assert_eq!(argmax(levels), 32);
        assert!(
            (levels[32].get() - want).abs() < FFT_TOL,
            "{:?}",
            levels[32]
        );
        for k in [31, 33] {
            assert!(
                (levels[k].get() - (want - 6.0206)).abs() < FFT_TOL,
                "bin {k}: {:?}",
                levels[k]
            );
        }
        // Leakage of a periodic Hann at a bin centre is exactly zero; what
        // is left is the `f32` input's rounding, far under -100 dB.
        assert!(levels[29].get() < -100.0 && levels[35].get() < -100.0);
    }
    assert_eq!(state.frequencies()[32], Hz::new(1000.0));
}

/// Off the centre the peak loses `|sinc(d) / (1 - d^2)|`, the Hann
/// window's response at an offset of `d` bins, at most 1.4236 dB at half a
/// bin. The tone's image at `-f` is 64 bins away, where the response is
/// under -118 dB, so it adds under `1e-5` dB to the bound.
#[test]
fn an_off_centre_sine_loses_the_scalloping_figure() {
    let size = 1536;
    let cfg = config(size, size);
    let loss = |d: f64| {
        let sinc = (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d);
        Db::from_amplitude(Amplitude::new((sinc / (1.0 - d * d)).abs())).get()
    };
    assert!((loss(0.5) - -1.4236).abs() < 1e-3);
    for d in [0.1, 0.25, 0.4, 0.5] {
        let freq = (32.0 + d) * RATE / size as f64;
        let (_, frames) = run(&cfg, &tone(size, freq, 0.5));
        let levels = &frames[0];
        let peak = levels[argmax(levels)].get();
        let want = Db::from_amplitude(Amplitude::new(0.5)).get() + loss(d);
        assert!((peak - want).abs() < FFT_TOL, "d {d}: {peak} vs {want}");
        assert!(peak >= Db::from_amplitude(Amplitude::new(0.5)).get() - 1.4237 - FFT_TOL);
    }
}

/// Two tones resolve to two peaks at their bins and levels.
///
/// A mis-unrolled ring is not caught here, and why: a tone of whole cycles per window is the same
/// after a rotation of the ring, so a mis-unrolled ring leaves these levels
/// alone; `frames_land_on_the_hop_grid_for_every_size_and_hop` catches it.
#[test]
fn two_tones_resolve_to_two_peaks() {
    let size = 1536;
    let cfg = config(size, size / 2);
    let (a, b) = (tone(size * 2, 1000.0, 0.5), tone(size * 2, 5000.0, 0.1));
    let mix: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
    let (_, frames) = run(&cfg, &mix);
    for levels in &frames {
        let peaks: Vec<usize> = (1..levels.len() - 1)
            .filter(|&k| {
                levels[k] > levels[k - 1] && levels[k] >= levels[k + 1] && levels[k].get() > -60.0
            })
            .collect();
        assert_eq!(peaks, [32, 160]);
        assert!((levels[32].get() - Db::from_amplitude(Amplitude::new(0.5)).get()).abs() < FFT_TOL);
        assert!(
            (levels[160].get() - Db::from_amplitude(Amplitude::new(0.1)).get()).abs() < FFT_TOL
        );
    }
}

/// Frames start at `k * hop` and end at `k * hop + size` for every size and
/// hop, including a hop that does not divide the window, whatever the
/// block: a unit impulse at `p` reads `w[p - k hop]` flat across the bins of
/// each frame that holds it (`10 log10(4 w^2 / (N/2)^2)`), and the floor
/// in the rest, and a frame is ready exactly at `k hop + size` samples.
#[test]
fn frames_land_on_the_hop_grid_for_every_size_and_hop() {
    for (size, hop) in [
        (64, 64),
        (64, 1),
        (100, 30),
        (256, 100),
        (257, 64),
        (1024, 256),
    ] {
        let cfg = config(size, hop);
        let impulse_at = size + 3 * hop + 5;
        let len = impulse_at + 2 * size;
        let mut signal = vec![0.0f32; len];
        signal[impulse_at] = 1.0;

        let mut state = meter(&cfg);
        let window = geometry(size, hop).window_coefficients::<f32>();
        let gain = geometry(size, hop).magnitude_gain::<f64>();
        for (i, s) in signal.iter().enumerate() {
            let ready = push_frames(&mut state, &[*s]);
            let fed = i + 1;
            let due = fed >= size && (fed - size) % hop == 0;
            assert_eq!(ready, due, "{size}/{hop} after {fed}");
            if !ready {
                continue;
            }
            let start = fed - size;
            let level = match impulse_at.checked_sub(start).filter(|&o| o < size) {
                Some(o) if window[o] > 0.0 => {
                    Db::from_power(4.0 * f64::from(window[o]).powi(2) / (gain * gain)).get()
                }
                _ => plan(&cfg).floor.get().get(),
            };
            for (k, l) in state.levels().iter().enumerate() {
                // DC and an even window's Nyquist drop the factor 4.
                let lone = k == 0 || (size.is_multiple_of(2) && k == size / 2);
                let level = if lone && level > plan(&cfg).floor.get().get() {
                    level - 6.0206
                } else {
                    level
                };
                assert!(
                    (l.get() - level).abs() < FFT_TOL,
                    "{size}/{hop} frame at {start}, bin {k}: {l:?} vs {level}"
                );
            }
        }
    }
}

/// The same stream cut into blocks of 1, 7, a hop, a prime and the whole
/// gives bit-identical levels, peaks, spectrogram and frame count.
#[test]
fn blocks_of_any_size_give_identical_frames() {
    let cfg = config(256, 100)
        .smoothing(Smoothing::Release(SpectrumTime::of(Seconds::new(0.05))))
        .banding(Banding::Octave {
            per_octave: BandsPerOctave::of(1),
            band: band(375.0, 12_000.0),
        })
        .spectrogram(FrameCount(6));
    let mut rng = SplitMix64::new(7);
    let signal: Vec<f32> = (0..9_000).map(|_| uniform(&mut rng, -1.0, 1.0)).collect();
    let read = |block: usize| {
        let mut state = meter(&cfg);
        for chunk in signal.chunks(block) {
            push_frames(&mut state, chunk);
        }
        let cols: Vec<Vec<Db>> = state.spectrogram().map(<[Db]>::to_vec).collect();
        (state.levels().to_vec(), state.peaks().to_vec(), cols)
    };
    let whole = read(signal.len());
    assert_eq!(whole.2.len(), 6);
    for block in [1, 7, 100, 1013, 4096] {
        assert_eq!(read(block), whole, "block {block}");
    }
}

/// White noise of variance `s^2` has `E|X_k|^2 = s^2 sum(w^2) = 3 N s^2 / 8`
/// for Hann, so the level of the mean power is `10 log10(6 s^2 / N)`.
/// Non-overlapping frames are independent, and bins 3 apart are uncorrelated
/// (the transform of `w^2` has three terms), so each smoothed bin power is
/// an exponential average of independent exponentials with relative
/// variance `a / (2 - a)`, and the mean of `B` such bins has `a / ((2 - a) B)`.
/// The tolerance is 5 sigma of that.
///
/// The time series of that mean has a spread near `sqrt(a / (2 - a) / B)`
/// against `1 / sqrt(B)` unsmoothed; asserting it under half the latter
/// is what an averaging that does not average fails.
///
/// Not covered: the first-frame seed, whose transient is under the
/// tolerance after 600 frames.
#[test]
fn averaging_converges_to_the_mean_noise_power() {
    let size = 1024;
    let tau = Seconds::new(1.0);
    let cfg = config(size, size).smoothing(Smoothing::Average(SpectrumTime::of(tau)));
    let sigma = 0.1f64;
    let mut rng = SplitMix64::new(0x5eed);
    let mut gauss = || {
        let (u, v): (f64, f64) = (uniform(&mut rng, 1e-12, 1.0), uniform(&mut rng, 0.0, 1.0));
        sigma * (-2.0 * u.ln()).sqrt() * (TAU * v).cos()
    };
    let frames = 600;
    let signal: Vec<f32> = (0..size * frames).map(|_| gauss() as f32).collect();

    let mut state = meter(&cfg);
    let picked: Vec<usize> = (3..size / 2 - 3).step_by(3).collect();
    let want = 6.0 * sigma * sigma / size as f64;
    let mut stats = Vec::new();
    for block in signal.chunks(size) {
        push_frames(&mut state, block);
        let mean = picked
            .iter()
            .map(|&k| 10f64.powf(state.levels()[k].get() / 10.0))
            .sum::<f64>()
            / picked.len() as f64;
        stats.push(mean / want);
    }
    let alpha = Ballistics::new(tau, tau)
        .expect("a valid time")
        .coeff_over(true, plan(&cfg).geometry.frame_interval());
    let b = picked.len() as f64;
    let rel_sigma = (alpha / (2.0 - alpha) / b).sqrt();
    let last = *stats.last().unwrap();
    assert!(
        (last - 1.0).abs() < 5.0 * rel_sigma,
        "mean power ratio {last}, 5 sigma {}",
        5.0 * rel_sigma
    );

    let tail = &stats[stats.len() - 300..];
    let mean = tail.iter().sum::<f64>() / tail.len() as f64;
    let spread = (tail.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
    assert!(spread < 0.5 / b.sqrt(), "spread {spread}");
}

/// With instant attack and release time `tau`, power after the signal ends
/// falls by `exp(-dt / tau)` per frame, so the level falls by
/// `10 log10(e) dt / tau` dB per frame. A rise reads at once: a frame wholly
/// inside the loud part reads the sine's level whatever came before.
#[test]
fn release_follows_the_analytic_exponential_and_attack_is_instant() {
    let (size, hop) = (1536, 512);
    let tau = Seconds::new(0.1);
    let cfg = config(size, hop).smoothing(Smoothing::Release(SpectrumTime::of(tau)));
    let (quiet, loud_end) = (size * 2, size * 6);
    let mut signal = tone(loud_end, 1000.0, 0.5);
    signal[..quiet].iter_mut().for_each(|s| *s *= 0.02);
    signal.resize(loud_end + size * 4, 0.0);
    let (_, frames) = run(&cfg, &signal);
    let at = |k: usize| frames[k][32].get();

    let want = Db::from_amplitude(Amplitude::new(0.5)).get();
    let first_loud = quiet.div_ceil(hop);
    for k in first_loud..=(loud_end - size) / hop {
        assert!((at(k) - want).abs() < FFT_TOL, "frame {k}: {}", at(k));
    }

    let step = Db::from_power(std::f64::consts::E).get()
        * plan(&cfg).geometry.frame_interval().get()
        / tau.get();
    let first_silent = loud_end.div_ceil(hop);
    let mut checked = 0;
    for k in first_silent..frames.len() - 1 {
        let drop = at(k) - at(k + 1);
        assert!((drop - step).abs() < 1e-3, "frame {k}: {drop} vs {step}");
        checked += 1;
    }
    assert!(checked >= 8);
}

/// A tone then silence: the peak holds its level for the hold rounded up
/// to whole frames, then falls by the decay per second, `decay * dt` a
/// frame.
#[test]
fn peaks_hold_then_decay_at_the_configured_rate() {
    let (size, hop) = (1536, 512);
    let (hold, decay) = (Seconds::new(0.25), Db::new(30.0));
    let cfg = config(size, hop)
        .peak_hold(SpectrumTime::of(hold))
        .peak_decay(PeakDecay::of(decay));
    let mut signal = tone(size * 3, 1000.0, 0.5);
    signal.resize(size * 3 + size * 40, 0.0);

    let mut state = meter(&cfg);
    let mut series = Vec::new();
    for block in signal.chunks(hop) {
        if push_frames(&mut state, block) {
            series.push((state.levels()[32].get(), state.peaks()[32].get()));
        }
    }
    // The last frame whose level is its own peak is where the hold starts.
    let anchor = series
        .iter()
        .rposition(|&(l, p)| l == p && l > -20.0)
        .unwrap();
    let dt = plan(&cfg).geometry.frame_interval().get();
    let held = plan(&cfg).peak_hold_frames().get();
    assert!(held as f64 * dt >= hold.get() && (held - 1) as f64 * dt < hold.get());
    assert_eq!(held, 24);

    let top = series[anchor].1;
    for j in 1..=held {
        assert_eq!(series[anchor + j].1, top, "held frame {j}");
    }
    for m in 1..30 {
        let want = top - decay.get() * dt * m as f64;
        let got = series[anchor + held + m].1;
        assert!(
            (got - want).abs() < 1e-3,
            "decay frame {m}: {got} vs {want}"
        );
    }
    // Never under the level.
    assert!(series.iter().all(|&(l, p)| p >= l));
}

/// The first `n` frames' columns are all there, and then the newest
/// `columns` in order: a ramp of amplitudes makes every frame differ.
#[test]
fn the_spectrogram_keeps_the_newest_columns_in_order() {
    let (size, hop, columns) = (1536, 512, 5);
    let cfg = config(size, hop).spectrogram(FrameCount(columns));
    let ramp: Vec<f32> = (0..size + hop * 11)
        .map(|i| {
            let a = 0.01 + 0.04 * (i / hop) as f64;
            (a * (TAU * 1000.0 * i as f64 / RATE).sin()) as f32
        })
        .collect();

    let mut state = meter(&cfg);
    assert_eq!(state.spectrogram().len(), 0);
    let mut seen: Vec<Vec<Db>> = Vec::new();
    for block in ramp.chunks(hop) {
        if push_frames(&mut state, block) {
            seen.push(state.levels().to_vec());
            let cols: Vec<Vec<Db>> = state.spectrogram().map(<[Db]>::to_vec).collect();
            let n = seen.len().min(columns);
            assert_eq!(cols, seen[seen.len() - n..], "after {} frames", seen.len());
        }
    }
    assert!(seen.len() > columns * 2);
    assert!(seen.windows(2).all(|w| w[0][32] != w[1][32]));
}

/// An impulse at the middle of a Hann frame has `|X_k| = 1` in every bin,
/// a flat spectrum, so a band reads `10 log10(n * 4 / (N/2)^2)` for the `n`
/// bins it holds (counted here from the edges): power sums over a band, it
/// is not averaged. All bands together hold every bin from `min` to the top
/// edge.
#[test]
fn a_band_sums_the_power_of_its_bins() {
    let size = 4096;
    let width = RATE / size as f64;
    let bandings = [
        Banding::Octave {
            per_octave: BandsPerOctave::of(3),
            band: band(100.0, 20_000.0),
        },
        Banding::Log {
            count: BandCount::of(24),
            band: band(50.0, 20_000.0),
        },
        Banding::Octave {
            per_octave: BandsPerOctave::of(1),
            band: band(100.0, 12_800.0),
        },
    ];
    for banding in bandings {
        let cfg = config(size, size).banding(banding);
        let mut signal = vec![0.0f32; size];
        signal[size / 2] = 1.0;
        let (state, frames) = run(&cfg, &signal);

        let (min, bands, edge): (f64, usize, Box<dyn Fn(usize) -> f64>) = match banding {
            Banding::Octave { per_octave, band } => {
                let (min, max) = (band.min(), band.max());
                let n = (per_octave.get() as f64 * (max.get() / min.get()).log2() + 1e-9).floor()
                    as usize;
                let lo = min.get();
                (
                    lo,
                    n,
                    Box::new(move |j| lo * 2f64.powf(j as f64 / per_octave.get() as f64)),
                )
            }
            Banding::Log { count, band } => {
                let (min, max) = (band.min(), band.max());
                let count = count.get();
                let (lo, hi) = (min.get(), max.get());
                (
                    lo,
                    count,
                    Box::new(move |j| lo * (hi / lo).powf(j as f64 / count as f64)),
                )
            }
            _ => unreachable!(),
        };
        assert_eq!(frames[0].len(), bands);
        assert_eq!(state.frequencies().len(), bands);
        let mut total = 0;
        for (j, (level, centre)) in frames[0].iter().zip(state.frequencies()).enumerate() {
            let n = (0..=size / 2)
                .filter(|&k| {
                    let f = k as f64 * width;
                    f >= edge(j) && f < edge(j + 1)
                })
                .count();
            total += n;
            assert!(n >= 1);
            let want = Db::from_power(n as f64 * 4.0 / (size as f64 / 2.0).powi(2)).get();
            assert!(
                (level.get() - want).abs() < FFT_TOL,
                "{banding:?} band {j} ({n} bins): {level:?} vs {want}"
            );
            let geometric_mean = (edge(j) * edge(j + 1)).sqrt();
            assert!((centre.get() / geometric_mean - 1.0).abs() < 1e-6);
        }
        let held = (0..=size / 2)
            .filter(|&k| (k as f64 * width) >= min && (k as f64 * width) < edge(bands))
            .count();
        assert_eq!(total, held);
    }
}

/// A band with no bin is refused with the band, its edges and a window that
/// holds every band. Third-octave bands from 20 Hz at 48 kHz: the second
/// (25.2 to 31.7 Hz) sits between the bins of a 4096-point window, 11.7 Hz
/// apart. The window named is for the narrowest band, the first, 5.198 Hz
/// wide: `rate / width` = 9234 rounded up to a power of two, 16384, where
/// the same banding is accepted.
#[test]
fn an_empty_band_names_itself_and_the_window_that_holds_it() {
    let banding = Banding::Octave {
        per_octave: BandsPerOctave::of(3),
        band: band(20.0, 20_000.0),
    };
    let Err(MeterError::EmptyBand {
        band,
        lower,
        upper,
        window,
    }) = Measure::<f32>::measure(&config(4096, 1024).banding(banding), SampleRate::of(RATE))
    else {
        panic!("a 4096-point window cannot hold the lowest third-octave band");
    };
    assert_eq!(band, 1);
    assert!((lower.get() - 20.0 * 2f64.powf(1.0 / 3.0)).abs() <= 20.0 * f64::EPSILON * 4.0);
    assert!((upper.get() - 20.0 * 2f64.powf(2.0 / 3.0)).abs() <= 40.0 * f64::EPSILON * 4.0);
    assert_eq!(window, Samples::new(16_384));
    assert!(
        Measure::<f32>::measure(&config(16_384, 4096).banding(banding), SampleRate::of(RATE))
            .is_ok()
    );
}

/// DC and an even window's Nyquist bin have no image, so a DC offset `A`
/// reads `20 log10(A)` (the Hann window sums to `N/2`, `|X_0| = A N / 2`)
/// and a Nyquist tone `A (-1)^n` does too.
#[test]
fn dc_and_nyquist_read_their_amplitude() {
    let size = 1024;
    let cfg = config(size, size);
    let want = Db::from_amplitude(Amplitude::new(0.25)).get();
    let (_, dc) = run(&cfg, &vec![0.25f32; size]);
    assert!((dc[0][0].get() - want).abs() < FFT_TOL, "{:?}", dc[0][0]);
    let nyquist: Vec<f32> = (0..size)
        .map(|i| if i % 2 == 0 { 0.25 } else { -0.25 })
        .collect();
    let (_, frames) = run(&cfg, &nyquist);
    assert!(
        (frames[0][size / 2].get() - want).abs() < FFT_TOL,
        "{:?}",
        frames[0][size / 2]
    );
}

/// A bin-centred sine inside one band reads its level plus `10 log10(1.5)`
/// = 1.76 dB: Hann's main bin and two half-amplitude neighbours sum to
/// `1 + 2 * (1/2)^2` = 1.5 of the sine's power.
#[test]
fn a_sine_inside_a_band_reads_the_noise_bandwidth_excess() {
    let size = 1536;
    let cfg = config(size, size).banding(Banding::Octave {
        per_octave: BandsPerOctave::of(1),
        band: band(700.0, 2800.0),
    });
    let (state, frames) = run(&cfg, &tone(size, 1000.0, 0.5));
    let want = Db::from_amplitude(Amplitude::new(0.5)).get() + Db::from_power(1.5).get();
    let band = state
        .frequencies()
        .iter()
        .position(|f| f.get() < 1000.0 && f.get() > 900.0);
    let level = frames[0][band.unwrap()].get();
    assert!((level - want).abs() < FFT_TOL, "{level} vs {want}");
}

/// The hold is a whole number of frames worked from the integer hop, so an
/// exact multiple is not over-held by the `f32` frame interval: 1.0 s at a
/// hop of 480 samples and 48 kHz is 100 frames, not 101, and 0.1 s (an `f32`
/// a hair over a tenth) is 10.
#[test]
fn the_hold_is_a_whole_number_of_frames_without_over_holding() {
    let cfg = config(1920, 480);
    let frames = |hold: f64| {
        plan(&cfg.peak_hold(SpectrumTime::new(Seconds::new(hold)).unwrap()))
            .peak_hold_frames()
            .get()
    };
    assert_eq!(frames(1.0), 100);
    assert_eq!(frames(0.1), 10);
    assert_eq!(frames(0.0), 0);
    assert_eq!(frames(0.25), 25);
    // A hair over a frame holds the next.
    assert_eq!(frames(0.0101), 2);
    assert!(matches!(
        Measure::<f32>::measure(
            &config(64, 1)
                .peak_hold(SpectrumTime::of(Seconds::new(3600.0)))
                .peak_decay(PeakDecay::of(Db::new(1.0))),
            SampleRate::of(10_000_000.0)
        )
        .map(|_: SpectrumMeter<f32>| ()),
        Err(MeterError::PeakHoldTooLong { .. })
    ));
}

/// The rules that span fields are checked, and sizes are capped before
/// anything is allocated.
#[test]
fn bad_settings_are_errors() {
    let new = |size, hop| Measure::<f32>::measure(&config(size, hop), SampleRate::of(RATE));
    for size in [63, 65_537] {
        assert!(matches!(
            new(size, size),
            Err(MeterError::Framing(FramingError::WindowOutOfRange { .. }))
        ));
    }
    assert!(matches!(
        new(256, 257),
        Err(MeterError::Framing(FramingError::HopWiderThanWindow { .. }))
    ));
    for (size, hop) in [(65_536, 1), (4096, 63)] {
        assert!(matches!(
            new(size, hop),
            Err(MeterError::Framing(FramingError::TooManyOverlaps { .. }))
        ));
    }
    assert!(new(64, 1).is_ok() && new(65_536, 65_536).is_ok() && new(4096, 64).is_ok());
    for rate in [0.5, 1e9] {
        assert!(matches!(
            Measure::<f32>::measure(&config(256, 64), SampleRate::of(rate)),
            Err(MeterError::SampleRateOutOfRange { .. })
        ));
    }

    let cfg = config(1024, 256);
    assert!(matches!(
        Measure::<f32>::measure(
            &cfg.spectrogram(FrameCount(usize::MAX)),
            SampleRate::of(RATE)
        )
        .map(|_: SpectrumMeter<f32>| ()),
        Err(MeterError::SpectrogramTooLarge { .. })
    ));
    assert_eq!(
        Measure::<f32>::measure(
            &cfg.spectrogram(FrameCount((1 << 22) / 513 + 1)),
            SampleRate::of(RATE)
        )
        .map(|_: SpectrumMeter<f32>| ()),
        Err(MeterError::SpectrogramTooLarge {
            cells: ((1 << 22) / 513 + 1) * 513,
            max: 1 << 22
        })
    );
    assert!(
        cfg.spectrogram(FrameCount((1 << 22) / 513))
            .measure(SampleRate::of(RATE))
            .map(|_: SpectrumMeter<f32>| ())
            .is_ok()
    );
    let banded = cfg
        .banding(Banding::Log {
            count: BandCount::of(4000),
            band: band(100.0, 20_000.0),
        })
        .measure(SampleRate::of(RATE))
        .map(|_: SpectrumMeter<f32>| ());
    assert!(
        matches!(banded, Err(MeterError::EmptyBand { .. })),
        "4000 bands of 513 bins cannot all hold a bin"
    );

    let oct = |per_octave: usize, min: f64, max: f64| {
        cfg.banding(Banding::Octave {
            per_octave: BandsPerOctave::new(per_octave).unwrap(),
            band: band(min, max),
        })
    };
    assert!(matches!(
        oct(48, 1e-30, 20_000.0)
            .measure(SampleRate::of(RATE))
            .map(|_: SpectrumMeter<f32>| ()),
        Err(MeterError::TooManyBands { max: 4096, .. })
    ));
    assert!(
        matches!(
            oct(48, 100.0, 20_000.0)
                .measure(SampleRate::of(RATE))
                .map(|_: SpectrumMeter<f32>| ()),
            Err(MeterError::EmptyBand { .. })
        ),
        "1/48 octave is finer than the bins"
    );
    for (min, max) in [
        (f64::NAN, 1000.0),
        (100.0, f64::INFINITY),
        (0.0, 1000.0),
        (1000.0, 100.0),
    ] {
        assert!(
            matches!(
                FrequencyBand::try_from(Hz::new(min)..=Hz::new(max)),
                Err(EmptyFrequencyRange { .. })
            ),
            "{min} {max}"
        );
    }
    assert!(matches!(
        oct(1, 100.0, 30_000.0)
            .measure(SampleRate::of(RATE))
            .map(|_: SpectrumMeter<f32>| ()),
        Err(MeterError::Nyquist(AboveNyquist { .. }))
    ));
    assert!(matches!(
        oct(1, 100.0, 150.0)
            .measure(SampleRate::of(RATE))
            .map(|_: SpectrumMeter<f32>| ()),
        Err(MeterError::EmptyFrequencyRange(EmptyFrequencyRange { .. }))
    ));
    // Returning to per-bin outputs multiplies the cells per column: a
    // spectrogram sized for 17 bands is not one for 513 bins.
    let banded = oct(3, 400.0, 20_000.0).spectrogram(FrameCount(100_000));
    assert!(
        banded
            .measure(SampleRate::of(RATE))
            .map(|_: SpectrumMeter<f32>| ())
            .is_ok()
    );
    assert!(matches!(
        banded
            .banding(Banding::Bins)
            .measure(SampleRate::of(RATE))
            .map(|_: SpectrumMeter<f32>| ()),
        Err(MeterError::SpectrogramTooLarge { .. })
    ));
}

/// The floor is where silence reads, and no level reads under it: a tone
/// far below a -60 dB floor reads the floor in every bin.
#[test]
fn no_level_reads_under_the_configured_floor() {
    let cfg = config(256, 64).floor(SpectrumFloor::of(Db::new(-60.0)));
    let mut state = meter(&cfg);
    push_frames(&mut state, &vec![0.0f32; 1000]);
    assert!(state.levels().iter().all(|&l| l == Db::new(-60.0)));
    push_frames(&mut state, &tone(1000, 1000.0, 1e-6));
    assert!(state.levels().iter().all(|&l| l == Db::new(-60.0)));
}

/// A refused block leaves the state as it was; silence reads the floor;
/// a huge finite block stays finite; `reset` returns to the first state.
#[test]
fn input_is_checked_and_state_stays_finite() {
    let cfg = config(256, 64).spectrogram(FrameCount(4));
    let mut state = meter(&cfg);
    assert!(state.levels().iter().all(|&l| l == plan(&cfg).floor.get()));

    push_frames(&mut state, &vec![0.0f32; 1000]);
    assert!(
        state.levels().iter().all(|&l| l == plan(&cfg).floor.get()),
        "silence"
    );
    assert!(state.peaks().iter().all(|&l| l == plan(&cfg).floor.get()));

    let before = state.levels().to_vec();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut block = vec![0.1f32; 500];
        block[499] = bad;
        assert_eq!(
            state.push(Mono::new(&block), &mut Vec::new()),
            Err(MeterError::NonFiniteInput)
        );
        assert_eq!(state.levels().to_vec(), before);
    }

    push_frames(&mut state, &vec![3.0e38f32; 600]);
    assert!(
        state
            .levels()
            .iter()
            .chain(state.peaks())
            .all(|l| l.get().is_finite())
    );
    // The power is clamped to `MAX_POWER`, so no level passes its decibels.
    let ceiling = Db::from_power(MAX_POWER);
    assert!(state.levels().iter().all(|&l| l <= ceiling));
    push_frames(&mut state, &vec![0.0f32; 600]);
    assert!(state.levels().iter().all(|l| l.get().is_finite()));

    state.reset();
    assert!(
        state
            .levels()
            .iter()
            .chain(state.peaks())
            .all(|&l| l == plan(&cfg).floor.get())
    );
    assert_eq!(state.spectrogram().len(), 0);
}

/// The trait fold over uneven chunks (1, 7, 64 samples and the rest) yields
/// one item per frame the stream completes, `1 + (n - size) / hop`, and ends
/// on the levels of one push of the whole stream.
#[test]
fn the_trait_fold_over_uneven_chunks_yields_every_frame_and_the_whole_stream_levels() {
    let cfg = config(256, 64);
    let x = tone(1000, 1500.0, 0.5);
    let (items, levels) = fold_uneven(&cfg, SampleRate::of(RATE), x.len(), |meter, at, out| {
        meter.push(Mono::new(&x[at]), out)
    })
    .unwrap();
    assert_eq!(items.len(), 1 + (x.len() - 256) / 64);
    let mut whole = meter(&cfg);
    assert!(push_frames(&mut whole, &x));
    assert_eq!(levels, whole.levels());
    let floor = plan(&cfg).floor.get();
    assert!(
        levels.iter().any(|&l| l > floor),
        "the tone reads above the floor"
    );
}
