use tutti_buffers::{Planes, PlanesMut};
use tutti_fixtures::signal::{bits, peak, sine};
use tutti_units::{Amplitude, Hz};

use super::*;

const RATE: SampleRate = SampleRate::SR_48K;
const STEREO: [ChannelWeight; 2] = [ChannelWeight::Unity, ChannelWeight::Unity];
/// One hop of 100 ms at 48 kHz.
const HOP_FRAMES: usize = 4_800;

/// Both channels of a 1 kHz sine of `dbfs` peak, `seconds` long.
fn tone(dbfs: f64, seconds: usize) -> [Vec<f32>; 2] {
    let one = sine(
        Hz::new(1_000.0),
        Db::new(dbfs).to_amplitude(),
        RATE,
        Samples::new(seconds * 48_000),
    );
    [one.clone(), one]
}

/// A bank at `RATE` on the stereo weights, at `controls`, its gain at rest.
fn bank(controls: LevellerControls) -> LevellerBank<f32> {
    let mut bank = LevellerBank::new(RATE, &STEREO);
    bank.retarget(&controls);
    bank
}

/// `audio` through `bank` in calls of at most `block` frames.
fn run(bank: &mut LevellerBank<f32>, audio: &[Vec<f32>], block: usize) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(Planes::new(&a).unwrap(), PlanesMut::new(&mut outs).unwrap());
        at = end;
    }
    out
}

/// The peak of the last second of `channel`, in dBFS.
fn last_second_peak_dbfs(channel: &[f32]) -> f64 {
    let tail = &channel[channel.len() - 48_000..];
    Amplitude::new(f64::from(peak(tail))).to_db().get()
}

/// EBU Tech 3341 case 1: a stereo 1 kHz sine at -23 dBFS peak per channel reads -23.0 LUFS
/// short-term, to the 0.1 LU the test tolerates; case 2's -33 dBFS reads -33.0.
#[test]
fn a_stereo_1_khz_sine_reads_its_dbfs_as_lufs_short_term_within_a_tenth_of_a_unit() {
    for dbfs in [-23.0, -33.0] {
        let mut bank = bank(LevellerControls::DEFAULT);
        run(&mut bank, &tone(dbfs, 4), 480);
        let lufs = bank.short_term().expect("a hop has completed").get();
        assert!((lufs - dbfs).abs() < 0.1, "{dbfs} dBFS read {lufs} LUFS");
    }
}

/// With no hop complete there is no reading, and the first completes after exactly 100 ms.
#[test]
fn the_first_reading_is_the_first_complete_hop() {
    let mut bank = bank(LevellerControls::DEFAULT);
    let x = tone(-23.0, 1);
    run(
        &mut bank,
        &[
            x[0][..HOP_FRAMES - 1].to_vec(),
            x[1][..HOP_FRAMES - 1].to_vec(),
        ],
        100,
    );
    assert_eq!(bank.short_term(), None);
    run(&mut bank, &[x[0][..1].to_vec(), x[1][..1].to_vec()], 1);
    assert!(bank.short_term().is_some());
}

/// A mono sine and the same sine in both channels of a stereo pair read 3.01 dB apart: the
/// sum over channels is a sum of mean squares (BS.1770 `z = sum G_i z_i`).
#[test]
fn a_second_equal_channel_adds_3_01_db() {
    let x = tone(-23.0, 4);
    let mut mono = LevellerBank::<f32>::new(RATE, &[ChannelWeight::Unity]);
    run(&mut mono, &x[..1], 480);
    let mut stereo = bank(LevellerControls::DEFAULT);
    run(&mut stereo, &x, 480);
    let gap = stereo.short_term().unwrap().get() - mono.short_term().unwrap().get();
    assert!((gap - Db::from_power(2.0).get()).abs() < 1e-6, "{gap}");
}

/// A surround channel counts 1.41 and the LFE nothing: the gap to a unity channel is
/// `10 log10 1.41` and the LFE reads silence.
#[test]
fn channel_weights_scale_the_power_and_the_lfe_is_silent() {
    let x = tone(-23.0, 4);
    let read = |w: ChannelWeight| {
        let mut b = LevellerBank::<f32>::new(RATE, &[w]);
        run(&mut b, &x[..1], 480);
        b.short_term().unwrap().get()
    };
    let gap = read(ChannelWeight::Surround) - read(ChannelWeight::Unity);
    assert!((gap - Db::from_power(1.41).get()).abs() < 1e-6, "{gap}");
    assert_eq!(read(ChannelWeight::Silent), f64::NEG_INFINITY);
}

