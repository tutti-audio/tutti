use super::*;

/// A ramp by a rate takes `ceil(distance / rate)` steps from where it is, lands on the
/// target exactly, and is not restarted by a `retarget` at the target it already has;
/// `Fixed(0)` lands at once.
#[test]
fn a_rate_is_a_number_of_steps_from_the_distance() {
    let count = |g: &mut Ramp<f32>| {
        (0..100)
            .take_while(|_| {
                let moved = g.moving() && g.from != g.to;
                g.advance(1);
                moved
            })
            .count()
    };
    let mut g = Ramp::new(0.0f32);
    g.retarget_by(2.5, Steps::Rate(1.0));
    assert_eq!(count(&mut g), 3);
    assert_eq!(g.now(), 2.5);
    // The same target again, mid-flight, leaves the ramp where it was: four of its
    // five steps remain.
    g.retarget_by(0.0, Steps::Fixed(5));
    g.advance(1);
    g.retarget_by(0.0, Steps::Fixed(5));
    assert_eq!(count(&mut g), 4);
    g.retarget_by(1.5, Steps::Fixed(0));
    assert_eq!((g.now(), g.moving()), (1.5, false));
    // An exact multiple takes exactly that many steps.
    let mut h = Ramp::new(1.0f32);
    h.retarget_by(5.0, Steps::Rate(2.0));
    assert_eq!(count(&mut h), 2);
}

/// `at(k)` is the closed form `from + (to - from) * k / n` of the definition at every
/// step, the target itself from step `n` on, and the same value whether it is read
/// directly or after stepping there.
#[test]
fn a_step_reads_the_closed_form_and_lands_on_the_target() {
    let mut g = Ramp::new(2.0f32).steps(8);
    g.retarget(10.0);
    for k in 0..8u32 {
        let want = 2.0 + 8.0 * (k as f32 / 8.0);
        assert_eq!(g.at(k), want, "step {k}");
    }
    assert_eq!(g.at(4), 6.0, "half way from 2 to 10");
    assert_eq!(g.at(8).to_bits(), 10.0f32.to_bits());
    assert_eq!(g.at(1_000), 10.0);
    let direct = g.at(5);
    g.advance(5);
    assert_eq!(g.now(), direct);
    g.advance(100);
    assert_eq!((g.steps_taken(), g.now()), (8, 10.0));
}

/// A new target starts from the value in flight, not from the old start or target.
#[test]
fn a_retarget_mid_flight_starts_from_the_value_in_flight() {
    let mut g = Ramp::new(0.0f32).steps(4);
    g.retarget(4.0);
    g.advance(2);
    g.retarget(0.0);
    assert_eq!(g.at(0), 2.0);
    assert_eq!(g.at(2), 1.0);
}

/// Of a ramp of `n` steps with `k` taken, the next `n - k - 1` frames blend and the
/// frame of step `n` reads the target; a ramp that starts on its target blends none.
#[test]
fn the_blended_frames_stop_one_short_of_the_last_step() {
    let mut g = Ramp::new(0.0f32).steps(16);
    g.retarget(1.0);
    assert_eq!(g.blended_frames(64), 15);
    assert_eq!(g.blended_frames(4), 4);
    g.advance(10);
    assert_eq!(g.blended_frames(64), 5);
    g.advance(5);
    assert_eq!(g.blended_frames(64), 0);
    let mut same = Ramp::new(3.0f32).steps(16);
    same.retarget(3.0);
    assert_eq!(same.blended_frames(64), 0);
}
