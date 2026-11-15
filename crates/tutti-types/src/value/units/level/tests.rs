use super::*;

/// What is deliberately *absent* is as load-bearing as what is present.
/// These must not compile; the ledger is here so a future contributor does
/// not "helpfully" add them.
///
/// - `Db * f32` — squares the amplitude rather than doubling the gain.
/// - `Db / Db` — a quotient of logarithms is not a quantity.
/// - `Amplitude + Amplitude` — cascading gains multiply; summing is what
///   the *signals* do, not their gains.
/// - `Gain * f32`, `f32 * Gain`, `Gain / f32` — `Amplitude` is `Scalable`, so
///   the same `x * 0.5` would read as scaling the gain there and as applying
///   it here. `Gain::apply` applies a gain to a sample; compose with
///   `Gain * Gain`.
/// - `Gain / Gain`, `Gain::to_db` — a quotient divides by a zero gain, and a
///   negative gain has no logarithm. `Gain::recip` refuses a non-finite
///   reciprocal; `to_amplitude` gives the `Amplitude` whose `to_db` is the level.
/// - `From<Gain> for Amplitude` — a negative gain has no amplitude. `to_amplitude`
///   refuses it. (`From<Amplitude>
///   for Gain` is present: total and lossless.)
/// - `From<raw> for Unit`, `From<Unit> for raw`, so `.into()` between a unit and
///   its bare number — a bare float must not become a unit unnamed at the call
///   site, and a unit must not leave as a float by an `.into()` that hides which
///   quantity it was. Build with `new`, read with `get` (generic code: the
///   `Unit` trait's `from_raw` / `to_raw`).
///
/// The body is empty and this test cannot fail; see the note in `units/tests.rs`.
#[test]
fn omitted_operators_are_documented() {}

#[test]
fn db_round_trips_through_amplitude() {
    assert_eq!(Db::UNITY.to_amplitude(), Amplitude(1.0));
    // -6 dB is very nearly half amplitude.
    assert!((Db(-6.0).to_amplitude().get() - 0.501_187).abs() < 1e-5);
    // +6 dB exceeds 1.0 — amplitude is not a 0..1 quantity.
    assert!(Db(6.0).to_amplitude().get() > 1.99);

    let round = Db::from_amplitude(Db(-12.0).to_amplitude());
    assert!((round.get() - -12.0).abs() < 1e-4);
}

#[test]
fn db_leaves_the_silence_floor_to_its_consumer() {
    // The conversion itself applies no floor. This is the property that
    // makes it a single-answer conversion, and therefore a `From`.
    assert!(Db::from_amplitude(Amplitude(0.0)).get().is_infinite());
    assert!(Db::from(Amplitude::SILENT).get().is_infinite());

    // -inf is what round-trips exactly: 10^(-inf/20) is 0.0, while
    // 10^(-144/20) is merely very small. A floor applied here would lose
    // that.
    assert_eq!(
        Db::from_amplitude(Amplitude::SILENT).to_amplitude(),
        Amplitude::SILENT
    );
    assert!(Db::FLOOR.to_amplitude().get() > 0.0);

    // `Db::FLOOR` still exists — it is what the consumers pin *to*, and
    // the three that do are the dynamics detectors, `ms_ratio`, and the
    // true-peak reading. It is simply not applied by the conversion.
    assert!(Db::FLOOR.get().is_finite());
}

#[test]
fn amplitude_is_not_a_normalized_scale() {
    // A gain routinely exceeds 1.0 — this is not a normalized `0..1`
    // scale. +6 dB is roughly a doubling.
    assert!(Db(6.0).to_amplitude() > Amplitude::UNITY);
    assert_eq!(Db::UNITY.to_amplitude(), Amplitude::UNITY);
    assert_eq!(Db::from(Amplitude::UNITY), Db::UNITY);

    // Scalable, because trimming a gain is meaningful.
    assert_eq!(Amplitude(2.0) * 0.5, Amplitude::UNITY);
}

#[test]
fn db_adds_because_gain_stages_cascade() {
    // Two -6 dB stages in series is -12 dB. This is why `Db` is additive
    // while `Beat` is not.
    assert_eq!(Db(-6.0) + Db(-6.0), Db(-12.0));
    assert_eq!(-Db(6.0), Db(-6.0));
    assert!(Db(-3.0) > Db(-6.0));
}

/// Silence is `-inf`, as `Db::from_amplitude` says, and the figures are the
/// decibel definition: `20 · log10(2) = 6.0206`.
#[test]
fn amplitude_to_db_leaves_silence_at_minus_infinity() {
    assert_eq!(Amplitude::SILENT.to_db().get(), f32::NEG_INFINITY);
    assert_eq!(Amplitude::UNITY.to_db(), Db::UNITY);
    assert!((Amplitude(2.0).to_db().get() - 6.0206).abs() < 1e-4);
    assert!((Amplitude(0.5).to_db().get() + 6.0206).abs() < 1e-4);
    assert!((Amplitude(0.1).to_db().get() + 20.0).abs() < 1e-5);
    // The floor is the caller's: it is a different, finite number.
    assert_ne!(Amplitude::SILENT.to_db(), Db::FLOOR);
}

