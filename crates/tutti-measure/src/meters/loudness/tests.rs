use super::*;
use crate::fold_uneven;
use std::f32::consts::TAU;
use tutti_buffers::{AmbisonicFormat, AmbisonicOrder, Bed, Speaker};
use tutti_units::Amplitude;

/// Pushes one chunk to a windowed meter and returns the reading it yields.
fn read(
    state: &mut WindowedMeter<f32>,
    chunk: Interleaved<'_>,
) -> Result<WindowedReading, MeterError> {
    let mut readings = Vec::new();
    state.push(chunk, &mut readings)?;
    assert_eq!(readings.len(), 1, "one reading per accepted push");
    Ok(readings[0])
}

/// A config with the rate its tests measure at.
struct Rated<C>(C, SampleRate);

/// `measure_loudness` at the rate the config was made for.
fn measure_loudness<S: Sample>(
    c: &Rated<LoudnessConfig>,
    buffer: Interleaved<'_, S>,
) -> Result<Loudness, MeterError> {
    super::measure_loudness(&c.0, buffer, c.1)
}

/// The true-peak floor lives at this boundary, not in `Db::from_amplitude`.
#[test]
fn true_peak_pins_silence_and_stays_wide() {
    // The shared conversion does not floor; this boundary does.
    assert!(Db::from_amplitude(Amplitude::SILENT).get().is_infinite());
    assert_eq!(true_peak_db(0.0), Db::FLOOR);
    assert!(true_peak_db(0.0).get().is_finite());

    // Agrees with the shared conversion everywhere above silence.
    let want = Db::from_amplitude(Amplitude::new(0.5));
    assert!((true_peak_db(0.5).get() - want.get()).abs() < 1e-5);
    assert!((true_peak_db(1.0).get() - 0.0).abs() < 1e-6);

    // Computed at f64 width: narrowing to `Amplitude` (f32) before the
    // log10 is the one thing this path exists to avoid.
    let tiny = 1.0e-30_f64;
    assert!(
        (true_peak_db(tiny).get() - -600.0).abs() < 1.0,
        "f64 input must survive the log10, got {:?}",
        true_peak_db(tiny)
    );
}

/// An infinite true peak poisons every gain derived from it — the same
/// reason `lufs` is clamped to the `-70` gate.
#[test]
fn silent_render_yields_a_finite_normalization_gain() {
    let m = Loudness {
        lufs: Db::new(-70.0),
        true_peak: true_peak_db(0.0),
        range: Db::new(0.0),
    };
    assert!(m.gain_to(Db::new(-14.0), Db::new(-1.0)).get().is_finite());
}

/// Interleaved stereo sine at `amp`, `secs` long.
fn sine(rate: f64, secs: f64, freq: f64, amp: f64) -> Vec<f32> {
    let n = (rate * secs) as usize;
    tutti_fixtures::signal::sine(
        tutti_units::Hz::new(freq),
        tutti_units::Amplitude::new(amp),
        SampleRate::of(rate),
        tutti_units::Samples::new(n),
    )
    .into_iter()
    .flat_map(|s| [s, s])
    .collect()
}

fn cfg(rate: f64) -> Rated<LoudnessConfig> {
    Rated(
        LoudnessConfig::new(Bed::Stereo.topology()),
        SampleRate::of(rate),
    )
}

/// The one-shot form must fold the streaming form exactly — the property
/// `peaks.rs` holds, and the reason a batch/stream drift cannot appear.
#[test]
fn streaming_matches_one_shot() {
    let c = cfg(48_000.0);
    let buf = sine(48_000.0, 1.0, 1_000.0, 0.5);

    let one_shot =
        measure_loudness(&c, Interleaved::new(&buf, ChannelLayout::STEREO).unwrap()).unwrap();

    // 777 frames: not a multiple of any block size the meter uses.
    for frames in [1, 777, 4800, 48_000] {
        let mut meter = c.0.measure(c.1).unwrap();
        for chunk in buf.chunks(frames * 2) {
            meter
                .push(
                    Interleaved::new(chunk, ChannelLayout::STEREO).unwrap(),
                    &mut (),
                )
                .unwrap();
        }
        assert_eq!(meter.finish(), one_shot, "chunks of {frames} frames");
    }
}

/// The rate is honoured, not assumed. A meter built for the wrong rate
/// reads a different loudness for the same musical signal.
#[test]
fn the_configured_rate_is_used() {
    // Same *sample* data interpreted at two rates is a different signal
    // (different frequency, different duration), so the readings differ.
    let buf = sine(44_100.0, 2.0, 1_000.0, 0.5);
    let stereo = Interleaved::new(&buf, ChannelLayout::STEREO).unwrap();
    let at_44 = measure_loudness(&cfg(44_100.0), stereo).unwrap();
    let at_48 = measure_loudness(&cfg(48_000.0), stereo).unwrap();
    assert!(
        (at_44.lufs.get() - at_48.lufs.get()).abs() > 1e-4,
        "a meter that ignored its rate would report the same LUFS twice: \
         {at_44:?} vs {at_48:?}"
    );
}

