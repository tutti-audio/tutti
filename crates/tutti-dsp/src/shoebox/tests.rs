use super::*;
use crate::EarlyReflections;
use tutti_buffers::{Planes, PlanesMut};
use tutti_units::Speed;

const RATE: SampleRate = SampleRate::SR_48K;
const C: f64 = Speed::SOUND_IN_AIR_20C.get();

fn m(x: f64) -> Metres {
    Metres::of(x)
}

fn alpha(x: f64) -> Normalized {
    Normalized::of(x)
}

/// A 10 x 8 x 6 m room, the source and the listener at the same point (3, 4, 2): the walls are
/// 3 and 7 m, 4 and 4 m, 2 and 4 m away, and every wall absorbs differently.
fn room() -> ShoeboxConfig {
    ShoeboxConfig::new(
        [m(10.0), m(8.0), m(6.0)],
        [m(3.0), m(4.0), m(2.0)],
        [m(3.0), m(4.0), m(2.0)],
        alpha(0.0),
    )
    .wall(Wall::LowX, alpha(0.1))
    .wall(Wall::HighX, alpha(0.2))
    .wall(Wall::LowY, alpha(0.3))
    .wall(Wall::HighY, alpha(0.4))
    .wall(Wall::LowZ, alpha(0.5))
    .wall(Wall::HighZ, alpha(0.6))
}

/// The taps of the room `config` makes, at the test rate.
fn taps_of(config: ShoeboxConfig) -> Taps {
    ShoeboxRoom::try_from(config).unwrap().taps(RATE)
}

/// `(delay, gain)` of every tap, sorted.
fn pairs(taps: &Taps) -> Vec<(f64, f64)> {
    let mut v: Vec<(f64, f64)> = taps
        .taps
        .iter()
        .map(|t| (t.delay.get(), t.gain.get()))
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    v
}

/// The frames a path of `metres` takes.
fn frames(metres: f64) -> f64 {
    metres / C * RATE.get()
}

/// **With the source at the listener the six first images are the wall distances doubled**:
/// each tap is at `2 d / c` of delay and `(1 - alpha) / (2 d)` of gain, `d` the distance to
/// that wall, and the direct path is not a tap.
#[test]
fn the_first_images_are_at_twice_the_wall_distances_with_gain_one_minus_alpha_over_2d() {
    // (the wall's distance, its alpha), in the order of `Wall`.
    let walls = [
        (3.0, 0.1),
        (7.0, 0.2),
        (4.0, 0.3),
        (4.0, 0.4),
        (2.0, 0.5),
        (4.0, 0.6),
    ];
    let mut want: Vec<(f64, f64)> = walls
        .iter()
        .map(|&(d, a)| (frames(2.0 * d), (1.0 - a) / (2.0 * d)))
        .collect();
    want.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    let got = pairs(&taps_of(room()));
    assert_eq!(got.len(), 6);
    for ((gd, gg), (wd, wg)) in got.iter().zip(&want) {
        assert!((gd - wd).abs() <= 1e-9 * wd, "delay {gd} for {wd}");
        assert!((gg - wg).abs() <= 1e-12, "gain {gg} for {wg}");
    }
}

/// The images with `k` reflections, from the closed form: an image has `r_x + r_y + r_z = k`
/// reflections, and an axis has one image with none and two with `r >= 1`.
fn images_of_order(k: usize) -> usize {
    let per_axis = |r: usize| if r == 0 { 1 } else { 2 };
    let mut count = 0;
    for rx in 0..=k {
        for ry in 0..=k - rx {
            count += per_axis(rx) * per_axis(ry) * per_axis(k - rx - ry);
        }
    }
    count
}

/// **The image count follows the closed form**: 6 at first order, 6 + 18 at second, and one
/// more with the direct path.
#[test]
fn the_image_count_is_the_closed_form() {
    assert_eq!(images_of_order(1), 6);
    assert_eq!(images_of_order(2), 18);
    let count = |c: ShoeboxConfig| taps_of(c).taps.len();
    assert_eq!(count(room()), images_of_order(1));
    let second = room().order(ReflectionOrder::Second);
    assert_eq!(count(second), images_of_order(1) + images_of_order(2));
    assert_eq!(
        count(second.direct(true)),
        1 + images_of_order(1) + images_of_order(2)
    );
}

/// **The taps are sorted by delay**, at both orders.
#[test]
fn the_delays_are_sorted() {
    for order in [ReflectionOrder::First, ReflectionOrder::Second] {
        let taps = taps_of(room().order(order).direct(true));
        let delays: Vec<f64> = taps.taps.iter().map(|t| t.delay.get()).collect();
        assert!(delays.windows(2).all(|w| w[0] <= w[1]), "{delays:?}");
    }
}