/// A leveller fed a sine inside its gain range settles with the output at the target: the
/// output is Tech 3341 case 1's signal, which reads -23.0 LUFS, to 0.1 dB of the peak.
#[test]
fn the_steady_state_output_is_at_the_target_for_inputs_inside_the_gain_range() {
    for dbfs in [-29.0, -17.0] {
        let mut bank = bank(LevellerControls::DEFAULT);
        let y = run(&mut bank, &tone(dbfs, 30), 480);
        let out = last_second_peak_dbfs(&y[0]);
        assert!(
            (out + 23.0).abs() < 0.1,
            "{dbfs} dBFS settled at {out} dBFS"
        );
        assert_eq!(last_second_peak_dbfs(&y[0]), last_second_peak_dbfs(&y[1]));
    }
}

/// Outside the range the gain stops at the limit: input plus the most boost (or minus the most
/// cut), to 0.1 dB.
#[test]
fn the_steady_state_output_is_the_input_plus_the_limit_outside_the_gain_range() {
    for (dbfs, limit) in [(-40.0, 12.0), (-10.0, -12.0)] {
        let mut bank = bank(LevellerControls::DEFAULT);
        let y = run(&mut bank, &tone(dbfs, 30), 480);
        let out = last_second_peak_dbfs(&y[0]);
        assert!(
            (out - (dbfs + limit)).abs() < 0.1,
            "{dbfs} dBFS settled at {out} dBFS, wanted {}",
            dbfs + limit
        );
    }
}

/// The settled gain is the target less the reading, whatever the float: 1e-3 dB in `f32`.
#[test]
fn the_settled_gain_is_the_target_less_the_short_term_loudness() {
    let mut bank = bank(LevellerControls::DEFAULT);
    run(&mut bank, &tone(-29.0, 30), 480);
    let want = -23.0 - bank.short_term().unwrap().get();
    assert!(
        (bank.gain().get() - want).abs() < 1e-3,
        "{}",
        bank.gain().get()
    );
}

/// The same at `f64`, to 1e-9 dB after 60 s (the release's 2 s leaves 6 dB * e^-30 of the gap): the gain state is held in the bank's float, so an `f32` one
/// would be off by 1e-6.
#[test]
fn an_f64_bank_settles_the_gain_to_the_target_less_the_loudness_in_f64() {
    let mut bank = LevellerBank::<f64>::new(RATE, &STEREO);
    let step = std::f64::consts::TAU * 1_000.0 / 48_000.0;
    let amp = Db::new(-29.0).to_amplitude().get();
    let x: Vec<f64> = (0..48_000 * 60)
        .map(|i| amp * (step * i as f64).sin())
        .collect();
    let mut y = vec![0.0f64; x.len()];
    for (xs, ys) in x.chunks(480).zip(y.chunks_mut(480)) {
        bank.process(
            Planes::new(&[xs, xs]).unwrap(),
            PlanesMut::new(&mut [&mut ys[..], &mut xs.to_vec()[..]]).unwrap(),
        );
    }
    let want = -23.0 - bank.short_term().unwrap().get();
    assert!(
        (bank.gain().get() - want).abs() < 1e-9,
        "{}",
        bank.gain().get()
    );
}

/// The gain recovers with the release's time constant `tau`: after a step of the wanted gain
/// from `g0` to `g1` it reads `g1 + (g0 - g1) exp(-t / tau)`, within 0.01 dB.
#[test]
fn the_gain_rises_with_the_release_time_constant() {
    let controls = LevellerControls::DEFAULT;
    let mut bank = bank(controls);
    run(&mut bank, &tone(-29.0, 30), 480);
    let g0 = bank.gain().get();
    // The wanted gain is the target less the loudness, which is steady: -17 LUFS is 6 dB more.
    bank.retarget(&LevellerControls {
        target: Db::new(-17.0),
        ..controls
    });
    let g1 = g0 + 6.0;
    let mut t = 0.0;
    for _ in 0..4 {
        run(&mut bank, &tone(-29.0, 1), 480);
        t += 1.0;
        let want = g1 + (g0 - g1) * (-t / controls.release.get()).exp();
        assert!(
            (bank.gain().get() - want).abs() < 0.01,
            "t = {t} s: {}",
            bank.gain().get()
        );
    }
}

/// The gain falls with the attack's time constant, as the release's closed form above.
#[test]
fn the_gain_falls_with_the_attack_time_constant() {
    let controls = LevellerControls::DEFAULT;
    let mut bank = bank(controls);
    run(&mut bank, &tone(-29.0, 30), 480);
    let g0 = bank.gain().get();
    bank.retarget(&LevellerControls {
        target: Db::new(-29.0),
        ..controls
    });
    let g1 = g0 - 6.0;
    let mut t = 0.0;
    for _ in 0..4 {
        run(&mut bank, &tone(-29.0, 1), 480);
        t += 1.0;
        let want = g1 + (g0 - g1) * (-t / controls.attack.get()).exp();
        assert!(
            (bank.gain().get() - want).abs() < 0.01,
            "t = {t} s: {}",
            bank.gain().get()
        );
    }
}