/// A −6 dBFS 1 kHz sine reads about −6.7 LUFS. Derived from BS.1770's
/// definition, not from this implementation:
///
/// ```text
/// LUFS = -0.691 + 10*log10( Σ_ch G_ch * mean_square_ch )
/// mean square of a sine of amplitude a = a²/2 = 0.125
/// stereo, both G = 1.0                  → Σ = 0.25
/// -0.691 + 10*log10(0.25)               = -6.71 LUFS
/// ```
///
/// Note the stereo **sum**: two identical channels are ~3 dB louder than
/// one, which is why this is not the −9.7 a single channel would read.
/// The tolerance covers K-weighting's small lift at 1 kHz (the shelf is
/// flat near 0 dB there, but not exactly).
#[test]
fn silence_floors_rather_than_erroring() {
    let silence = vec![0.0; 4800];
    let m = measure_loudness(
        &cfg(48_000.0),
        Interleaved::new(&silence, ChannelLayout::STEREO).unwrap(),
    )
    .unwrap();
    assert_eq!(m.true_peak, Db::FLOOR);
    assert!(m.lufs.get() <= -70.0, "gated silence, got {:?}", m.lufs);
}

/// Every reading must be finite, even when nothing passes R128's gate.
///
/// `ebur128` returns `Ok(-inf)` rather than an error for an input shorter
/// than the 400 ms gating block — which a short render legitimately is. An
/// infinite LUFS silently poisons `gain_to` into `NaN`, and a `NaN` gain
/// multiplies a whole render into `NaN`. Caught by an export test composing
/// normalization over a 0.2 s buffer.
#[test]
fn a_reading_is_always_finite_even_below_the_gate() {
    // 100 ms — a quarter of the gating block.
    let short = sine(48_000.0, 0.1, 1_000.0, 0.5);
    let m = measure_loudness(
        &cfg(48_000.0),
        Interleaved::new(&short, ChannelLayout::STEREO).unwrap(),
    )
    .unwrap();
    assert!(
        m.lufs.get().is_finite(),
        "LUFS must be finite, got {:?}",
        m.lufs
    );
    assert!(m.range.get().is_finite());
    assert!(
        m.gain_to(Db::new(-14.0), Db::new(-1.0)).get().is_finite(),
        "a gain derived from a sub-gate reading must not be NaN"
    );
}

/// A chunk whose own width disagrees with the meter's is refused, not fed.
///
/// The buffer carries its width, so the disagreement is detectable. Fed
/// anyway, a quad chunk handed to a stereo meter would be split into twice
/// as many frames of the wrong signal and silently folded into the
/// integrated reading.
#[test]
fn a_chunk_of_the_wrong_width_is_refused() {
    let c = cfg(48_000.0);
    let buf = sine(48_000.0, 1.0, 1_000.0, 0.5);

    let mut meter = c.0.measure(c.1).unwrap();
    assert_eq!(
        meter.push(
            Interleaved::new(&buf, ChannelLayout::QUAD).unwrap(),
            &mut ()
        ),
        Err(MeterError::ChunkLayoutMismatch {
            chunk: ChannelLayout::QUAD,
            config: ChannelLayout::STEREO,
        })
    );
    let m = meter.finish();

    assert_eq!(
        m.true_peak,
        Db::FLOOR,
        "a mismatched chunk must not reach the meter, got {:?}",
        m.true_peak
    );
}

/// The gain is the plain difference when the ceiling is not in play.
#[test]
fn gain_to_targets_the_requested_loudness() {
    let m = Loudness {
        lufs: Db::new(-20.0),
        true_peak: Db::new(-12.0),
        range: Db::new(0.0),
    };
    // −20 → −14 is +6 dB; the peak lands at −6, under a −1 ceiling.
    assert!((m.gain_to(Db::new(-14.0), Db::new(-1.0)).get() - 6.0).abs() < 1e-5);
}

/// …and is pulled back by exactly the overshoot when it is.
#[test]
fn gain_to_respects_the_true_peak_ceiling() {
    let m = Loudness {
        lufs: Db::new(-20.0),
        true_peak: Db::new(-2.0),
        range: Db::new(0.0),
    };
    // +6 dB would put the peak at +4, which is 5 dB over a −1 ceiling.
    assert!((m.gain_to(Db::new(-14.0), Db::new(-1.0)).get() - 1.0).abs() < 1e-5);
}

/// A signal already under the ceiling must not be pushed *up* to meet it.
#[test]
fn gain_to_never_amplifies_to_reach_the_ceiling() {
    let m = Loudness {
        lufs: Db::new(-20.0),
        true_peak: Db::new(-40.0),
        range: Db::new(0.0),
    };
    assert!((m.gain_to(Db::new(-20.0), Db::new(-1.0)).get()).abs() < 1e-5);
}

/// Read-out tolerance for a steady tone, in LU: EBU Tech 3341's `±0.1 LU`
/// for a conforming meter. The truncation of a 997 Hz tone to a window that
/// is not a whole number of cycles adds at most
/// `10 log10(1 + 1/(4 pi f T))` = 0.001 dB for the 400 ms window, so the
/// published figure is the whole budget.
const WINDOW_TOL: f64 = 0.1;

