//! The envelope's figures: stage lengths in frames, the dB-linear fall, the closed form against
//! the per-frame increments, and the start levels of a release and a retrigger. Each is a
//! closed form written out here from the stage's authored time.

use super::*;
use tutti_units::{Amplitude, Db, Normalized, SampleRate, Samples, Seconds};

const RATE: f64 = 48_000.0;

fn rate() -> SampleRate {
    SampleRate::of(RATE)
}

fn frames(n: usize) -> Samples {
    Samples::new(n)
}

fn secs(frames: f64) -> Seconds {
    Seconds::new(frames / RATE)
}

/// Delay 240, attack 480, hold 192, decay 960, release 1440 frames; sustain 0.5.
fn stages() -> EnvelopeStages {
    EnvelopeStages::new(secs(480.0), secs(960.0), Normalized::of(0.5), secs(1440.0))
        .delay(secs(240.0))
        .hold(secs(192.0))
}

fn runner(stages: &EnvelopeStages) -> Envelope {
    let mut envelope = Envelope::new(EnvelopeDesign::new(stages, rate()));
    envelope.start(EnvelopeDesign::new(stages, rate()));
    envelope
}

/// The frame count at which the envelope first reads `stage`, stepping one frame at a time
/// from where it is.
fn frame_of(envelope: &mut Envelope, stage: EnvelopeStage) -> usize {
    (0..1_000_000)
        .find(|_| {
            envelope.advance(frames(1));
            envelope.stage() == stage
        })
        .expect("the stage is reached")
        + 1
}

fn db_of(level: Amplitude) -> f64 {
    Db::from_amplitude(level).get()
}

/// A dB-linear curve of `depth` decibels.
fn db(depth: f64) -> EnvelopeCurve {
    EnvelopeCurve::db_linear(Db::new(depth)).expect("a depth above zero")
}

fn ceil(x: f64) -> usize {
    x.ceil() as usize
}

/// Delay, attack and hold take their authored frames, and a linear decay at the stage pace
/// takes its own, whatever the sustain.
#[test]
fn a_stage_takes_its_authored_frames_at_the_stage_pace_for_both_curves() {
    for curve in [EnvelopeCurve::LINEAR, db(60.0)] {
        let stages = stages().decay_curve(curve).pace(EnvelopePace::Stage);
        let mut envelope = runner(&stages);
        assert_eq!(frame_of(&mut envelope, EnvelopeStage::Attack), 240);
        assert_eq!(frame_of(&mut envelope, EnvelopeStage::Hold), 480);
        assert_eq!(frame_of(&mut envelope, EnvelopeStage::Decay), 192);
        assert_eq!(frame_of(&mut envelope, EnvelopeStage::Sustain), 960);
        assert_eq!(envelope.level(), Amplitude::new(0.5));
        envelope.release();
        assert_eq!(frame_of(&mut envelope, EnvelopeStage::Idle), 1440);
        assert_eq!(envelope.level(), Amplitude::SILENT);
    }
}

/// At the full-scale pace a fall to a sustain takes the share of the scale it covers: for a
/// line `(1 - s) D` frames, for a dB line `20 log10(1/s) / depth * D`; a release from `a`
/// takes `a R` frames, or `20 log10(a / floor) / depth * R`.
#[test]
fn a_full_scale_fall_takes_the_share_of_the_scale_it_covers() {
    let depth = 60.0;
    let sustain = 0.5_f64;
    let linear = stages();
    let mut envelope = runner(&linear);
    frame_of(&mut envelope, EnvelopeStage::Decay);
    assert_eq!(
        frame_of(&mut envelope, EnvelopeStage::Sustain),
        ceil((1.0 - sustain) * 960.0)
    );

    let curved = stages().decay_curve(db(depth)).release_curve(db(depth));
    let mut envelope = runner(&curved);
    frame_of(&mut envelope, EnvelopeStage::Decay);
    assert_eq!(
        frame_of(&mut envelope, EnvelopeStage::Sustain),
        ceil(20.0 * (1.0 / sustain).log10() / depth * 960.0)
    );
    // From the sustain: `a = 0.5`, down to the floor `10^(-depth / 20)`.
    envelope.release();
    let floor = 10.0_f64.powf(-depth / 20.0);
    assert_eq!(
        frame_of(&mut envelope, EnvelopeStage::Idle),
        ceil(20.0 * (sustain / floor).log10() / depth * 1440.0)
    );
}

