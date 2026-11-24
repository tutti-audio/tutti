use super::*;
use tutti_buffers::Float;
use tutti_dsp::EnvelopeCurve;
use tutti_dsp::Noise;
use tutti_dsp::{LadderSaturation, nonlinear_ladder_step};
use tutti_units::{Amplitude, Db, Normalized, PhaseIncrement, Seconds};

const SR: f64 = 48_000.0;

fn sr() -> SampleRate {
    SampleRate::of(SR)
}

/// Instant attack, full sustain: the envelope is 1.0 from the first
/// sample, so the output is the oscillator/filter chain alone.
fn flat() -> Adsr {
    Adsr::new(
        Seconds::new(0.0),
        Seconds::new(0.0),
        Amplitude::new(1.0),
        Seconds::new(0.1),
    )
}

fn bank<F: SynthFloat>(osc: OscillatorType, filter: FilterType, env: Adsr) -> VoiceBank<F> {
    VoiceBank::new(osc, &filter, &env, sr(), 16, UnisonVoices::ONE)
}

/// Start `lane` sounding at `freq` with unity gains, as a note-on would.
fn start<F: SynthFloat>(
    bank: &mut VoiceBank<F>,
    lane: usize,
    freq: Hz,
    cutoff: Hz,
    res: Resonance,
) {
    bank.set_pitch(lane, freq);
    bank.set_filter(lane, 1, cutoff, res);
    bank.set_gains(lane, Amplitude::UNITY, Amplitude::UNITY);
    bank.snap_tone(lane);
    bank.snap_gain(lane);
    bank.start(lane, Phase::START);
    bank.gate_on(lane);
}

/// Render `frames` frames of the given lanes, one control step of `step`
/// frames at a time, returning the left channel.
fn render<F: SynthFloat>(
    bank: &mut VoiceBank<F>,
    lanes: &[usize],
    frames: usize,
    step: usize,
) -> Vec<F> {
    let mut out = Vec::with_capacity(frames);
    while out.len() < frames {
        let n = step.min(frames - out.len());
        for &lane in lanes {
            bank.mark_live(lane);
        }
        out.extend_from_slice(bank.render(n).0);
    }
    out
}

/// The envelope level after each of `frames` single-frame steps.
fn envelope_trace<F: SynthFloat>(bank: &mut VoiceBank<F>, lane: usize, frames: usize) -> Vec<f64> {
    (0..frames)
        .map(|_| {
            bank.mark_live(lane);
            bank.render(1);
            bank.envelope(lane).get()
        })
        .collect()
}

/// Attack, decay and release take their authored times, and sustain holds.
///
/// At 48 kHz: a 10 ms attack is 480 samples to 1.0, a 20 ms decay 960 more
/// to the 0.5 sustain, and a 30 ms release 1440 samples to silence.
#[test]
fn adsr_stages_take_their_authored_times() {
    let env = Adsr::new(
        Seconds::new(0.01),
        Seconds::new(0.02),
        Amplitude::new(0.5),
        Seconds::new(0.03),
    );
    let mut b = bank::<f32>(OscillatorType::Sine, FilterType::None, env);
    start(
        &mut b,
        0,
        Hz::new(440.0),
        Hz::new(20_000.0),
        Resonance::NONE,
    );

    let trace = envelope_trace(&mut b, 0, 3000);
    let peak = trace.iter().position(|&e| e >= 1.0).unwrap();
    assert!(
        (478..=481).contains(&peak),
        "attack peaked at sample {peak}, want ~479"
    );
    // Linear rise: half way at half time.
    assert!(
        (trace[239] - 0.5).abs() < 0.01,
        "attack midpoint {}",
        trace[239]
    );

    let sustained = peak + trace[peak..].iter().position(|&e| e <= 0.5).unwrap();
    assert!(
        (peak + 958..=peak + 962).contains(&sustained),
        "decay reached sustain at {sustained}, want ~{}",
        peak + 960
    );
    assert!(
        trace[2500..].iter().all(|&e| e == 0.5),
        "sustain did not hold at 0.5"
    );

    b.gate_off(0);
    let release = envelope_trace(&mut b, 0, 2000);
    let silent = release.iter().position(|&e| e == 0.0).unwrap();
    assert!(
        (1438..=1441).contains(&silent),
        "release reached zero at {silent}, want ~1439"
    );
    assert!(b.is_idle(0), "a finished release must leave the lane idle");
    assert!(
        (release[719] - 0.25).abs() < 0.01,
        "release midpoint {}",
        release[719]
    );
}