/// Stereo (identical channels) 997 Hz sine, amplitude `amp(t)` at `t`
/// seconds, with a continuous phase.
fn windowed_sine(rate: f64, secs: f64, amp: impl Fn(f64) -> f64) -> Vec<f32> {
    let n = (rate * secs).round() as usize;
    (0..n)
        .flat_map(|i| {
            let t = i as f64 / rate;
            let s = (amp(t) * (std::f64::consts::TAU * 997.0 * t).sin()) as f32;
            [s, s]
        })
        .collect()
}

/// The reading for identical channels of amplitude `a`: BS.1770 gives
/// `-0.691 + 10 log10(sum_ch G * a^2 / 2)`, and the K-weighting's gain at
/// 997 Hz is the `+0.691` dB that offset cancels, so two channels read
/// `10 log10(a^2) = 20 log10(a)`.
fn stereo_lufs(a: f64) -> f64 {
    Db::from_amplitude(Amplitude::new(a)).get()
}

fn windowed_cfg(rate: f64) -> Rated<WindowedLoudnessConfig> {
    Rated(
        WindowedLoudnessConfig::new(Bed::Stereo.topology()),
        SampleRate::of(rate),
    )
}

/// Feeds `buf` in `frames`-frame chunks, returning the reading after each.
fn windowed_run(
    cfg: &Rated<WindowedLoudnessConfig>,
    buf: &[f32],
    frames: usize,
) -> Vec<WindowedReading> {
    let mut state = cfg.0.measure(cfg.1).unwrap();
    buf.chunks(frames * 2)
        .map(|chunk| {
            read(
                &mut state,
                Interleaved::new(chunk, ChannelLayout::STEREO).unwrap(),
            )
            .unwrap()
        })
        .collect()
}

/// A -20 dBFS stereo 997 Hz sine reads -20.0 LUFS in both windows, and
/// each reads the floor until exactly its own window of frames is in.
#[test]
fn a_sine_reads_its_loudness_once_each_window_fills() {
    let want = stereo_lufs(0.1);
    assert!((want - -20.0).abs() < 1e-5);
    for rate in [44_100.0, 48_000.0] {
        let cfg = windowed_cfg(rate);
        let block = (rate / 100.0) as usize; // 10 ms
        let tone = windowed_sine(rate, 3.5, |_| 0.1);
        let r = windowed_run(&cfg, &tone, block);
        let at = |ms: usize| r[ms / 10 - 1];

        assert_eq!(at(390).momentary, WindowedReading::LOUDNESS_FLOOR, "{rate}");
        assert!(
            (at(400).momentary.get() - want).abs() < WINDOW_TOL,
            "{rate}: momentary {:?}",
            at(400).momentary
        );
        assert_eq!(
            at(2990).short_term,
            WindowedReading::LOUDNESS_FLOOR,
            "{rate}"
        );
        assert!(
            (at(3000).short_term.get() - want).abs() < WINDOW_TOL,
            "{rate}: short-term {:?}",
            at(3000).short_term
        );
        // The peak of an amplitude-0.1 tone is 20 log10(0.1) dBTP, a figure of
        // its own; the 4x oversampled meter adds under 0.3 dB, as in
        // `true_peak_is_the_signals_amplitude`.
        let peak_want = Db::from_amplitude(Amplitude::new(0.1)).get();
        assert!((at(3500).true_peak.get() - peak_want).abs() < 0.3);
    }
}

/// A 10 dB step (10x the power) moves momentary loudness along
/// `10 log10(1 + 9 min(t/0.4, 1))` and short-term along the same with
/// `t/3`: the windows are linear in power, so the share of the window past
/// the step is `t / window`. Each is settled at its window and not before.
#[test]
fn a_ten_db_step_settles_after_each_window() {
    let rate = 48_000.0;
    let cfg = windowed_cfg(rate);
    let (before, step_at) = (0.05, 4.0);
    let after = before * 10f64.sqrt();
    let signal = windowed_sine(rate, step_at + 4.0, |t| {
        if t < step_at { before } else { after }
    });
    let r = windowed_run(&cfg, &signal, 480);
    let at = |t: f64| r[((step_at + t) * 100.0).round() as usize - 1];

    let (low, high) = (stereo_lufs(before), stereo_lufs(after));
    assert!((high - low - 10.0).abs() < 1e-4);
    assert!((at(0.0).momentary.get() - low).abs() < WINDOW_TOL);
    assert!((at(0.0).short_term.get() - low).abs() < WINDOW_TOL);

    let ramp = |t: f64, window: f64| low + 10.0 * (1.0 + 9.0 * (t / window).min(1.0)).log10();
    for t in [0.1, 0.2, 0.3, 0.4, 0.5, 1.0] {
        let got = at(t).momentary.get();
        assert!(
            (got - ramp(t, 0.4)).abs() < WINDOW_TOL,
            "momentary +{t}s: {got}"
        );
    }
    for t in [0.5, 1.0, 2.0, 3.0, 3.5, 4.0] {
        let got = at(t).short_term.get();
        assert!(
            (got - ramp(t, 3.0)).abs() < WINDOW_TOL,
            "short-term +{t}s: {got}"
        );
    }
    // Settled at the window, and visibly not before it.
    assert!((at(0.4).momentary.get() - high).abs() < WINDOW_TOL);
    assert!(at(0.3).momentary.get() < high - 1.0);
    assert!((at(3.0).short_term.get() - high).abs() < WINDOW_TOL);
    assert!(at(2.0).short_term.get() < high - 1.0);
}

