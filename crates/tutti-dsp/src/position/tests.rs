use super::*;

fn at(count: u64) -> ReadPosition {
    ReadPosition::from_count(count)
}

#[test]
fn a_position_glide_lands_on_the_target_and_reads_the_closed_form_between() {
    let (from, to) = (at(10_u64 << 32), at((30_u64 << 32) + 3));
    let mut glide = Glide::landed(from);
    glide.aim_in(to, 8);
    assert_eq!(glide.at(0), from);
    assert_eq!(glide.at(8), to);
    // 20 frames and 3 quanta over 8 steps: step 4 is the halfway point, truncated.
    assert_eq!(glide.at(4).get(), (20_u64 << 32) + 1);
    assert_eq!(glide.at(2).get(), 15_u64 << 32);
}

#[test]
fn a_position_glide_aimed_by_rate_takes_the_distance_over_the_rate_in_steps() {
    let mut glide = Glide::landed(at(100_u64 << 32));
    // 20 frames at no more than 0.5 a step: 40 steps.
    glide.aim_position(at(120_u64 << 32), Steps::Rate(0.5));
    assert_eq!(glide.at(20).get(), 110_u64 << 32);
    assert!(glide.at(39).get() < (120_u64 << 32));
    assert_eq!(glide.at(40).get(), 120_u64 << 32);
    // A glide runs backwards too.
    let mut back = Glide::landed(at(100_u64 << 32));
    back.aim_position(at(90_u64 << 32), Steps::Fixed(10));
    assert_eq!(back.at(5).get(), 95_u64 << 32);
}

#[test]
fn a_glide_step_past_the_end_reads_the_far_end_in_either_direction() {
    let (a, b) = (at(100), at(900));
    assert_eq!(a.lerp(&b, 50, 8), b);
    assert_eq!(b.lerp(&a, 50, 8), a);
}
