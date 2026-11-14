use super::*;

/// A glide by a rate takes `ceil(distance / rate)` steps from where it is, lands on the
/// target exactly, and is not restarted by an aim at the target it already has;
/// `Fixed(0)` lands at once.
#[test]
fn a_rate_is_a_number_of_steps_from_the_distance() {
    let count = |g: &mut Glide<f32>| {
        (0..100)
            .take_while(|_| {
                let moved = g.moving() && g.from != g.to;
                g.advance(1);
                moved
            })
            .count()
    };
    let mut g = Glide::landed(0.0f32);
    g.aim_by(2.5, Steps::Rate(1.0));
    assert_eq!(count(&mut g), 3);
    assert_eq!(g.now(), 2.5);
    // The same target again, mid-flight, leaves the glide where it was: four of its
    // five steps remain.
    g.aim_by(0.0, Steps::Fixed(5));
    g.advance(1);
    g.aim_by(0.0, Steps::Fixed(5));
    assert_eq!(count(&mut g), 4);
    g.aim_by(1.5, Steps::Fixed(0));
    assert_eq!((g.now(), g.moving()), (1.5, false));
    // An exact multiple takes exactly that many steps.
    let mut h = Glide::landed(1.0f32);
    h.aim_by(5.0, Steps::Rate(2.0));
    assert_eq!(count(&mut h), 2);
}

/// `at(k)` is the closed form `from + (to - from) * k / n` of the definition at every
/// step, the target itself from step `n` on, and the same value whether it is read
/// directly or after stepping there.
#[test]
fn a_step_reads_the_closed_form_and_lands_on_the_target() {
    let mut g = Glide::landed(2.0f32).by(8);
    g.aim(10.0);
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
    assert_eq!((g.taken(), g.now()), (8, 10.0));
}

/// A new aim starts from the value in flight, not from the old start or target.
#[test]
fn an_aim_mid_flight_starts_from_the_value_in_flight() {
    let mut g = Glide::landed(0.0f32).by(4);
    g.aim(4.0);
    g.advance(2);
    g.aim(0.0);
    assert_eq!(g.at(0), 2.0);
    assert_eq!(g.at(2), 1.0);
}

/// Of a glide of `n` steps with `k` taken, the next `n - k - 1` frames blend and the
/// frame of step `n` reads the target; a glide that starts on its target blends none.
#[test]
fn the_blended_frames_stop_one_short_of_the_last_step() {
    let mut g = Glide::landed(0.0f32).by(16);
    g.aim(1.0);
    assert_eq!(g.blended(64), 15);
    assert_eq!(g.blended(4), 4);
    g.advance(10);
    assert_eq!(g.blended(64), 5);
    g.advance(5);
    assert_eq!(g.blended(64), 0);
    let mut same = Glide::landed(3.0f32).by(16);
    same.aim(3.0);
    assert_eq!(same.blended(64), 0);
}