/// Silence reads the floor in both windows and a floor peak; so does a
/// window that signal has left, once the K-weighting's ring-out is under
/// the floor.
#[test]
fn silence_reads_the_floor() {
    let cfg = windowed_cfg(48_000.0);
    let silence = vec![0.0f32; 48_000 * 2 * 4];
    let last = *windowed_run(&cfg, &silence, 4800).last().unwrap();
    assert_eq!(last.momentary, WindowedReading::LOUDNESS_FLOOR);
    assert_eq!(last.short_term, WindowedReading::LOUDNESS_FLOOR);
    assert_eq!(last.true_peak, Db::FLOOR);

    let mut left_behind = windowed_sine(48_000.0, 4.0, |_| 0.1);
    left_behind.extend(vec![0.0f32; 48_000 * 2]);
    let r = windowed_run(&cfg, &left_behind, 480);
    let last = r.last().unwrap();
    assert_eq!(last.momentary, WindowedReading::LOUDNESS_FLOOR);
    assert!(last.short_term.get() > -30.0, "{:?}", last.short_term);
    assert!(last.true_peak.get() > -21.0, "the peak is held");
}

/// The readings do not depend on how the stream is cut: 64-frame,
/// 4096-frame and 777-frame chunks agree bit for bit wherever they have
/// fed the same frames.
#[test]
fn readings_do_not_depend_on_the_chunking() {
    let cfg = windowed_cfg(48_000.0);
    // A level change and a quiet stretch, so the windows are not trivial.
    let signal = windowed_sine(48_000.0, 5.0, |t| {
        if t < 2.0 {
            0.2
        } else if t < 3.0 {
            0.01
        } else {
            0.4
        }
    });
    let fine = windowed_run(&cfg, &signal, 64);
    let coarse = windowed_run(&cfg, &signal, 4096);
    let total = signal.len() / 2;
    for (i, c) in coarse.iter().enumerate() {
        let frames = ((i + 1) * 4096).min(total);
        if frames % 64 == 0 {
            assert_eq!(*c, fine[frames / 64 - 1], "after {frames} frames");
        }
    }
    assert_eq!(coarse.last(), fine.last());

    let mut state = cfg.0.measure(cfg.1).unwrap();
    let mut odd = None;
    for chunk in signal.chunks(777 * 2) {
        odd = Some(
            read(
                &mut state,
                Interleaved::new(chunk, ChannelLayout::STEREO).unwrap(),
            )
            .unwrap(),
        );
    }
    assert_eq!(odd.as_ref(), fine.last());
}

/// `reset` returns the meter to its first state.
#[test]
fn reset_reads_like_a_fresh_meter() {
    let cfg = windowed_cfg(48_000.0);
    let tone = windowed_sine(48_000.0, 4.0, |_| 0.1);
    let mut state = cfg.0.measure(cfg.1).unwrap();
    read(
        &mut state,
        Interleaved::new(&tone, ChannelLayout::STEREO).unwrap(),
    )
    .unwrap();
    assert!(state.frames_seen() > 0);
    state.reset();
    assert_eq!(state.frames_seen(), 0);
    let quiet = vec![0.0f32; 48_000 * 2];
    let r = read(
        &mut state,
        Interleaved::new(&quiet, ChannelLayout::STEREO).unwrap(),
    )
    .unwrap();
    assert_eq!(r.momentary, WindowedReading::LOUDNESS_FLOOR);
    assert_eq!(r.short_term, WindowedReading::LOUDNESS_FLOOR);
    assert_eq!(r.true_peak, Db::FLOOR);
}

/// The true peak is the loudest channel's, wherever it is: a tone of
/// amplitude 0.5 on the right alone peaks at `20 log10(0.5)` dBTP (4x
/// oversampling adds under 0.3 dB), and on the left alone just the same.
#[test]
fn the_true_peak_is_the_loudest_channels() {
    let cfg = windowed_cfg(48_000.0);
    let want = Db::from_amplitude(Amplitude::new(0.5)).get();
    for (left, right) in [(0.0f64, 0.5f64), (0.5, 0.0), (0.1, 0.5)] {
        let signal: Vec<f32> = (0..24_000)
            .flat_map(|i| {
                let s = (TAU as f64 * 997.0 * i as f64 / 48_000.0).sin();
                [(left * s) as f32, (right * s) as f32]
            })
            .collect();
        let mut state = cfg.0.measure(cfg.1).unwrap();
        let r = read(
            &mut state,
            Interleaved::new(&signal, ChannelLayout::STEREO).unwrap(),
        )
        .unwrap();
        assert!(
            (r.true_peak.get() - want).abs() < 0.3,
            "L {left} R {right}: {:?}",
            r.true_peak
        );
    }
}