/// A release begun mid-attack falls from where the attack had got to, and
/// still takes the authored release time; a retrigger mid-release attacks
/// from where the release had got to, never dropping to zero first.
///
/// Both are what make a fast re-strike click-free. (`adsr_live` did the
/// first with a multiplier on the still-rising attack curve and restarted
/// the second from zero.)
#[test]
fn a_release_starts_from_the_current_level() {
    let env = Adsr::new(
        Seconds::new(0.01),
        Seconds::new(0.02),
        Amplitude::new(0.5),
        Seconds::new(0.03),
    );
    let mut b = bank::<f32>(OscillatorType::Sine, FilterType::None, env);
    start(
        &mut b,
        0,
        Hz::new(440.0),
        Hz::new(20_000.0),
        Resonance::NONE,
    );

    // 150 samples into a 480-sample attack.
    let _ = envelope_trace(&mut b, 0, 150);
    let at_release = b.envelope(0).get();
    assert!((at_release - 150.0 / 480.0).abs() < 0.01);
    b.gate_off(0);
    let release = envelope_trace(&mut b, 0, 1500);
    assert!(release[0] < at_release && release[0] > at_release - 0.01);
    assert!(
        release.windows(2).all(|w| w[1] <= w[0]),
        "release must fall monotonically"
    );
    let silent = release.iter().position(|&e| e == 0.0).unwrap();
    assert!(
        (1438..=1441).contains(&silent),
        "release took {silent} samples, want ~1439"
    );

    // Retrigger half way through a release from sustain.
    b.gate_on(0);
    let _ = envelope_trace(&mut b, 0, 2000);
    b.gate_off(0);
    let _ = envelope_trace(&mut b, 0, 720);
    let before = b.envelope(0).get();
    b.gate_on(0);
    // From 0.25 the attack needs 360 samples to peak; stop short of it.
    let attack = envelope_trace(&mut b, 0, 300);
    assert!(
        attack[0] >= before,
        "retrigger dropped the level from {before} to {}",
        attack[0]
    );
    assert!(
        attack.windows(2).all(|w| w[1] >= w[0]),
        "retriggered attack must rise"
    );
}

/// A lane renders the same whichever lane it is in, and lanes sum without
/// touching each other.
///
/// Run on the nonlinear ladder with a saw, the chain with the most per-lane
/// state, so a coefficient, state or gain written to the wrong lane shows.
#[test]
fn a_voice_renders_the_same_in_any_lane_and_lanes_sum() {
    let filter = FilterType::Moog {
        cutoff: Hz::new(1_500.0),
        resonance: Resonance::new(0.6),
        saturation: LadderSaturation::PerStage,
    };
    let solo = |lane: usize, freq: Hz| {
        let mut b = bank::<f32>(OscillatorType::Saw, filter, flat());
        start(&mut b, lane, freq, Hz::new(1_500.0), Resonance::new(0.6));
        render(&mut b, &[lane], 2048, CONTROL_BLOCK)
    };

    let in_lane_0 = solo(0, Hz::new(220.0));
    let in_lane_5 = solo(5, Hz::new(220.0));
    assert!(
        in_lane_0.iter().any(|s| s.abs() > 0.1),
        "the solo lane is silent"
    );
    assert_eq!(
        in_lane_0, in_lane_5,
        "lane 5 renders differently from lane 0"
    );

    // Two lanes of one group at once, and a lane in the next group.
    let other = solo(3, Hz::new(331.0));
    let far = solo(9, Hz::new(97.0));
    let mut b = bank::<f32>(OscillatorType::Saw, filter, flat());
    start(
        &mut b,
        0,
        Hz::new(220.0),
        Hz::new(1_500.0),
        Resonance::new(0.6),
    );
    start(
        &mut b,
        3,
        Hz::new(331.0),
        Hz::new(1_500.0),
        Resonance::new(0.6),
    );
    start(
        &mut b,
        9,
        Hz::new(97.0),
        Hz::new(1_500.0),
        Resonance::new(0.6),
    );
    let together = render(&mut b, &[0, 3, 9], 2048, CONTROL_BLOCK);
    for (i, &t) in together.iter().enumerate() {
        let sum = in_lane_0[i] + other[i] + far[i];
        assert!(
            (t - sum).abs() < 1e-5,
            "frame {i}: lanes together {t} but separately {sum}"
        );
    }
}