/// Under the gate the gain does not rise: an input at -60 dBFS (under the -50 LUFS gate)
/// leaves the gain at exactly 0 dB, and with the gate lowered to -80 the same input is
/// boosted to the full 12 dB, so it is the gate that held it.
#[test]
fn the_gain_does_not_rise_under_the_gate() {
    let x = tone(-60.0, 20);
    let mut gated = bank(LevellerControls::DEFAULT);
    run(&mut gated, &x, 480);
    assert!(gated.short_term().unwrap().get() < -50.0);
    assert_eq!(gated.gain(), Db::UNITY);
    let mut open = bank(LevellerControls {
        gate: Db::new(-80.0),
        ..LevellerControls::DEFAULT
    });
    run(&mut open, &x, 480);
    assert!(
        (open.gain().get() - 12.0).abs() < 0.01,
        "{}",
        open.gain().get()
    );
}

/// A gain already cut stays where it is while the loudness falls under the gate: from the
/// first hop that reads under the gate on, it never rises.
#[test]
fn a_cut_gain_stays_put_once_the_loudness_is_under_the_gate() {
    let controls = LevellerControls {
        max_cut: Db::new(20.0),
        ..LevellerControls::DEFAULT
    };
    let mut bank = bank(controls);
    run(&mut bank, &tone(-10.0, 20), 480);
    let cut = bank.gain().get();
    assert!(cut < -12.0, "{cut}");
    let quiet = tone(-70.0, 1);
    let mut held = None;
    for _ in 0..200 {
        let block: Vec<Vec<f32>> = quiet.iter().map(|c| c[..HOP_FRAMES].to_vec()).collect();
        run(&mut bank, &block, 480);
        let gain = bank.gain().get();
        if bank.short_term().unwrap().get() < controls.gate.get() {
            let first = *held.get_or_insert(gain);
            assert_eq!(gain, first, "the gain moved under the gate");
        }
    }
    assert!(held.is_some(), "the loudness never fell under the gate");
}

/// The render does not depend on how the stream is cut into calls: a stereo signal whose
/// level steps (so the gain keeps moving) renders bit for bit alike in blocks of 1, 7, 64, 480
/// and 10 000 frames.
#[test]
fn the_output_is_the_same_bit_for_bit_whatever_the_blocks() {
    let mut x = tone(-35.0, 2);
    for (c, tail) in x.iter_mut().zip(tone(-14.0, 2)) {
        c.extend(tail);
    }
    let reference = run(&mut bank(LevellerControls::DEFAULT), &x, 480);
    for block in [1, 7, 64, 10_000] {
        let y = run(&mut bank(LevellerControls::DEFAULT), &x, block);
        for c in 0..2 {
            assert_eq!(
                bits(&y[c]),
                bits(&reference[c]),
                "block {block}, channel {c}"
            );
        }
    }
}

/// A NaN in the input is passed on in its own frame and not latched: the frames after it are
/// finite and the reading stays a number.
#[test]
fn a_non_finite_sample_is_not_latched() {
    let mut x = tone(-23.0, 2);
    x[0][10_000] = f32::NAN;
    x[1][10_000] = f32::INFINITY;
    let mut bank = bank(LevellerControls::DEFAULT);
    let y = run(&mut bank, &x, 480);
    assert!(y[0][10_001..].iter().all(|v| v.is_finite()));
    assert!(y[1][10_001..].iter().all(|v| v.is_finite()));
    assert!(bank.short_term().unwrap().get().is_finite());
    assert!(bank.gain().get().is_finite());
}

/// `reset` returns to 0 dB with no reading, and a controls value that is not finite reads as
/// the default's.
#[test]
fn reset_forgets_the_reading_and_a_non_finite_control_is_the_default() {
    let mut bank = bank(LevellerControls::DEFAULT);
    run(&mut bank, &tone(-29.0, 10), 480);
    assert!(bank.gain().get() > 1.0);
    bank.reset();
    assert_eq!(bank.short_term(), None);
    assert_eq!(bank.gain(), Db::UNITY);
    let nan = Db::new(f64::NAN);
    let hostile = LevellerControls {
        target: nan,
        max_boost: nan,
        max_cut: nan,
        gate: nan,
        attack: Seconds::new(f64::NAN),
        release: Seconds::new(f64::NAN),
    };
    assert_eq!(
        LevellerControls {
            attack: Seconds::ZERO,
            release: Seconds::ZERO,
            ..hostile
        }
        .designed()
        .target,
        LevellerControls::DEFAULT.target
    );
}
