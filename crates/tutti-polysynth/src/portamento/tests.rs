use super::*;
use tutti_curve::Exponent;

#[test]
fn mode_off_jumps_to_the_new_pitch() {
    let config = PortamentoConfig::new().mode(PortamentoMode::Off);
    let mut porta = Portamento::new(config, SampleRate::SR_44K1, Hz::new(440.0));

    porta.retarget(Hz::new(880.0), false);
    assert!((porta.tick().get() - 880.0).abs() < 0.01);
    assert!(!porta.is_gliding());
}

#[test]
fn always_glides_from_the_old_pitch_and_arrives() {
    let config = PortamentoConfig::new()
        .mode(PortamentoMode::Always)
        // 10ms
        .time(GlideTime::of(Seconds::new(0.01)));
    let mut porta = Portamento::new(config, SampleRate::SR_44K1, Hz::new(440.0));

    porta.reset(Hz::new(440.0));
    porta.retarget(Hz::new(880.0), false);

    // Should start near 440
    let first = porta.tick();
    assert!(first.get() < 500.0);

    // Run until glide finishes
    while porta.is_gliding() {
        porta.tick();
    }

    // Should end at 880
    assert!((porta.current().get() - 880.0).abs() < 1.0);
}

#[test]
fn legato_only_glides_only_on_an_overlapping_note() {
    let config = PortamentoConfig::new()
        .mode(PortamentoMode::LegatoOnly)
        .time(GlideTime::of(Seconds::new(0.01)));
    let mut porta = Portamento::new(config, SampleRate::SR_44K1, Hz::new(440.0));

    porta.reset(Hz::new(440.0));

    // Non-legato should not glide
    porta.retarget(Hz::new(880.0), false);
    assert!(!porta.is_gliding());
    assert!((porta.current().get() - 880.0).abs() < 0.01);

    // Legato should glide
    porta.retarget(Hz::new(440.0), true);
    assert!(porta.is_gliding());
}

#[test]
fn every_curve_arrives_at_the_target() {
    for curve in [
        Law::Linear,
        Law::PowerIn(Exponent::of(2.0)),
        Law::PowerIn(Exponent::of(0.5)),
    ] {
        let config = PortamentoConfig::new()
            .mode(PortamentoMode::Always)
            .curve(curve)
            .time(GlideTime::of(Seconds::new(0.01)));
        let mut porta = Portamento::new(config, SampleRate::SR_44K1, Hz::new(440.0));

        porta.reset(Hz::new(440.0));
        porta.retarget(Hz::new(880.0), false);

        // Run to completion
        while porta.is_gliding() {
            porta.tick();
        }

        // All curves should reach the target
        assert!((porta.current().get() - 880.0).abs() < 1.0);
    }
}

/// **`advance(n)` is `n` ticks**: the same frequency returned, the same
/// `current()` and the same progress after, bit for bit (one machine, one
/// libm), for every curve and several intervals, at step lengths that end inside the glide, on
/// its last frame and past it, and for zero frames.
#[test]
fn advance_is_that_many_ticks() {
    for curve in [
        Law::Linear,
        Law::PowerIn(Exponent::of(2.0)),
        Law::PowerIn(Exponent::of(0.5)),
    ] {
        let config = PortamentoConfig::new()
            .mode(PortamentoMode::Always)
            .curve(curve)
            // 44.1 frames: a glide that ends mid-step.
            .time(GlideTime::of(Seconds::new(0.001)));
        let (mut by_tick, mut by_advance) = (
            Portamento::new(config.clone(), SampleRate::SR_44K1, Hz::new(440.0)),
            Portamento::new(config, SampleRate::SR_44K1, Hz::new(440.0)),
        );
        for target in [587.0, 311.13, 1234.5, 97.3, 2093.0] {
            for p in [&mut by_tick, &mut by_advance] {
                p.reset(Hz::new(220.0));
                p.retarget(Hz::new(target), false);
            }
            for steps in [0usize, 1, 7, 16, 0, 19, 3, 16, 16] {
                let mut want = by_tick.current();
                for _ in 0..steps {
                    want = by_tick.tick();
                }
                let got = by_advance.advance(steps);
                assert_eq!(
                    got.get().to_bits(),
                    want.get().to_bits(),
                    "{curve:?} +{steps}"
                );
                assert_eq!(
                    by_advance.current().get().to_bits(),
                    by_tick.current().get().to_bits(),
                    "{curve:?} +{steps}: current"
                );
                assert_eq!(by_advance.glide, by_tick.glide);
            }
            assert!(!by_advance.is_gliding(), "the steps run past the glide");
            assert_eq!(
                by_advance.advance(1).get(),
                target,
                "a finished glide is its target"
            );
        }
    }
}