/// The steady-state amplitude of lane 0's output for a unit sine at `hz`: the bank's
/// sine oscillator through its filter, settled for 4800 frames, then measured over the
/// next 4800 by projecting onto `e^(-jwn)`. `hz` is a multiple of 10 Hz at 48 kHz, so the
/// window holds whole cycles and the projection is `A/2` exactly for a sine of amplitude `A`.
fn filtered_sine_amplitude<F: SynthFloat>(
    filter: FilterType,
    cutoff: Hz,
    res: Resonance,
    hz: f64,
) -> f64 {
    const WINDOW: usize = 4800;
    let mut b = bank::<F>(OscillatorType::Sine, filter, flat());
    start(&mut b, 0, Hz::new(hz), cutoff, res);
    let _ = render(&mut b, &[0], WINDOW, CONTROL_BLOCK);
    let y = render(&mut b, &[0], WINDOW, CONTROL_BLOCK);
    let w = core::f64::consts::TAU * hz / SR;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &v) in y.iter().enumerate() {
        let v: f64 = v.into();
        re += v * (w * n as f64).cos();
        im -= v * (w * n as f64).sin();
    }
    2.0 * re.hypot(im) / WINDOW as f64
}

/// The bilinear map `s/wc = (1/g)(1 - z^-1)/(1 + z^-1)` at `hz`, prewarped at `fc`, as
/// `(re, im)`: the map the trapezoidal integrators realise.
fn s_over_wc(fc: f64, hz: f64) -> (f64, f64) {
    let g = (core::f64::consts::PI * fc / SR).tan();
    let w = core::f64::consts::TAU * hz / SR;
    // (1 - e^-jw) / (1 + e^-jw) = j tan(w/2).
    (0.0, (w / 2.0).tan() / g)
}

fn mul(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}

fn norm(a: (f64, f64)) -> f64 {
    a.0.hypot(a.1)
}

/// `|H|` of the SVF tap at `hz`: with `s = s/wc` and `k = 1/Q`, `D = s^2 + k s + 1` and the
/// taps `1`, `s^2`, `s` and `s^2 + 1` over it (low-pass, high-pass, band-pass, notch).
fn svf_gain(mode: SvfMode, fc: f64, q: f64, hz: f64) -> f64 {
    let s = s_over_wc(fc, hz);
    let s2 = mul(s, s);
    let d = (s2.0 + s.0 / q + 1.0, s2.1 + s.1 / q);
    let num = match mode {
        SvfMode::Lowpass => (1.0, 0.0),
        SvfMode::Highpass => s2,
        SvfMode::Bandpass => s,
        SvfMode::Notch => (s2.0 + 1.0, s2.1),
    };
    norm(num) / norm(d)
}

/// The lane SVF's steady-state gain is the analytic `|H|` of its tap at the probes from a
/// decade below the cutoff to a decade above, at `f32` and `f64`.
///
/// The bound is relative to the larger of `|H|` and `0.01` (a notch's zero is a
/// subtraction of unit terms): `64 * EPSILON * Q^2`. Each coefficient is rounded by
/// `EPSILON / 2` and the response moves by at most `Q^2` times that relative figure near
/// the resonance. The projection's own error over 4800 samples is far below it.
fn svf_gain_matches_the_closed_form<F: SynthFloat>() {
    let (fc, q) = (900.0f64, 2.0f64);
    let eps: f64 = F::EPSILON.into();
    for mode in [
        SvfMode::Lowpass,
        SvfMode::Highpass,
        SvfMode::Bandpass,
        SvfMode::Notch,
    ] {
        let filter = FilterType::Svf {
            cutoff: Hz::new(fc),
            q: crate::SvfQ::of(Q::new(q)),
            mode,
        };
        for hz in [90.0, 450.0, 900.0, 3_000.0, 9_000.0] {
            let want = svf_gain(mode, fc, q, hz);
            let got = filtered_sine_amplitude::<F>(filter, Hz::new(fc), Resonance::NONE, hz);
            let bound = 64.0 * eps * q * q * want.max(0.01);
            assert!(
                (got - want).abs() < bound,
                "{mode:?} at {hz} Hz: gain {got}, closed form {want}, bound {bound}"
            );
        }
    }
}

#[test]
fn the_lane_svf_gain_is_the_closed_form_at_both_widths() {
    svf_gain_matches_the_closed_form::<f32>();
    svf_gain_matches_the_closed_form::<f64>();
}

/// `|1 / ((1 + s)^4 + k)|`: the 24 dB ladder, `k = 4 res`, at `hz`.
fn ladder_gain(fc: f64, res: f64, hz: f64) -> f64 {
    let p = {
        let s = s_over_wc(fc, hz);
        (1.0 + s.0, s.1)
    };
    let p2 = mul(p, p);
    let p4 = mul(p2, p2);
    1.0 / norm((p4.0 + 4.0 * res, p4.1))
}