/// A bad chunk is refused without touching the meter.
#[test]
fn bad_settings_and_input_are_errors() {
    let cfg = windowed_cfg(48_000.0);
    let mut state = cfg.0.measure(cfg.1).unwrap();
    let tone = windowed_sine(48_000.0, 0.5, |_| 0.1);
    let fed = |state: &mut WindowedMeter<f32>, buf: &[f32]| {
        read(state, Interleaved::new(buf, ChannelLayout::STEREO).unwrap())
    };
    fed(&mut state, &tone).unwrap();
    let frames = state.frames_seen();

    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut chunk = tone.clone();
        chunk[7] = bad;
        assert_eq!(fed(&mut state, &chunk), Err(MeterError::NonFiniteInput));
        assert_eq!(state.frames_seen(), frames, "a refused chunk feeds nothing");
    }
    assert!(matches!(
        read(
            &mut state,
            Interleaved::new(&tone, ChannelLayout::QUAD).unwrap()
        ),
        Err(MeterError::ChunkLayoutMismatch { .. })
    ));
    assert_eq!(state.frames_seen(), frames);

    // A rate outside what a windowed meter is built for is refused where
    // the rate binds.
    for rate in [7_999.0, 192_001.0] {
        assert!(matches!(
            Measure::<f32>::measure(&cfg.0, SampleRate::of(rate)),
            Err(MeterError::SampleRateOutOfRange { .. })
        ));
    }
}

// The oracle is BS.1770-5 Table 5: the weight of each speaker of each BS.2051
// system, and Annex 1's loudness `-0.691 + 10 log10(sum_i G_i z_i)` over the
// mean squares `z_i` of the K-weighted channels. A sine of amplitude `a` has
// `z = a^2 / 2 * k`, `k` the K-filter's power gain at its frequency, which
// cancels against a reference channel of weight 1.0 at the same tone.

const TONE_HZ: f64 = 1_000.0;
const TONE_AMP: f64 = 0.1;

/// Integrated loudness of `bed` with a tone of `TONE_AMP` on the `index`th
/// channel alone, two seconds at 48 kHz.
fn tone_on(bed: Bed, index: usize) -> Loudness {
    let width = usize::from(bed.width().count());
    let rate = SampleRate::SR_48K;
    let tone = tutti_fixtures::signal::sine(
        tutti_units::Hz::new(TONE_HZ),
        Amplitude::new(TONE_AMP),
        rate,
        tutti_units::Samples::new(2 * 48_000),
    );
    let mut buf = vec![0.0f32; tone.len() * width];
    for (frame, v) in tone.iter().enumerate() {
        buf[frame * width + index] = *v;
    }
    let c = Rated(LoudnessConfig::new(bed.topology()), rate);
    measure_loudness(&c, Interleaved::new(&buf, bed.width()).unwrap()).unwrap()
}

/// Table 5, per system: the weight of each speaker (1.00 or 1.41) in the
/// bed's channel order, `None` for the LFE.
fn table_5(bed: Bed) -> Vec<Option<f64>> {
    const W: Option<f64> = Some(1.41);
    const U: Option<f64> = Some(1.0);
    match bed {
        // System A, 0+2+0: M+030 M-030.
        Bed::Stereo => vec![U, U],
        // System B, 0+5+0: M+030 M-030 M+000 LFE M+110 M-110.
        Bed::Surround51 => vec![U, U, U, None, W, W],
        // System I, 0+7+0: M+030 M-030 M+000 LFE M+090 M-090 M+135 M-135.
        Bed::Surround71 => vec![U, U, U, None, W, W, U, U],
        // System J, 4+7+0: system I then U+045 U-045 U+135 U-135.
        Bed::Surround714 => vec![U, U, U, None, W, W, U, U, U, U, U, U],
        // System C, 2+5+0: system B then U+030 U-030.
        Bed::Surround512 => vec![U, U, U, None, W, W, U, U],
        // System D, 4+5+0: system B then U+030 U-030 U+110 U-110.
        Bed::Surround514 => vec![U, U, U, None, W, W, U, U, U, U],
        other => panic!("{other:?} is not pinned"),
    }
}

/// Each speaker of each BS.2051 bed the crate pins weighs what Table 5
/// says: a tone on it reads `-0.691 + 10 log10(G z)`, so against the same
/// tone on the front left (weight 1.0) it is `10 log10(G)` louder; and the
/// LFE adds no loudness (its peak still counts).
#[test]
fn a_speaker_weighs_what_table_5_lists_for_its_system() {
    for bed in [
        Bed::Stereo,
        Bed::Surround51,
        Bed::Surround71,
        Bed::Surround512,
        Bed::Surround514,
        Bed::Surround714,
    ] {
        let reference = tone_on(bed, 0);
        for (index, weight) in table_5(bed).into_iter().enumerate() {
            let read = tone_on(bed, index);
            match weight {
                Some(g) => {
                    let want = reference.lufs.get() + Db::from_power(g).get();
                    assert!(
                        (read.lufs.get() - want).abs() < 0.01,
                        "{bed:?} channel {index}: {:?} vs {want}",
                        read.lufs
                    );
                }
                None => {
                    assert_eq!(
                        read.lufs,
                        Db::new(-70.0),
                        "{bed:?} LFE counted: {:?}",
                        read.lufs
                    );
                    // The LFE is unweighted, not unmetered: the file's peak
                    // is still the tone's amplitude, 20 log10 0.1 = -20 dBTP.
                    assert!(
                        (read.true_peak.get() + 20.0).abs() < 0.1,
                        "{bed:?} LFE peak {:?}",
                        read.true_peak
                    );
                }
            }
        }
    }
}