/// **Two reflections multiply the walls' reflectivities and add their path**: the image two
/// walls of one axis away is `2 L` off, and gains `(1 - alpha_low)(1 - alpha_high) / (2 L)`;
/// the image in an x wall and a y wall is `2 sqrt(d_x^2 + d_y^2)` off with those two walls'
/// reflectivities.
#[test]
fn second_order_images_multiply_the_reflectivities_of_their_walls() {
    let got = pairs(&taps_of(room().order(ReflectionOrder::Second)));
    let has = |metres: f64, gain: f64| {
        got.iter()
            .any(|&(d, g)| (d - frames(metres)).abs() <= 1e-9 * d && (g - gain).abs() <= 1e-12)
    };
    // Both x walls: at `x + 2 L` (the listener is at `x`).
    assert!(has(20.0, 0.9 * 0.8 / 20.0));
    // The low x wall and the low y wall: the source at (-3, -4).
    let across = 2.0 * (3.0f64 * 3.0 + 4.0 * 4.0).sqrt();
    assert!(has(across, 0.9 * 0.7 / across));
    // The high x wall and the high z wall: (7 m and 4 m away).
    let across = 2.0 * (7.0f64 * 7.0 + 4.0 * 4.0).sqrt();
    assert!(has(across, 0.8 * 0.4 / across));
}

/// **The direct path is a tap of gain `1 / r` at `r / c`** once asked for, at 3 m (a listener
/// 3 m from the source), and a path under the 1 m reference has gain 1 and not more.
#[test]
fn the_direct_path_is_a_tap_when_asked_and_a_near_one_is_not_louder() {
    let room = |listener: f64| {
        ShoeboxConfig::new(
            [m(10.0), m(8.0), m(6.0)],
            [m(2.0), m(4.0), m(3.0)],
            [m(listener), m(4.0), m(3.0)],
            alpha(0.5),
        )
    };
    let direct = |r: ShoeboxConfig| {
        let all = pairs(&taps_of(r.direct(true)));
        let without = pairs(&taps_of(r));
        assert_eq!(all.len(), without.len() + 1);
        *all.iter().find(|t| !without.contains(t)).unwrap()
    };
    let (d, g) = direct(room(5.0));
    assert!((d - frames(3.0)).abs() <= 1e-9 * d);
    assert!((g - 1.0 / 3.0).abs() <= 1e-12);
    let (_, g) = direct(room(2.5));
    assert_eq!(g, 1.0);
}

/// **The room's limits refuse a source or a listener outside it**, and a room with no extent;
/// a position on a wall is inside.
#[test]
fn a_source_or_listener_outside_the_room_is_refused() {
    let size = [m(10.0), m(8.0), m(6.0)];
    let inside = [m(1.0), m(1.0), m(1.0)];
    let new = |source, listener| {
        ShoeboxRoom::try_from(ShoeboxConfig::new(size, source, listener, alpha(0.5)))
    };
    assert_eq!(
        new([m(10.5), m(1.0), m(1.0)], inside).err(),
        Some(ShoeboxError::Source { axis: 0 })
    );
    assert_eq!(
        new(inside, [m(1.0), m(8.5), m(1.0)]).err(),
        Some(ShoeboxError::Listener { axis: 1 })
    );
    assert_eq!(
        new([m(1.0), m(1.0), m(6.5)], inside).err(),
        Some(ShoeboxError::Source { axis: 2 })
    );
    assert!(new([m(10.0), m(8.0), m(6.0)], [m(0.0), m(0.0), m(0.0)]).is_ok());
    assert_eq!(
        ShoeboxRoom::try_from(ShoeboxConfig::new(
            [m(10.0), m(0.0), m(6.0)],
            inside,
            inside,
            alpha(0.5)
        ))
        .err(),
        Some(ShoeboxError::Empty { axis: 1 })
    );
}

/// **The taps run through the early-reflections stage**: linear reads keep each gain whole
/// across the two frames it falls between, so the impulse response sums to the sum of the
/// gains.
#[test]
fn the_room_taps_realize_as_a_stage_whose_response_sums_to_the_gains() {
    let taps = taps_of(room().order(ReflectionOrder::Second));
    let total: f64 = taps.taps.iter().map(|t| t.gain.get()).sum();
    let last = taps.taps.iter().map(|t| t.delay.get()).fold(0.0, f64::max);
    let mut stage = EarlyReflections::<f64>::try_from(&taps).unwrap();
    let frames = last.ceil() as usize + 2;
    let mut x = vec![0.0; frames];
    x[0] = 1.0;
    let (input, mut output) = ([x], vec![vec![0.0; frames]; stage.channels()]);
    stage.process(
        Planes::from_vecs(&input).unwrap(),
        PlanesMut::from_vecs(&mut output).unwrap(),
    );
    let sum: f64 = output[0].iter().sum();
    assert!((sum - total).abs() <= 1e-12, "{sum} for {total}");
}

/// The speed moves the delays and leaves the gains: at half the speed every delay doubles.
#[test]
fn half_the_speed_doubles_the_delays_and_keeps_the_gains() {
    let slow = room().speed(Speed::new(C / 2.0).unwrap());
    let (a, b) = (pairs(&taps_of(room())), pairs(&taps_of(slow)));
    for ((da, ga), (db, gb)) in a.iter().zip(&b) {
        assert!((db - 2.0 * da).abs() <= 1e-9 * db);
        assert_eq!(ga, gb);
    }
}
