use super::*;

// Each family's tests hold an omission ledger: an empty
// `omitted_operators_are_documented` test whose doc comment lists the
// operators its types deliberately lack. The body is empty and the test cannot
// fail. It exists as documentation: the ledger is the contract, and a `#[test]`
// is what keeps it beside the operators rather than drifting into a README.
// Adding an omitted operator means deleting its entry, not making a test pass.
//
// Every unit also lacks `From<raw>` and `From<Unit> for raw`; each ledger lists
// that omission, and `new_and_get_are_the_one_way_between_a_unit_and_its_raw`
// below pins the replacement.

/// The replacement for the omitted `From<raw>` / `From<Unit> for raw`: `new`
/// and `get` agree with the `Unit` trait's `from_raw` / `to_raw`, one of each
/// per representation (float, and an integer count).
#[test]
fn new_and_get_are_the_one_way_between_a_unit_and_its_raw() {
    assert_eq!(Hz::new(440.0), Hz::from_raw(440.0));
    assert_eq!(Hz::new(440.0).get(), Hz::new(440.0).to_raw());
    assert_eq!(Hz::new(440.0).get(), 440.0);
    assert_eq!(Db::new(-6.0).get(), -6.0);
    assert_eq!(Samples::new(512).get(), 512);
}

#[test]
fn from_is_the_spelling_for_single_answer_conversions() {
    // Each of these delegates to its inherent method, so the two spellings
    // cannot drift.
    assert_eq!(
        Semitones::from(Cents::new(150.0)),
        Cents::new(150.0).to_semitones()
    );
    assert_eq!(
        Cents::from(Semitones::new(1.5)),
        Semitones::new(1.5).to_cents()
    );
    assert_eq!(
        Radians::from(Phase::new(0.25)),
        Phase::new(0.25).to_radians()
    );
    assert_eq!(
        StereoWidth::from(Correlation::new(0.25)),
        Correlation::new(0.25).to_stereo_width()
    );
    assert_eq!(
        Db::from(Amplitude::new(0.5)),
        Db::from_amplitude(Amplitude::new(0.5))
    );
    assert_eq!(Amplitude::from(Db::new(-6.0)), Db::new(-6.0).to_amplitude());

    // And they round-trip where the pair is mutually inverse.
    assert_eq!(
        Cents::from(Semitones::from(Cents::new(1200.0))),
        Cents::new(1200.0)
    );
}

/// The `From`/named-method split, as a ledger — the same discipline the
/// operator omissions in the family tests get, and for the same reason: a split nobody
/// wrote down drifts unexamined.
#[test]
fn conversions_that_stay_named_and_why() {
    // Needs a second input, so it is not a two-type conversion at all.
    assert_eq!(
        Seconds::new(1.0).to_samples(SampleRate::SR_48K),
        Samples::new(48_000)
    );

    // What stays named is the case where the second input is real. A
    // policy knob hiding inside a conversion is not that case — it is
    // relocated to the consumer instead, which is what lets
    // `Amplitude` <-> `Db` be a `From`. See the test below.

    // Target is a bare scalar, and two different units convert into it —
    // as `From` impls these would be two meanings of one target type.
    assert!(Cents::new(1200.0).to_pitch_ratio() == 2.0);
    assert!(Semitones::new(12.0).to_pitch_ratio() == 2.0);

    // `ArcDegrees` has no radians conversion at all. `Azimuth` and
    // `Elevation` do (as `From`), so adding one here would look like
    // filling a gap — but nothing needs it, and an unused conversion
    // between a displacement and an angle invites treating one as the
    // other. Left absent deliberately.
}

#[test]
fn clamp_stays_in_the_unit_type() {
    assert_eq!(
        Hz::new(20_000.0).clamp(Hz::new(20.0), Hz::new(18_000.0)),
        Hz::new(18_000.0)
    );
    assert_eq!(Mix::new(1.5).clamp(Mix::DRY, Mix::WET), Mix::WET);
}

/// The module's one non-finite rule, for every `new_clamped`: NaN stays
/// NaN, an infinity lands on the bound on its side, and an unbounded side
/// stays infinite.
#[test]
fn every_new_clamped_passes_nan_and_saturates_infinity() {
    // Each unit is checked at its own width.
    macro_rules! check {
        ($t:ident, $lo:expr, $hi:expr) => {
            check!($t, $lo, $hi, f32)
        };
        ($t:ident, $lo:expr, $hi:expr, $raw:ident) => {
            assert!($t::new_clamped($raw::NAN).get().is_nan(), stringify!($t));
            assert_eq!($t::new_clamped($raw::INFINITY).get(), $hi, stringify!($t));
            assert_eq!(
                $t::new_clamped($raw::NEG_INFINITY).get(),
                $lo,
                stringify!($t)
            );
        };
    }
    check!(Mix, 0.0, 1.0, f64);
    check!(Feedback, 0.0, 0.99, f64);
    check!(Spread, 0.0, 1.0, f64);
    check!(Resonance, 0.0, 1.0, f64);
    check!(Confidence, 0.0, 1.0, f64);
    check!(Correlation, -1.0, 1.0, f64);
    check!(Pan, -1.0, 1.0, f64);
    check!(Balance, -1.0, 1.0, f64);
    check!(StereoBalance, -1.0, 1.0, f64);
    check!(Velocity, 0.0, 1.0, f64);
    check!(Elevation, -90.0, 90.0, f64);
    check!(StretchFactor, 0.25, 4.0, f64);
    // The unbounded side stays infinite.
    check!(StereoWidth, 0.0, f64::INFINITY, f64);
    check!(CompressionRatio, 1.0, f64::INFINITY, f64);
    check!(Q, f64::MIN_POSITIVE, f64::INFINITY, f64);
    check!(BitDepth, 1.0, 32.0, f64);
    check!(HoldFactor, 1.0, f64::INFINITY, f64);
    // In-range values pass untouched, and -0.0 keeps its sign bit.
    assert_eq!(StereoWidth::new_clamped(1.5), StereoWidth::new(1.5));
    assert_eq!(
        CompressionRatio::new_clamped(4.0),
        CompressionRatio::new(4.0)
    );
    assert_eq!(Q::new_clamped(0.7), Q::new(0.7));
    assert_eq!(
        StereoWidth::new_clamped(-0.0).get().to_bits(),
        (-0.0f64).to_bits()
    );
}