/// The absolute figure, from Annex 1: a 1 kHz sine of amplitude 0.1 on one
/// 1.0 speaker has mean square `a^2 / 2 = 0.005`, and K-weighting's gain at
/// 1 kHz is 0.691 dB (the constant of the formula, cancelling at 997 Hz), so
/// it reads `10 log10(0.005) = -23.01` LKFS; on a 1.41 speaker it reads
/// 1.49 dB (`10 log10 1.41`) more.
#[test]
fn one_tone_reads_the_annex_1_figure_at_weight_one_and_one_point_four_one() {
    let unit = tone_on(Bed::Surround51, 0).lufs.get();
    let side = tone_on(Bed::Surround51, 4).lufs.get();
    let analytic = Db::from_power(TONE_AMP.powi(2) / 2.0).get();
    assert!((unit - analytic).abs() < 0.05, "{unit} vs {analytic}");
    assert!((side - (analytic + 1.4922)).abs() < 0.05, "{side}");
}

/// The 7.1's rears (M+-135) are 1.0 although the 5.1's M+-110 is 1.41, and
/// the 7.1.4's upper speakers are 1.0: a tone on each reads as the front
/// left's.
#[test]
fn rears_and_upper_speakers_weigh_one() {
    let front = tone_on(Bed::Surround714, 0).lufs.get();
    for index in [6, 7, 8, 9, 10, 11] {
        let read = tone_on(Bed::Surround714, index).lufs.get();
        assert!(
            (read - front).abs() < 0.01,
            "channel {index}: {read} vs {front}"
        );
    }
}

/// A topology in another order is weighed by speaker, not by index: the
/// WAVE 5.1 (`FL FR FC LFE BL BR`) puts the 1.41 speakers on the BL and BR
/// channels, which are the 5.1's M+-110.
#[test]
fn the_weight_follows_the_speaker_in_a_reordered_topology() {
    let reordered = ChannelTopology::new([
        Speaker::SideLeft,
        Speaker::FrontLeft,
        Speaker::LowFrequency,
        Speaker::FrontRight,
        Speaker::FrontCenter,
        Speaker::SideRight,
    ])
    .unwrap();
    let c = Rated(LoudnessConfig::new(reordered), SampleRate::SR_48K);
    let tone = tutti_fixtures::signal::sine(
        tutti_units::Hz::new(TONE_HZ),
        Amplitude::new(TONE_AMP),
        SampleRate::SR_48K,
        tutti_units::Samples::new(2 * 48_000),
    );
    let read = |channel: usize| {
        let mut buf = vec![0.0f32; tone.len() * 6];
        for (frame, v) in tone.iter().enumerate() {
            buf[frame * 6 + channel] = *v;
        }
        measure_loudness(
            &c,
            Interleaved::new(&buf, ChannelLayout::from(6u16)).unwrap(),
        )
        .unwrap()
        .lufs
        .get()
    };
    let (side, front, lfe) = (read(0), read(1), read(2));
    assert!((side - front - Db::from_power(1.41).get()).abs() < 0.01);
    assert_eq!(lfe, -70.0);
}

/// Ambisonic input and speakers that are no known bed are refused with
/// their own errors, by both configs, where the rate binds.
#[test]
fn a_topology_without_angles_is_refused_by_name() {
    let rate = SampleRate::SR_48K;
    let ambisonic: ChannelTopology = AmbisonicFormat::ambix(AmbisonicOrder::of(1)).into();
    let unplaced = ChannelTopology::unassigned(ChannelLayout::from(3u16)).unwrap();
    assert_eq!(
        Measure::<f32>::measure(&LoudnessConfig::new(ambisonic), rate).unwrap_err(),
        MeterError::AmbisonicLoudness
    );
    assert_eq!(
        Measure::<f32>::measure(&LoudnessConfig::new(unplaced), rate).unwrap_err(),
        MeterError::UnplacedLoudnessChannels {
            width: ChannelLayout::from(3u16)
        }
    );
    assert_eq!(
        Measure::<f32>::measure(&WindowedLoudnessConfig::new(ambisonic), rate).unwrap_err(),
        MeterError::AmbisonicLoudness
    );
    assert!(matches!(
        Measure::<f32>::measure(&WindowedLoudnessConfig::new(unplaced), rate),
        Err(MeterError::UnplacedLoudnessChannels { .. })
    ));
}

/// The windowed meter weighs by placement as the integrated one does: the
/// momentary loudness of a tone on the 5.1's side speaker is `10 log10 1.41`
/// above the same tone on its front left.
#[test]
fn the_windowed_meter_weighs_by_placement() {
    let cfg = Rated(
        WindowedLoudnessConfig::new(Bed::Surround51.topology()),
        SampleRate::SR_48K,
    );
    let tone = tutti_fixtures::signal::sine(
        tutti_units::Hz::new(TONE_HZ),
        Amplitude::new(TONE_AMP),
        SampleRate::SR_48K,
        tutti_units::Samples::new(48_000),
    );
    let momentary = |channel: usize| {
        let mut buf = vec![0.0f32; tone.len() * 6];
        for (frame, v) in tone.iter().enumerate() {
            buf[frame * 6 + channel] = *v;
        }
        let mut state = cfg.0.measure(cfg.1).unwrap();
        read(
            &mut state,
            Interleaved::new(&buf, Bed::Surround51.width()).unwrap(),
        )
        .unwrap()
        .momentary
        .get()
    };
    let (front, side, lfe) = (momentary(0), momentary(4), momentary(3));
    assert!((side - front - Db::from_power(1.41).get()).abs() < 0.01);
    assert_eq!(lfe, WindowedReading::LOUDNESS_FLOOR.get());
}