/// The lane ladder's gain is `|1 / ((1 + s/wc)^4 + k)|`, `k = 4 res`, under the bilinear map
/// prewarped at the cutoff, in both saturation models. The bank's own coefficients (`g1`, `k`
/// of the lane) drive the step the bank renders with (`ladder_lanes`, with the lane `tanh`)
/// and a sine of amplitude `1e-3`: the oscillator's unit amplitude would saturate. A saturator's
/// cubic term is `u^2 / 3` of its input `u`, and `u = x / |1 + k L^4|` reaches `2x` at the
/// cutoff for `res = 0.5` (`L^4 = -1/4`, `k = 2`), so `(2e-3)^2 / 3 = 1.3e-6` of the signal
/// per saturator; the per-stage model has five, the input model one. The bound is `1e-5` for
/// that plus `64 * EPSILON` of the response.
fn ladder_gain_matches_the_closed_form<F: SynthFloat>() {
    type L<F> = <F as SynthFloat>::Lanes;
    let (fc, res) = (1_200.0f64, 0.5f64);
    let eps: f64 = F::EPSILON.into();
    let filter = FilterType::Moog {
        cutoff: Hz::new(fc),
        resonance: Resonance::new(res),
        saturation: LadderSaturation::PerStage,
    };
    let mut b = bank::<F>(OscillatorType::Sine, filter, flat());
    b.set_filter(0, 1, Hz::new(fc), Resonance::new(res));
    let c = b.coef_target[0];
    let coefficients = tutti_dsp::LadderCoefficients { g1: c[0], k: c[1] };
    const AMPLITUDE: f64 = 1e-3;
    const WINDOW: usize = 4800;
    for (per_stage, hz) in [false, true]
        .into_iter()
        .flat_map(|m| [120.0, 600.0, 1_200.0, 4_800.0].map(|hz| (m, hz)))
    {
        let w = core::f64::consts::TAU * hz / SR;
        let mut z = [L::<F>::ZERO; 4];
        let (mut re, mut im) = (0.0, 0.0);
        for n in 0..2 * WINDOW {
            let x = L::<F>::splat(F::from_design(AMPLITUDE * (w * n as f64).sin()));
            let y = ladder_lanes(per_stage, &coefficients, &mut z, x);
            if n >= WINDOW {
                let y: f64 = y.as_slice()[0].into();
                let m = (n - WINDOW) as f64;
                re += y * (w * m).cos();
                im -= y * (w * m).sin();
            }
        }
        let got = 2.0 * re.hypot(im) / WINDOW as f64 / AMPLITUDE;
        let want = ladder_gain(fc, res, hz);
        let bound = 1e-5 + 64.0 * eps;
        assert!(
            (got - want).abs() < bound * want.max(0.01),
            "{hz} Hz, per stage {per_stage}: gain {got}, closed form {want}"
        );
    }
}

/// The per-stage solve over lanes is the scalar per-stage solve, bit for bit: one generic
/// `nonlinear_ladder_step`, its scalar instance saturating through the lane `tanh` of one
/// splatted value. Every lane carries its own loud input (a sine of amplitude `4 (k + 1) /
/// LANES`, so the saturators bite) through a resonant ladder, and a lane's output equals the
/// scalar run on that input at every sample: the lane arithmetic is the scalar's add, multiply
/// and divide in the same order, and the `tanh` is the same function.
fn lane_per_stage_ladder_is_the_scalar_solve<F: SynthFloat + SimdLanes>() {
    type L<F> = <F as SynthFloat>::Lanes;
    let filter = FilterType::Moog {
        cutoff: Hz::new(1_200.0),
        resonance: Resonance::new(0.8),
        saturation: LadderSaturation::PerStage,
    };
    let mut b = bank::<F>(OscillatorType::Sine, filter, flat());
    b.set_filter(0, 1, Hz::new(1_200.0), Resonance::new(0.8));
    let c = b.coef_target[0];
    let scalar = LadderCoefficients {
        g1: c[0].as_slice()[0],
        k: c[1].as_slice()[0],
    };
    let lanes = LadderCoefficients {
        g1: L::<F>::splat(scalar.g1),
        k: L::<F>::splat(scalar.k),
    };
    let w = core::f64::consts::TAU * 300.0 / SR;
    let mut z = [L::<F>::ZERO; 4];
    let mut zs = [[F::ZERO; 4]; 8];
    let mut loud = 0.0f64;
    for n in 0..2_000 {
        let mut x = L::<F>::ZERO;
        for (k, lane) in x.as_mut_slice().iter_mut().enumerate() {
            let gain = 4.0 * (k + 1) as f64 / L::<F>::LANES as f64;
            *lane = F::from_design(gain * (w * f64::from(n)).sin());
        }
        let y = ladder_lanes(true, &lanes, &mut z, x);
        for (k, zk) in zs.iter_mut().enumerate().take(L::<F>::LANES) {
            let xk = x.as_slice()[k];
            loud = loud.max(xk.into().abs());
            let want = nonlinear_ladder_step(&scalar, LadderType::LP24, zk, xk, |v| {
                kernel::tanh(L::<F>::splat(v)).as_slice()[0]
            });
            assert_eq!(y.as_slice()[k], want, "lane {k}, sample {n}");
        }
    }
    assert!(loud > 3.0, "the input never reached the saturators: {loud}");
}