/// The metering floor is 144 dB below full scale, about one 24-bit step
/// (`6.02 dB` a bit, 24 bits).
#[test]
fn the_metering_floor_is_144_db_down() {
    assert_eq!(Db::FLOOR.get(), -144.0);
    let amp = Db::FLOOR.to_amplitude().get();
    assert!((amp - 6.31e-8).abs() < 1e-9, "{amp:e}");
}

/// Signs multiply and magnitudes multiply: two inversions cancel.
#[test]
fn gains_compose_by_multiplying_their_signs() {
    assert_eq!(Gain(0.5) * Gain(4.0), Gain(2.0));
    assert_eq!(Gain(-0.5) * Gain(4.0), Gain(-2.0));
    assert_eq!(Gain::INVERTING * Gain::INVERTING, Gain::UNITY);
    assert_eq!(Gain(-3.0) * Gain::SILENT, Gain::SILENT);
    assert_eq!(Gain(-3.0) * Gain::UNITY, Gain(-3.0));
    let mut g = Gain(-2.0);
    g *= Gain(-0.25);
    assert_eq!(g, Gain(0.5));
}

/// Parallel paths sum, and a path and its inversion cancel.
#[test]
fn gains_sum_and_negate() {
    assert_eq!(Gain(0.75) + Gain(0.25), Gain::UNITY);
    assert_eq!(Gain::UNITY + Gain::INVERTING, Gain::SILENT);
    assert_eq!(Gain(0.5) - Gain(2.0), Gain(-1.5));
    assert_eq!(-Gain(0.5), Gain(-0.5));
    let mut g = Gain(1.0);
    g += Gain(-3.0);
    g -= Gain(0.5);
    assert_eq!(g, Gain(-2.5));
    assert!(Gain(-2.0) < Gain(-1.0));
    assert_eq!(Gain(-2.0).clamp(Gain(-1.0), Gain(1.0)), Gain(-1.0));
}

#[test]
fn a_gain_applies_to_a_sample_with_its_sign() {
    assert_eq!(Gain(0.5).apply(0.8), 0.4);
    assert_eq!(Gain::INVERTING.apply(0.25), -0.25);
    assert_eq!(Gain::UNITY.apply(0.3), 0.3);
    assert_eq!(Gain(-2.0).apply(0.5), -1.0);
    assert_eq!(Gain::SILENT.apply(0.5), 0.0);
}

#[test]
fn a_gain_converts_to_and_from_an_amplitude() {
    assert_eq!(Gain::from(Amplitude(0.5)), Gain(0.5));
    assert_eq!(Gain::from(Amplitude::UNITY), Gain::UNITY);

    assert_eq!(Gain(2.0).to_amplitude(), Some(Amplitude(2.0)));
    assert_eq!(Gain::SILENT.to_amplitude(), Some(Amplitude::SILENT));
    assert_eq!(Gain(-0.0).to_amplitude(), Some(Amplitude::SILENT));
    assert_eq!(Gain(-2.0).to_amplitude(), None);
    assert_eq!(Gain(f32::NAN).to_amplitude(), None);

    assert_eq!(Gain(0.5).to_amplitude(), Some(Amplitude(0.5)));
    assert_eq!(Gain(-0.5).to_amplitude(), None);
    assert_eq!(
        Gain(1.0).to_amplitude().map(Amplitude::to_db),
        Some(Db::UNITY)
    );
    for a in [0.0_f32, 0.25, 1.0, 8.0] {
        assert_eq!(Gain::from(Amplitude(a)).to_amplitude(), Some(Amplitude(a)));
    }
}

#[test]
fn a_gain_reciprocal_undoes_it_and_refuses_zero() {
    assert_eq!(Gain(4.0).recip(), Some(Gain(0.25)));
    assert_eq!(Gain(-0.5).recip(), Some(Gain(-2.0)));
    assert_eq!(Gain::SILENT.recip(), None);
    assert_eq!(Gain(-0.0).recip(), None);
    // 1 / 1e-45 overflows f32, and NaN has no reciprocal.
    assert_eq!(Gain::new(1e-45).recip(), None);
    assert_eq!(Gain(f32::NAN).recip(), None);
    assert_eq!(Gain(-4.0) * Gain(-4.0).recip().unwrap(), Gain::UNITY);
}

/// A tap, a routing weight: a driven, signed cell that reads back as written.
#[test]
fn a_gain_is_a_param() {
    let tap = crate::Param::new(Gain(-0.5));
    let on_audio_thread = tap.handle();
    assert_eq!(on_audio_thread.load(), Gain(-0.5));
    tap.store(Gain::INVERTING);
    assert_eq!(on_audio_thread.load(), Gain::INVERTING);

    // The width-erased cell a modulation lane writes keeps the sign.
    tap.cell()
        .store(0.75, core::sync::atomic::Ordering::Relaxed);
    assert_eq!(tap.load(), Gain(0.75));
}

#[test]
fn a_gain_has_no_range_so_a_cell_holds_any_float() {
    assert_eq!(<Gain as Unit>::from_raw(-3.0), Gain(-3.0));
    assert!(<Gain as Unit>::from_raw(f32::NAN).get().is_nan());
}