/// The classic inter-sample case: a sine at a quarter of the rate, phase 45
/// degrees, has samples of +-a/sqrt(2) and a true peak of `a`. At a = 0.5
/// the sample peak is -9.03 dBFS and the true peak -6.02 dBTP.
#[test]
fn the_true_peak_meter_reads_the_inter_sample_peak() {
    let rate = SampleRate::SR_48K;
    let samples: Vec<f32> = (0..48_000)
        // The phase repeats every four samples: reduced first, so the f32
        // argument stays exact.
        .map(|n| {
            0.5 * ((n % 4) as f32 * std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_4).sin()
        })
        .collect();
    let sample_peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((sample_peak - 0.5 / 2f32.sqrt()).abs() < 1e-3);
    let mut state = TruePeakConfig.measure((rate, ChannelLayout::MONO)).unwrap();
    state
        .push(
            Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
            &mut (),
        )
        .unwrap();
    let peak = state.finish().get();
    let analytic = Db::from_amplitude(Amplitude::new(0.5)).get();
    // The 4x oversampling filter reads 0.1 dB high at this phase.
    assert!((peak - analytic).abs() < 0.15, "{peak} vs {analytic}");
    assert!(
        peak > Db::from_amplitude(Amplitude::new(f64::from(sample_peak))).get() + 2.9,
        "{peak}"
    );
}

/// Silence reads the floor, and a chunk of another width is refused.
#[test]
fn the_true_peak_meter_floors_silence_and_refuses_a_wrong_width() {
    let mut state = TruePeakConfig
        .measure((SampleRate::SR_48K, ChannelLayout::STEREO))
        .unwrap();
    let loud = vec![0.9f32; 4_800 * 4];
    assert!(matches!(
        state.push(
            Interleaved::new(&loud, ChannelLayout::QUAD).unwrap(),
            &mut ()
        ),
        Err(MeterError::ChunkLayoutMismatch { .. })
    ));
    state
        .push(
            Interleaved::new(&[0.0f32; 960], ChannelLayout::STEREO).unwrap(),
            &mut (),
        )
        .unwrap();
    assert_eq!(state.finish(), Db::FLOOR);
}

/// The one-shot true peak is a meter fed the buffer, at any chunking.
#[test]
fn the_true_peak_one_shot_equals_a_chunked_meter() {
    let buf = sine(48_000.0, 0.5, 1_000.0, 0.7);
    let whole = Interleaved::new(&buf, ChannelLayout::STEREO).unwrap();
    let one_shot = measure_true_peak(whole, SampleRate::SR_48K).unwrap();
    for frames in [1, 777, 4800, 24_000] {
        let mut meter = TruePeakConfig
            .measure((SampleRate::SR_48K, ChannelLayout::STEREO))
            .unwrap();
        for chunk in buf.chunks(frames * 2) {
            meter
                .push(
                    Interleaved::new(chunk, ChannelLayout::STEREO).unwrap(),
                    &mut (),
                )
                .unwrap();
        }
        assert_eq!(meter.finish(), one_shot, "chunks of {frames} frames");
    }
}

/// The same fs/4 figure at 16-bit: the samples are the same sine rounded to
/// the format, and the meter reads its nominal level, -6.02 dBTP.
#[test]
fn the_true_peak_meter_reads_the_same_figure_at_i16() {
    let samples: Vec<i16> = (0..48_000)
        .map(|n| {
            let x = 0.5
                * ((n % 4) as f32 * std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_4)
                    .sin();
            x.convert_to::<i16>()
        })
        .collect();
    let peak = measure_true_peak(
        Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
        SampleRate::SR_48K,
    )
    .unwrap()
    .get();
    assert!(
        (peak - Db::from_amplitude(Amplitude::new(0.5)).get()).abs() < 0.15,
        "{peak}"
    );
}

/// The trait fold over uneven chunks (1, 7, 64 frames and the rest) is
/// `measure_loudness`, and a push yields no item.
#[test]
fn the_trait_fold_over_uneven_chunks_equals_measure_loudness() {
    let c = cfg(48_000.0);
    let buf = sine(48_000.0, 1.0, 1_000.0, 0.5);
    let batch =
        measure_loudness(&c, Interleaved::new(&buf, ChannelLayout::STEREO).unwrap()).unwrap();
    let (items, loudness) = fold_uneven(&c.0, c.1, buf.len() / 2, |meter, at, out| {
        let chunk = Interleaved::new(&buf[at.start * 2..at.end * 2], ChannelLayout::STEREO);
        meter.push(chunk.unwrap(), out)
    })
    .unwrap();
    assert!(items.is_empty());
    assert_eq!(loudness, batch);
    assert!(batch.lufs.get() > -20.0, "the tone is audible: {batch:?}");
}