/// **A glide follows its law in cents**: `begin(from, to, time)` over
/// `time · rate` frames puts the pitch, `k` frames in, at `law(k / frames)` of
/// the interval in cents (pitch is exponential in time for `Law::Linear`), and
/// it arrives exactly on the frame the glide's length rounds up to, where the
/// pitch the voice plays (`ratio`) is the target's, for each law and an interval up, down and across several octaves. The
/// tolerance (1e-6 cents) covers the `ln`/`exp` of the pitch and the
/// square-root law's steepness at progress 0.
#[test]
fn a_glide_follows_its_law_in_cents() {
    let rate = SampleRate::SR_48K;
    for law in [
        Law::Linear,
        Law::PowerIn(Exponent::of(2.0)),
        Law::PowerIn(Exponent::of(0.5)),
    ] {
        for (from, to, time) in [
            (220.0f64, 1760.0f64, 0.01),
            (1760.0, 311.0, 0.0073),
            (95.0, 97.0, 0.05),
        ] {
            let mut porta = Portamento::new(
                PortamentoConfig::new()
                    .mode(PortamentoMode::Always)
                    .curve(law),
                rate,
                Hz::new(440.0),
            );
            porta.begin(Hz::new(from), Hz::new(to), Seconds::new(time));
            let frames = time * rate.get();
            let interval = 1200.0 * (to / from).log2();
            let mut k = 0usize;
            while porta.is_gliding() {
                k += 1;
                let got = 1200.0 * (porta.tick().get() / from).log2();
                let p = Normalized::clamped(k as f64 / frames);
                let want = interval * law.eval(p).get();
                assert!(
                    (got - want).abs() < 1e-6,
                    "{law:?} {from} -> {to} Hz, frame {k}: {got} cents, the law gives {want}"
                );
            }
            assert_eq!(
                k,
                frames.ceil() as usize,
                "{law:?} {from} -> {to} Hz: arrival frame"
            );
            assert_eq!(porta.ratio(), 1.0, "{law:?}: the voice plays the target");
        }
    }
}

/// A glide time is finite and above zero: `new`, `TryFrom` and `of` agree, and
/// zero (which is `PortamentoMode::Off`, not a time), negatives, NaN and the
/// infinities are refused. (Serde is not a feature of this crate, so the check
/// runs on the `try_from` shape written here.)
#[test]
fn glide_time_keeps_the_range_checked_shape() {
    use serde::{Deserialize, Serialize};
    use tutti_fixtures::{RangeContract, check_range_contract};

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Checked(GlideTime);
    impl Serialize for Checked {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.get().serialize(s)
        }
    }
    impl<'de> Deserialize<'de> for Checked {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            GlideTime::try_from(Seconds::deserialize(d)?)
                .map(Checked)
                .map_err(serde::de::Error::custom)
        }
    }
    impl TryFrom<Seconds> for Checked {
        type Error = GlideTimeError;
        fn try_from(raw: Seconds) -> Result<Self, Self::Error> {
            GlideTime::try_from(raw).map(Checked)
        }
    }
    impl From<Checked> for Seconds {
        fn from(c: Checked) -> Seconds {
            c.0.get()
        }
    }
    check_range_contract(&RangeContract {
        new: |raw| GlideTime::new(raw).map(Checked),
        get: |c| c.0.get(),
        of: Some(|raw| Checked(GlideTime::of(raw))),
        valid: &[
            Seconds::new(0.001),
            Seconds::new(0.1),
            Seconds::new(10_000.0),
        ],
        invalid: &[
            Seconds::new(0.0),
            Seconds::new(-0.1),
            Seconds::new(f64::NAN),
            Seconds::new(f64::INFINITY),
            Seconds::new(f64::NEG_INFINITY),
        ],
    });
}

/// **A constant-rate glide scales with the interval in octaves**, floored at
/// 10% of the base time: an octave takes the base time, and a quarter-tone
/// (a twentieth of an octave) takes a tenth of it, not a twentieth. The
/// glide length is the frame count to within one frame.
#[test]
fn a_constant_rate_glide_scales_with_the_interval_and_floors_at_a_tenth() {
    let rate = SampleRate::SR_48K;
    let frames_of = |from: f64, to: f64| {
        let mut porta = Portamento::new(
            PortamentoConfig::new()
                .mode(PortamentoMode::Always)
                .time(GlideTime::of(Seconds::new(1.0)))
                .constant_time(false),
            rate,
            Hz::new(from),
        );
        porta.retarget(Hz::new(to), false);
        let mut frames = 0u32;
        while porta.is_gliding() {
            porta.advance(1);
            frames += 1;
        }
        f64::from(frames)
    };
    let octave = frames_of(220.0, 440.0);
    assert!(
        (octave - rate.get()).abs() <= 1.0,
        "an octave took {octave}"
    );
    let quarter_tone = frames_of(440.0, 440.0 * 2f64.powf(0.5 / 12.0));
    assert!(
        (quarter_tone - 0.1 * rate.get()).abs() <= 1.0,
        "a quarter-tone took {quarter_tone}, the floor is a tenth of the base time"
    );
}

/// **Idle until set**: a glider at rest is not gliding, sits on its starting
/// pitch, and a glide begun from it leaves that pitch.
#[test]
fn a_glider_rests_on_the_pitch_it_is_given() {
    let mut porta = Portamento::new(
        PortamentoConfig::new().mode(PortamentoMode::Always),
        SampleRate::SR_48K,
        Hz::new(330.0),
    );
    assert!(!porta.is_gliding());
    assert_eq!(porta.current(), Hz::new(330.0));
    assert_eq!(porta.ratio(), 1.0);
    porta.retarget(Hz::new(660.0), false);
    assert!(porta.is_gliding());
    assert_eq!(
        porta.current(),
        Hz::new(330.0),
        "the glide starts where it rested"
    );
}