/// A dB-linear decay falls its depth over its time, and is a straight line in decibels: the
/// level `n` frames in is `-depth n / D` dB, to the rounding of `exp` and `log10`.
#[test]
fn a_db_linear_segment_falls_its_depth_over_its_time_in_a_straight_line() {
    let depth = 100.0;
    let decay = 4800.0;
    let stages = EnvelopeStages::new(secs(0.0), secs(decay), Normalized::of(0.0), secs(1.0))
        .decay_curve(db(depth));
    let design = EnvelopeDesign::new(&stages, rate());
    let leg = design.enter(EnvelopeStage::Decay, Amplitude::UNITY);
    assert!((leg.duration(rate()).get() * RATE - decay).abs() < 1e-9);
    for n in [1_usize, 480, 1200, 2400, 4799] {
        let fell = -db_of(leg.level_at(frames(n)));
        let want = depth * n as f64 / decay;
        assert!(
            (fell - want).abs() < 1e-11,
            "{n} frames: {fell} dB, want {want}"
        );
    }
    // The whole time is the whole depth.
    assert!((-db_of(leg.to()) - depth).abs() < 1e-11);
}

/// The closed form `level_at(n)` is the per-frame increments of `step` applied `n` times, to
/// the rounding of the recurrence: each add (or multiply) in the float errs by half an epsilon
/// of the level, at most 1, so `n` of them and the increment's own rounding stay within
/// `(n + 1) epsilon` (absolute for a line, relative for a dB line).
fn recurrence_matches<F: Float + Into<f64>>(epsilon: f64) {
    let stages = stages()
        .decay_curve(db(80.0))
        .release_curve(EnvelopeCurve::LINEAR);
    let design = EnvelopeDesign::new(&stages, rate());
    for (stage, start) in [
        (EnvelopeStage::Attack, 0.0),
        (EnvelopeStage::Decay, 1.0),
        (EnvelopeStage::Release, 0.75),
    ] {
        let leg = design.enter(stage, Amplitude::new(start));
        let mut level = F::from_design(leg.from().get());
        let n = (leg.duration(rate()).get() * RATE).floor() as usize;
        for k in 1..=n {
            level = match leg.step::<F>() {
                EnvelopeStep::Add(s) => level + s,
                EnvelopeStep::Mul(g) => level * g,
                EnvelopeStep::Hold => level,
            };
            let want = leg.level_at(frames(k)).get();
            let got: f64 = level.into();
            let bound = (k as f64 + 1.0) * epsilon * want.abs().max(1.0);
            assert!(
                (got - want).abs() <= bound,
                "{stage:?} frame {k}: {got} against {want}"
            );
        }
    }
}

#[test]
fn the_closed_form_is_the_recurrence_within_its_bound_at_f64() {
    recurrence_matches::<f64>(f64::EPSILON);
}

#[test]
fn the_closed_form_is_the_recurrence_within_its_bound_at_f32() {
    recurrence_matches::<f32>(f64::from(f32::EPSILON));
}

/// A release begun mid-attack falls from where the attack had got to: `150 / 480`. At the stage
/// pace it takes the authored 1440 frames from there, and after one frame is a 1440th down.
/// A retrigger from the mid-release level attacks from it and takes the rest of the scale.
#[test]
fn a_release_and_a_retrigger_start_from_the_current_level() {
    let stages = stages().pace(EnvelopePace::Stage);
    let mut envelope = runner(&stages);
    envelope.advance(frames(240 + 150));
    let level = 150.0 / 480.0;
    assert!((envelope.level().get() - level).abs() < 1e-15);
    envelope.release();
    assert!((envelope.level().get() - level).abs() < 1e-15);
    envelope.advance(frames(1));
    assert!((envelope.level().get() - level * (1.0 - 1.0 / 1440.0)).abs() < 1e-15);
    envelope.advance(frames(719));
    let mid = envelope.level().get();
    assert!((mid - level / 2.0).abs() < 1e-15);
    envelope.retrigger();
    assert_eq!(envelope.stage(), EnvelopeStage::Attack);
    assert!((envelope.level().get() - mid).abs() < 1e-15);
    // At the full-scale pace of the attack, the rest of the scale takes `(1 - a) 480` frames.
    assert_eq!(
        frame_of(&mut envelope, EnvelopeStage::Hold),
        ceil((1.0 - mid) * 480.0)
    );
}