/// Tiny negatives are within rounding of a whole cycle below zero, and the
/// documented half-open range holds for them.
#[test]
fn the_wraps_hold_their_half_open_range_at_the_seam() {
    let below_one = 1.0_f64.next_down();
    for v in [
        -1e-20_f64,
        -1e-300,
        -f64::MIN_POSITIVE,
        -2.7e-17,
        -5.5e-17,
        -1.1e-16,
    ] {
        let p = Phase::wrapped(v);
        assert!((0.0..1.0).contains(&p.get()), "wrapped({v:e}) = {p}");
        assert_eq!(p, Phase::new(below_one), "{v:e}");
    }
    // Ordinary inputs keep their exact figures.
    assert_eq!(Phase::wrapped(-0.25), Phase::new(0.75));
    assert_eq!(Phase::wrapped(0.0), Phase::new(0.0));
    assert_eq!(Phase::wrapped(1.0), Phase::new(0.0));
    assert_eq!(Phase::wrapped(2.0), Phase::new(0.0));
    assert_eq!(Phase::wrapped(-1.0), Phase::new(0.0));
    // NaN and the infinities give NaN.
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(Phase::wrapped(v).get().is_nan());
    }

    let top = 360.0_f64.next_down() - 180.0;
    for v in [
        (-180.0_f64).next_down(),
        -180.000_000_000_000_06,
        -180.000_000_000_000_1,
    ] {
        let a = Azimuth::new(v).wrap().get();
        assert!((-180.0..180.0).contains(&a), "{v} wrapped to {a}");
    }
    assert_eq!(
        Azimuth::new((-180.0_f64).next_down()).wrap(),
        Azimuth::new(top)
    );
    assert_eq!(Azimuth::new(190.0).wrap(), Azimuth::new(-170.0));
    assert_eq!(Azimuth::new(-190.0).wrap(), Azimuth::new(170.0));
    assert_eq!(Azimuth::new(180.0).wrap(), Azimuth::new(-180.0));
    assert_eq!(Azimuth::new(-180.0).wrap(), Azimuth::new(-180.0));
    assert!(Azimuth::new(f64::NAN).wrap().get().is_nan());
    assert!(Azimuth::new(f64::INFINITY).wrap().get().is_nan());

    assert!(Radians::new(f64::NAN).wrapped_signed().get().is_nan());
    assert!(Radians::new(f64::INFINITY).wrapped_signed().get().is_nan());

    let below_one = 1.0_f64.next_down();
    assert_eq!(Beat::new(-1e-20).fract(), BeatDuration::new(below_one));
    assert_eq!(SamplePosition::new(-1e-20).fract(), below_one);
    assert_eq!(Beat::new(2.25).fract(), BeatDuration::new(0.25));
    assert_eq!(Beat::new(-0.75).fract(), BeatDuration::new(0.25));
    assert_eq!(Beat::new(3.0).fract(), BeatDuration::new(0.0));
    assert!(Beat::new(f64::NAN).fract().get().is_nan());
    assert!(Beat::new(f64::INFINITY).fract().get().is_nan());

    let four = BeatDuration::new(4.0);
    assert_eq!(
        BeatDuration::new(-1e-20).rem_euclid(four),
        BeatDuration::new(4.0_f64.next_down())
    );
    assert_eq!(
        BeatDuration::new(-1.0).rem_euclid(four),
        BeatDuration::new(3.0)
    );
    assert_eq!(
        BeatDuration::new(9.0).rem_euclid(four),
        BeatDuration::new(1.0)
    );
    assert_eq!(
        BeatDuration::new(-1e-20).rem_euclid(BeatDuration::new(-4.0)),
        BeatDuration::new(4.0_f64.next_down())
    );
    assert!(
        BeatDuration::new(1.0)
            .rem_euclid(BeatDuration::new(0.0))
            .get()
            .is_nan()
    );
}

/// **A float unit serializes and deserializes refusing NaN, keeping an infinity and a finite raw
/// exactly.**
/// Read through serde's own `f64` deserializer, which carries every IEEE value (JSON
/// carries no NaN).
#[cfg(feature = "serde")]
#[test]
fn a_float_unit_serializes_and_deserializes_refusing_nan_and_keeping_an_infinity() {
    use serde::Deserialize;
    use serde::de::IntoDeserializer;
    use serde::de::value::{Error, F64Deserializer};
    let read = |raw: f64| Db::deserialize::<F64Deserializer<Error>>(raw.into_deserializer());
    assert!(read(f64::NAN).is_err());
    assert!(serde_json::to_string(&Db::new(f64::NAN)).is_err());
    assert_eq!(read(f64::NEG_INFINITY).unwrap().get(), f64::NEG_INFINITY);
    assert_eq!(read(-6.0).unwrap(), Db::new(-6.0));
    assert_eq!(
        serde_json::from_str::<Seconds>(&serde_json::to_string(&Seconds::new(1.5)).unwrap())
            .unwrap(),
        Seconds::new(1.5)
    );
}