/// A bank renders its configured model: a unit sine at 100 Hz through the ladder at 18 kHz with
/// no resonance is quasi-static (the stages' lag is under 1% of the peak), so its peak is `tanh`
/// of the unit peak applied once by the input model and four times by the per-stage model.
#[test]
fn a_bank_peaks_at_the_saturation_of_its_model() {
    for (saturation, times) in [
        (LadderSaturation::Input, 1),
        (LadderSaturation::PerStage, 4),
    ] {
        let filter = FilterType::Moog {
            cutoff: Hz::new(18_000.0),
            resonance: Resonance::NONE,
            saturation,
        };
        let mut b = VoiceBank::<f64>::new(
            OscillatorType::Sine,
            &filter,
            &flat(),
            sr(),
            16,
            UnisonVoices::ONE,
        );
        start(
            &mut b,
            0,
            Hz::new(100.0),
            Hz::new(18_000.0),
            Resonance::NONE,
        );
        let peak = render(&mut b, &[0], 4_800, CONTROL_BLOCK)
            .into_iter()
            .fold(0.0, f64::max);
        let want = (0..times).fold(1.0f64, |x, _| x.tanh());
        assert!(
            (peak - want).abs() < 0.01 * want,
            "{saturation:?}: peak {peak}, want {want}"
        );
    }
}

#[test]
fn the_lane_per_stage_ladder_is_the_scalar_solve_at_both_widths() {
    lane_per_stage_ladder_is_the_scalar_solve::<f32>();
    lane_per_stage_ladder_is_the_scalar_solve::<f64>();
}

#[test]
fn the_lane_ladder_gain_is_the_closed_form_at_both_widths() {
    ladder_gain_matches_the_closed_form::<f32>();
    ladder_gain_matches_the_closed_form::<f64>();
}

/// A pitch change ramps across the control step, one increment per
/// sample — never a stair.
///
/// Read off a saw well away from its wrap, where the output is the naive
/// ramp `2p - 1` exactly, so each sample's phase step is recoverable.
#[test]
fn a_pitch_change_ramps_every_sample_across_the_step() {
    let mut b = bank::<f32>(OscillatorType::Saw, FilterType::None, flat());
    start(
        &mut b,
        0,
        Hz::new(100.0),
        Hz::new(20_000.0),
        Resonance::NONE,
    );
    b.start(0, Phase::new(0.1));
    let _ = render(&mut b, &[0], CONTROL_BLOCK, CONTROL_BLOCK);

    b.set_pitch(0, Hz::new(200.0));
    let out = render(&mut b, &[0], CONTROL_BLOCK + 1, CONTROL_BLOCK);
    let phase: Vec<f64> = out.iter().map(|&y| (f64::from(y) + 1.0) / 2.0).collect();
    let steps: Vec<f64> = phase.windows(2).map(|w| w[1] - w[0]).collect();
    let (from, to) = (100.0 / SR, 200.0 / SR);

    assert!(
        steps
            .windows(2)
            .take(CONTROL_BLOCK - 1)
            .all(|w| w[1] > w[0] + 1e-6),
        "the phase step did not rise every sample: {steps:?}"
    );
    assert!(
        steps[0] > from + 1e-6,
        "the ramp did not start moving on the first sample"
    );
    assert!(
        (steps[CONTROL_BLOCK - 1] - to).abs() < 1e-6,
        "the ramp ended at {} rather than the target {to}",
        steps[CONTROL_BLOCK - 1]
    );
}

/// A sustain above unity is clamped, so a retrigger at sustain never drops
/// the level: the attack peaks at 1.0 and would otherwise hand decay a
/// level below where the note was held.
#[test]
fn a_retrigger_at_a_boosted_sustain_does_not_drop() {
    let env = Adsr::new(
        Seconds::new(0.001),
        Seconds::new(0.001),
        Amplitude::new(1.5),
        Seconds::new(0.1),
    );
    let mut b = bank::<f32>(OscillatorType::Sine, FilterType::None, env);
    start(
        &mut b,
        0,
        Hz::new(440.0),
        Hz::new(20_000.0),
        Resonance::NONE,
    );
    let held = envelope_trace(&mut b, 0, 500);
    let before = *held.last().unwrap();
    b.gate_on(0);
    let after = envelope_trace(&mut b, 0, 100);
    assert!(
        after.iter().all(|&e| e >= before),
        "retrigger dropped the level from {before} to {:?}",
        after.iter().cloned().fold(f64::INFINITY, f64::min)
    );
    assert!(
        before <= 1.0,
        "the held level {before} exceeds the clamped sustain"
    );
}