/// The trait fold over uneven chunks is `measure_true_peak`, which reads
/// the inter-sample peak of this fs/4 sine.
#[test]
fn the_true_peak_trait_fold_over_uneven_chunks_equals_measure_true_peak() {
    let rate = SampleRate::SR_48K;
    let samples: Vec<f32> = (0..4_800)
        .map(|n| {
            0.5 * ((n % 4) as f32 * std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_4).sin()
        })
        .collect();
    let batch = measure_true_peak(
        Interleaved::new(&samples, ChannelLayout::MONO).unwrap(),
        rate,
    )
    .unwrap();
    let (items, peak) = fold_uneven(
        &TruePeakConfig,
        (rate, ChannelLayout::MONO),
        samples.len(),
        |meter, at, out| {
            meter.push(
                Interleaved::new(&samples[at], ChannelLayout::MONO).unwrap(),
                out,
            )
        },
    )
    .unwrap();
    assert!(items.is_empty());
    assert_eq!(peak, batch);
    // The samples reach 0.5 / sqrt 2 (-9.03 dB); the sine between them 0.5.
    assert!(
        (peak.get() - Db::from_amplitude(Amplitude::new(0.5)).get()).abs() < 0.15,
        "{peak:?}"
    );
}

/// The windowed fold yields one reading per push, in order, and its last
/// item and its output are the reading of one push of the whole stream: the
/// reading does not depend on how the stream is cut.
#[test]
fn the_windowed_trait_fold_yields_one_reading_per_push_and_ends_on_the_whole_stream_reading() {
    let c = windowed_cfg(48_000.0);
    let buf = windowed_sine(48_000.0, 1.0, |_| 0.1);
    let (items, last) = fold_uneven(&c.0, c.1, buf.len() / 2, |meter, at, out| {
        let chunk = Interleaved::new(&buf[at.start * 2..at.end * 2], ChannelLayout::STEREO);
        meter.push(chunk.unwrap(), out)
    })
    .unwrap();
    assert_eq!(items.len(), 4, "one reading for each of the four pushes");
    let mut whole = c.0.measure(c.1).unwrap();
    let reading = read(
        &mut whole,
        Interleaved::new(&buf, ChannelLayout::STEREO).unwrap(),
    )
    .unwrap();
    assert_eq!(items.last(), Some(&reading));
    assert_eq!(last, reading);
    assert_eq!(
        reading.frame,
        Samples::new(buf.len() / 2),
        "the reading names its end frame"
    );
    assert!(reading.momentary.get() > -30.0, "{reading:?}");
}

/// A 997 Hz tone in one channel of a 5.1 stream over 48 000 frames, which no
/// conversion block divides: in a surround channel it reads the 1.41 weight
/// (+1.49 dB) above the same tone in the left channel, and in the LFE it
/// reads nothing. A sample moved to another channel by a run cut off a frame
/// boundary breaks all three.
#[test]
fn a_wide_layout_keeps_every_sample_on_its_channel() {
    let c = Rated(
        LoudnessConfig::new(Bed::Surround51.topology()),
        SampleRate::SR_48K,
    );
    let frames = 48_000;
    let tone = |at: usize| {
        let mut buf = vec![0.0f32; frames * 6];
        for (n, frame) in buf.as_chunks_mut::<6>().0.iter_mut().enumerate() {
            frame[at] = 0.3 * (std::f32::consts::TAU * 997.0 * n as f32 / 48_000.0).sin();
        }
        measure_loudness(&c, Interleaved::new(&buf, Bed::Surround51.width()).unwrap())
            .unwrap()
            .lufs
            .get()
    };
    let (left, surround, lfe) = (tone(0), tone(4), tone(3));
    assert!(
        (surround - left - Db::from_power(1.41).get()).abs() < 0.05,
        "{left} {surround}"
    );
    assert_eq!(lfe, -70.0, "the LFE adds no loudness");
}

/// The true peak of a sine in one channel of 3 and of 6 equals the mono
/// true peak of that sine, to the bit: each channel's samples stay in its
/// own stream whatever the run length.
#[test]
fn the_true_peak_of_a_channel_does_not_depend_on_the_layout_width() {
    let rate = SampleRate::SR_48K;
    let sine: Vec<f32> = (0..5000)
        .map(|n| 0.4 * (std::f32::consts::TAU * 3_001.0 * n as f32 / 48_000.0).sin())
        .collect();
    let mono =
        measure_true_peak(Interleaved::new(&sine, ChannelLayout::MONO).unwrap(), rate).unwrap();
    for width in [3u16, 6] {
        let layout = ChannelLayout::from(width);
        let w = usize::from(width);
        let mut buf = vec![0.0f32; 5000 * w];
        for (frame, &v) in buf.chunks_exact_mut(w).zip(&sine) {
            frame[w - 1] = v;
        }
        let peak = measure_true_peak(Interleaved::new(&buf, layout).unwrap(), rate).unwrap();
        assert_eq!(peak, mono, "{width} channels");
    }
    assert!(
        (mono.get() - Db::from_amplitude(Amplitude::new(0.4)).get()).abs() < 0.3,
        "{mono:?}"
    );
}