/// A quick release of `T` frames ends within them whatever the level it starts from, and falls
/// the full depth over exactly `T`: half of `T` on, `depth / 2` down from where it began.
#[test]
fn a_quick_release_ends_within_its_time() {
    let stages = EnvelopeStages::new(
        secs(48.0),
        secs(4800.0),
        Normalized::of(0.8),
        secs(240_000.0),
    )
    .release_curve(db(100.0));
    let mut envelope = runner(&stages);
    envelope.advance(frames(48 + 10_000));
    let before = db_of(envelope.level());
    envelope.release_within(secs(480.0));
    envelope.release();
    envelope.advance(frames(240));
    assert!((before - db_of(envelope.level()) - 50.0).abs() < 1e-11);
    envelope.advance(frames(240));
    assert!(envelope.is_idle());
    assert_eq!(envelope.level(), Amplitude::SILENT);
}

/// A running release made quick continues from its level and a release already shorter than
/// the cap is not slowed.
#[test]
fn a_quick_release_never_slows_a_shorter_release() {
    let stages = EnvelopeStages::new(secs(0.0), secs(0.0), Normalized::of(1.0), secs(100.0))
        .pace(EnvelopePace::Stage);
    let mut envelope = runner(&stages);
    envelope.release();
    envelope.release_within(secs(480.0));
    assert_eq!(frame_of(&mut envelope, EnvelopeStage::Idle), 100);
}

/// A stage of no frames takes none: a zero attack is already at its end, unity, at frame 0 and
/// the envelope is in its decay, and a zero release is silent at once.
#[test]
fn a_zero_length_stage_takes_no_frame() {
    let stages = EnvelopeStages::new(secs(0.0), secs(960.0), Normalized::of(0.6), secs(0.0));
    let mut envelope = runner(&stages);
    assert_eq!(envelope.stage(), EnvelopeStage::Decay);
    assert_eq!(envelope.level(), Amplitude::UNITY);
    envelope.release();
    assert!(envelope.is_idle());
    assert_eq!(envelope.level(), Amplitude::SILENT);
}

/// A depth that is not above zero, or not finite, is no fall: it makes no curve.
#[test]
fn a_depth_that_is_no_fall_makes_no_curve() {
    for depth in [0.0, -60.0, f64::NAN, f64::INFINITY] {
        assert_eq!(EnvelopeCurve::db_linear(Db::new(depth)), None, "{depth}");
    }
    assert!(EnvelopeCurve::db_linear(Db::new(60.0)).is_some());
}

/// The level after `N` frames is the same read in one chunk, in frames of one, or in chunks
/// that straddle stage ends.
#[test]
fn any_chunking_of_the_same_frames_reads_the_same_level() {
    let stages = stages().decay_curve(db(70.0));
    for total in [1, 239, 240, 241, 719, 720, 912, 1500, 3000] {
        let mut whole = runner(&stages);
        whole.advance(frames(total));
        let mut ones = runner(&stages);
        (0..total).for_each(|_| ones.advance(frames(1)));
        let mut chunks = runner(&stages);
        let mut left = total;
        while left > 0 {
            let take = left.min(37);
            chunks.advance(frames(take));
            left -= take;
        }
        assert_eq!(whole.stage(), ones.stage());
        assert_eq!(whole.stage(), chunks.stage());
        assert!((whole.level().get() - ones.level().get()).abs() < 1e-15);
        assert!((whole.level().get() - chunks.level().get()).abs() < 1e-15);
    }
}

/// A time under a trillionth of a frame lasts none, as a zero does: the levels stay finite
/// where a slope over so short a time would not be.
#[test]
fn a_time_under_a_trillionth_of_a_frame_is_a_zero_length_stage() {
    let tiny = Seconds::new(1e-30);
    let stages = EnvelopeStages::new(tiny, tiny, Normalized::of(0.6), tiny);
    let mut envelope = runner(&stages);
    assert_eq!(envelope.stage(), EnvelopeStage::Sustain);
    assert_eq!(envelope.level(), Amplitude::new(0.6));
    envelope.release();
    assert!(envelope.is_idle());
    assert_eq!(envelope.level(), Amplitude::SILENT);
}

/// **An `Adsr` sustain outside `0..=1` is pinned onto it, a NaN is silence**: the envelope
/// of a sustain of 1.5 holds at unity, of -0.5 and NaN at zero.
#[test]
fn an_adsr_sustain_outside_the_unit_range_is_pinned_onto_it() {
    for (sustain, held) in [
        (1.5, Amplitude::UNITY),
        (-0.5, Amplitude::SILENT),
        (f64::NAN, Amplitude::SILENT),
        (0.25, Amplitude::new(0.25)),
    ] {
        let adsr = tutti_music::Adsr::new(secs(0.0), secs(0.0), Amplitude::new(sustain), secs(1.0));
        let stages = EnvelopeStages::from_adsr_clamped(adsr);
        let envelope = runner(&stages);
        assert_eq!(envelope.level(), held, "sustain {sustain}");
    }
}