/// A dB-linear release runs on the lanes as a multiply per frame: `n` frames in, the level is
/// `10^(-depth n / (20 R))` of the level it began from, to the recurrence's rounding: each
/// multiply errs by half an epsilon relative and the factor's own rounding adds the same
/// per frame, so `n` frames stay within `(n + 1) epsilon` relative.
fn a_db_linear_release_is_a_straight_line_in_decibels<F: SynthFloat>(epsilon: f64) {
    let depth = 60.0;
    let release = 4800.0;
    let mut b = bank::<F>(OscillatorType::Sine, FilterType::None, flat());
    let stages = EnvelopeStages::new(
        Seconds::new(0.0),
        Seconds::new(0.0),
        Normalized::of(1.0),
        Seconds::new(release / SR),
    )
    .release_curve(EnvelopeCurve::db_linear(Db::new(depth)).expect("a depth above zero"));
    b.design = EnvelopeDesign::new(&stages, sr());
    start(
        &mut b,
        0,
        Hz::new(440.0),
        Hz::new(20_000.0),
        Resonance::NONE,
    );
    let _ = envelope_trace(&mut b, 0, 10);
    b.gate_off(0);
    let trace = envelope_trace(&mut b, 0, 2400);
    for n in [1_usize, 100, 1200, 2400] {
        let want = 10.0_f64.powf(-depth * n as f64 / release / 20.0);
        let bound = (n as f64 + 1.0) * epsilon * want;
        assert!(
            (trace[n - 1] - want).abs() <= bound,
            "{n} frames: {} against {want}",
            trace[n - 1]
        );
    }
}

#[test]
fn a_db_linear_release_is_a_straight_line_in_decibels_at_both_widths() {
    a_db_linear_release_is_a_straight_line_in_decibels::<f64>(f64::EPSILON);
    a_db_linear_release_is_a_straight_line_in_decibels::<f32>(f64::from(f32::EPSILON));
}

/// A pulse width of 0 still renders a pulse, not DC.
#[test]
fn a_zero_pulse_width_still_oscillates() {
    let mut b = bank(
        OscillatorType::Square {
            pulse_width: Normalized::of(0.0),
        },
        FilterType::None,
        flat(),
    );
    start(
        &mut b,
        0,
        Hz::new(220.0),
        Hz::new(20_000.0),
        Resonance::NONE,
    );
    let out = render(&mut b, &[0], 4800, CONTROL_BLOCK);
    let mean = out.iter().sum::<f32>() / out.len() as f32;
    let ac = (out.iter().map(|s| (s - mean).powi(2)).sum::<f32>() / out.len() as f32).sqrt();
    assert!(ac > 0.05, "a zero-width pulse rendered DC (AC rms {ac})");
}

/// The first four samples of every lane's stream, as bits: two lanes agree on them only if
/// they share a seed (the stream is a bijection of the seed, and a four-sample prefix of
/// 24 or 53 bits each is distinct for distinct seeds with overwhelming probability).
fn first_samples<F: Float, const N: usize>(rng: &[[Noise<F>; N]]) -> Vec<[u64; 4]> {
    rng.iter()
        .flat_map(|g| {
            g.clone().map(|mut lane| {
                core::array::from_fn(|_| {
                    let sample: f64 = lane.next_sample().into();
                    sample.to_bits()
                })
            })
        })
        .collect()
}

/// Every lane of a built bank, across voices and across one voice's unison
/// stack, starts its noise generator on its own seed: two lanes on one seed
/// would play the same noise, in both ears of a stereo stack.
#[test]
fn every_lane_starts_on_its_own_noise_seed() {
    let bank = VoiceBank::<f32>::new(
        OscillatorType::Noise,
        &FilterType::None,
        &flat(),
        sr(),
        16,
        UnisonVoices::of(7),
    );
    let seeds = first_samples(&bank.rng);
    assert!(seeds.len() >= 16 * 7);
    let mut sorted = seeds.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seeds.len(), "two lanes share a seed");
}

/// A bank of one voice at `rate`, a flat envelope of `release` and `sustain`.
fn extreme_bank<F: SynthFloat>(rate: SampleRate, env: Adsr) -> VoiceBank<F> {
    VoiceBank::new(
        OscillatorType::Sine,
        &FilterType::None,
        &env,
        rate,
        1,
        UnisonVoices::ONE,
    )
}

/// Render `frames` frames of lane 0 a control step at a time.
fn run<F: SynthFloat>(bank: &mut VoiceBank<F>, frames: usize) {
    let mut left = frames;
    while left > 0 {
        let n = left.min(CONTROL_BLOCK);
        bank.mark_live(0);
        bank.render(n);
        left -= n;
    }
}

/// A release's level after `elapsed` of its `length` frames, from `from`: the straight
/// line the envelope draws.
fn release_level(from: f64, elapsed: usize, length: usize) -> f64 {
    from * (1.0 - elapsed as f64 / length as f64)
}

/// The longest release at the highest rate: a 100 s release at 192 kHz is
/// 19.2 million frames, so its slope is `1/N = 5.2e-8` of the level per frame. From
/// a level of 0.27, in `[0.25, 0.5)` where `f32` steps by `2^-25 = 3e-8`, the slope
/// `1.4e-8` is under half a step: a level that adds its slope plainly never moves, and
/// the voice never ends. The level adds its slope compensated, so after a hundredth
/// of the release it is a hundredth of the way down.
///
/// The bound is 4 steps of `F` at the starting level (`4 * 3e-8 = 1.2e-7` at `f32`, the
/// level's own rounding, the slope's relative rounding and the final rounding of the
/// compensated sum, with margin; `4 * 2^-54` at `f64`). A plain `level += slope` at
/// `f32` stalls at the starting level, so the bound also fails a level that never moved.
/// Returns the starting level.
fn a_100_second_release_moves_at_its_authored_slope<F: SynthFloat>(epsilon: f64) -> f64 {
    const RELEASE: usize = 19_200_000;
    /// One second of it, a hundredth of the release.
    const ELAPSED: usize = RELEASE / 100;
    let env = Adsr::new(
        Seconds::new(0.0),
        Seconds::new(0.0),
        Amplitude::new(0.27),
        Seconds::new(100.0),
    );
    let mut b = extreme_bank::<F>(SampleRate::SR_192K, env);
    start(
        &mut b,
        0,
        Hz::new(440.0),
        Hz::new(20_000.0),
        Resonance::NONE,
    );
    run(&mut b, 32);
    let from = b.envelope(0).get();
    b.gate_off(0);
    run(&mut b, ELAPSED);

    let level = b.envelope(0).get();
    let want = release_level(from, ELAPSED, RELEASE);
    let bound = 4.0 * epsilon / 2.0 * 0.5;
    assert!(
        (level - want).abs() < bound,
        "mid-release level {level}, closed form {want}, bound {bound}"
    );
    from
}

#[test]
fn a_100_second_release_at_192_khz_moves_at_its_authored_slope_at_both_widths() {
    let from = a_100_second_release_moves_at_its_authored_slope::<f32>(f64::from(f32::EPSILON));
    assert_eq!(from, f64::from(0.27_f32), "the sustain level");
    let from = a_100_second_release_moves_at_its_authored_slope::<f64>(f64::EPSILON);
    assert_eq!(from, 0.27);
}

/// The distance between two phases on the unit circle.
fn turn_distance(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(1.0);
    d.min(1.0 - d)
}

/// A voice's phase after `frames` at `freq`: the closed form `frames * freq / rate`,
/// modulo a turn. The increment is stored at `F` and so is off by half a step of `F`
/// (`EPSILON / 2` relative), which `cycles` of them add up to; the phase's own rounding
/// and the compensation's residue are a step each.
fn pitch_after<F: SynthFloat>(freq: Hz, rate: SampleRate, frames: usize) {
    let mut b = extreme_bank::<F>(
        rate,
        Adsr::new(
            Seconds::new(0.0),
            Seconds::new(0.0),
            Amplitude::new(1.0),
            Seconds::new(0.1),
        ),
    );
    start(&mut b, 0, freq, Hz::new(20_000.0), Resonance::NONE);
    b.start(0, Phase::START);
    run(&mut b, frames);

    // The exact phase: `frames * freq / rate` in whole-Hz rationals, modulo a turn.
    let (num, den) = (freq.get() as u128, rate.get() as u128);
    assert!(freq.get() == num as f64 && rate.get() == den as f64);
    let want = ((frames as u128 * num) % den) as f64 / den as f64;
    let cycles = frames as f64 * freq.get() / rate.get();
    let got: f64 = b.phase(0).into();
    let eps: f64 = F::EPSILON.into();
    let bound = cycles * eps / 2.0 + 3.0 * eps;
    assert!(
        turn_distance(got, want) < bound,
        "phase after {frames} frames {got}, exact {want}, bound {bound}"
    );

    // The plain sum of the same stored increment, which the bank no longer does, is
    // out of the bound.
    let inc = F::from_unit(PhaseIncrement::per_sample(freq, rate));
    let mut plain = F::ZERO;
    for _ in 0..frames {
        plain += inc;
        if plain >= F::ONE {
            plain -= F::ONE;
        }
    }
    let plain: f64 = plain.into();
    assert!(
        turn_distance(plain, want) > 2.0 * bound,
        "the plain sum {plain} stays within the bound {bound} of {want}"
    );
}

/// 20 Hz at 192 kHz is `1.04e-4` of a turn a frame: a second and a half cycle (the
/// last 4800 frames) are 20.5 cycles. A phase that adds that increment plainly loses
/// part of every sum to the phase's `6e-8` step, and drifts; the compensated one holds
/// the pitch to the increment's own rounding.
#[test]
fn a_20_hz_voice_at_192_khz_keeps_its_phase_for_a_second_and_a_half_cycle_at_both_widths() {
    pitch_after::<f32>(Hz::new(20.0), SampleRate::SR_192K, 192_000 + 4_800);
    pitch_after::<f64>(Hz::new(20.0), SampleRate::SR_192K, 192_000 + 4_800);
}

/// At `f64` the closed-form tests of the envelope's stage times hold as at `f32`, with
/// the stages one sample off at most.
#[test]
fn adsr_stages_take_their_authored_times_at_f64() {
    let env = Adsr::new(
        Seconds::new(0.01),
        Seconds::new(0.02),
        Amplitude::new(0.5),
        Seconds::new(0.03),
    );
    let mut b = bank::<f64>(OscillatorType::Sine, FilterType::None, env);
    start(
        &mut b,
        0,
        Hz::new(440.0),
        Hz::new(20_000.0),
        Resonance::NONE,
    );
    let trace = envelope_trace(&mut b, 0, 3000);
    let peak = trace.iter().position(|&e| e >= 1.0).unwrap();
    assert!(
        (478..=481).contains(&peak),
        "attack peaked at sample {peak}"
    );
    assert!(
        (trace[239] - 0.5).abs() < 1e-3,
        "attack midpoint {}",
        trace[239]
    );
    let sustained = peak + trace[peak..].iter().position(|&e| e <= 0.5).unwrap();
    assert!((peak + 958..=peak + 962).contains(&sustained));
    assert!(trace[2500..].iter().all(|&e| e == 0.5));
    b.gate_off(0);
    let release = envelope_trace(&mut b, 0, 2000);
    let silent = release.iter().position(|&e| e == 0.0).unwrap();
    assert!(
        (1438..=1441).contains(&silent),
        "release reached zero at {silent}"
    );
    assert!((release[719] - 0.25).abs() < 1e-3);
}

/// A lane renders the same in any lane of the group and the lanes sum, at `f64`: the
/// sum of lanes rendered apart is the sum rendered together to a few ulp.
///
/// The sum of three samples under unity differs from three separate renders by at
/// most the rounding of two additions, `2 * 2^-53 * 3`; the bound is `1e-14`.
#[test]
fn lanes_sum_and_do_not_touch_each_other_at_f64() {
    let filter = FilterType::Moog {
        cutoff: Hz::new(1_500.0),
        resonance: Resonance::new(0.6),
        saturation: LadderSaturation::PerStage,
    };
    let solo = |lane: usize, freq: Hz| {
        let mut b = bank::<f64>(OscillatorType::Saw, filter, flat());
        start(&mut b, lane, freq, Hz::new(1_500.0), Resonance::new(0.6));
        render(&mut b, &[lane], 2048, CONTROL_BLOCK)
    };
    let (a, o, far) = (
        solo(0, Hz::new(220.0)),
        solo(3, Hz::new(331.0)),
        solo(5, Hz::new(97.0)),
    );
    assert_eq!(
        a,
        solo(2, Hz::new(220.0)),
        "lane 2 renders differently from lane 0"
    );
    let mut b = bank::<f64>(OscillatorType::Saw, filter, flat());
    for (lane, freq) in [(0, 220.0), (3, 331.0), (5, 97.0)] {
        start(
            &mut b,
            lane,
            Hz::new(freq),
            Hz::new(1_500.0),
            Resonance::new(0.6),
        );
    }
    let together = render(&mut b, &[0, 3, 5], 2048, CONTROL_BLOCK);
    for (i, &t) in together.iter().enumerate() {
        assert!((t - (a[i] + o[i] + far[i])).abs() < 1e-14, "frame {i}");
    }
}

/// Every lane of an `f64` bank, which holds four to a group, starts on its own seed.
#[test]
fn every_lane_of_an_f64_bank_starts_on_its_own_noise_seed() {
    let bank = VoiceBank::<f64>::new(
        OscillatorType::Noise,
        &FilterType::None,
        &flat(),
        sr(),
        16,
        UnisonVoices::of(7),
    );
    let seeds = first_samples(&bank.rng);
    assert!(seeds.len() >= 16 * 7);
    let mut sorted = seeds.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seeds.len(), "two lanes share a seed");
}
