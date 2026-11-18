use tutti_types::{Amplitude, BusTopology, Gain, SampleRate, Samples, Seconds};

use super::*;
use crate::MaxDelayError;
use crate::delay::times::ChannelTimesError;

const MAX: MaxDelay = MaxDelay::of(Seconds(1.0));

fn bus(width: u16) -> BusTopology {
    BusTopology::assumed(width.into()).expect("a test width is one bus")
}

/// A stereo delay: the default swap routing, the channels linked in time.
fn stereo() -> DelayLayout {
    DelayLayout::new(BusTopology::STEREO, MAX, InterpolationMode::Linear)
}

/// `MaxDelay` keeps the range-checked shape: `new` refuses what is not a longest delay (not
/// finite, not above zero, past the cap: a finite 1e30 s among them) and never clamps.
#[test]
fn a_max_delay_keeps_the_range_checked_shape() {
    let c = tutti_fixtures::RangeContract {
        new: MaxDelay::new,
        get: MaxDelay::get,
        of: Some(MaxDelay::of),
        valid: &[Seconds(1e-9), Seconds(0.1), Seconds(2.0), MaxDelay::CAP],
        invalid: &[
            Seconds::ZERO,
            Seconds(-0.1),
            Seconds(10.000_001),
            Seconds(1e30),
            Seconds(f64::NAN),
            Seconds(f64::INFINITY),
            Seconds(f64::NEG_INFINITY),
        ],
    };
    tutti_fixtures::check_range_contract(&c);
    assert_eq!(
        MaxDelay::try_from(Seconds(-1.0)),
        Err(MaxDelayError(Seconds(-1.0)))
    );
}

/// A line holds `ceil(max * rate)` frames, from the exact product (0.1 s at 48 kHz is
/// 4800, where the `f32` nearest 0.1 would make 4801), and never more than the guard
/// against an absurd rate.
#[test]
fn a_max_delay_is_a_line_of_the_exact_ceiling_of_its_frames() {
    let max = MaxDelay::of(Seconds(0.1));
    assert_eq!(max.frames(SampleRate::SR_48K), Samples(4800));
    assert_eq!(max.frames(SampleRate::SR_44K1), Samples(4410));
    assert_eq!(
        MaxDelay::of(MaxDelay::CAP).frames(SampleRate::SR_192K),
        Samples(1_920_000)
    );
    assert_eq!(
        max.frames(SampleRate::of(1e15)),
        Samples(crate::delay::max_delay::MAX_FRAMES)
    );
}

/// `v` as signed gains.
fn gains(v: &[f32]) -> Vec<Gain> {
    v.iter().map(|&x| Gain::new(x)).collect()
}

/// A routing refuses what it cannot hold: a matrix of the wrong length, a weight that is
/// not finite (it would survive the scaling, `inf / inf` being NaN, and poison the loop
/// for good) and a row that sums past 1; and it keeps what it is given.
#[test]
fn a_routing_refuses_what_it_cannot_hold() {
    let two = BusTopology::STEREO;
    assert_eq!(
        Routing::new(two, &gains(&[0.0, 1.0, 1.0])),
        Err(RoutingError::Length {
            width: 2,
            expected: 4,
            got: 3
        })
    );
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let e = Routing::new(two, &gains(&[0.0, bad, 1.0, 0.0])).unwrap_err();
        assert!(
            matches!(
                e,
                RoutingError::NotFinite {
                    row: 0,
                    column: 1,
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(Routing::normalized(two, &gains(&[0.0, bad, 1.0, 0.0])).is_err());
    }
    assert_eq!(
        Routing::new(two, &gains(&[0.0, 1.0, 0.7, 0.4])),
        Err(RoutingError::Row {
            row: 1,
            sum: Amplitude(1.1)
        })
    );
    // A row that sums to exactly 1, and a negative weight, are held.
    let r = Routing::new(two, &gains(&[0.5, -0.5, 1.0, 0.0])).unwrap();
    assert_eq!(
        [
            r.weight(0, 0),
            r.weight(0, 1),
            r.weight(1, 0),
            r.weight(1, 1)
        ],
        [0.5, -0.5, 1.0, 0.0].map(Gain::new)
    );
    assert_eq!(
        r.weight(9, 9),
        Gain::SILENT,
        "a channel off the bus has no weight"
    );
}

/// `normalized` is the one saturation: a row past 1 is scaled down to sum to 1 with its
/// weights' ratios kept, and a row within 1 is left alone.
#[test]
fn a_normalized_routing_scales_a_row_past_one_and_no_other() {
    let four = bus(4);
    let r = Routing::normalized(four, &gains(&[1.0; 16])).unwrap();
    for row in 0..4 {
        let sum: f32 = (0..4).map(|c| r.weight(row, c).magnitude().get()).sum();
        assert!((sum - 1.0).abs() < 1e-6, "row {row}: {sum}");
        assert!((0..4).all(|c| r.weight(row, c) == Gain::new(0.25)));
    }
    let mut m = [0.0; 16];
    m[1] = 0.6;
    m[2] = -0.2;
    let r = Routing::normalized(four, &gains(&m)).unwrap();
    assert_eq!(
        (r.weight(0, 1), r.weight(0, 2)),
        (Gain::new(0.6), Gain::new(-0.2))
    );
}

/// The presets: the swap crosses left and right, a ring feeds each channel from the one
/// before it (the first from the last), none is silent, and a mono ring has nobody to feed
/// from.
#[test]
fn the_routing_presets_are_what_their_names_say() {
    let swap = Routing::swap();
    assert_eq!(
        [
            swap.weight(0, 1),
            swap.weight(1, 0),
            swap.weight(0, 0),
            swap.weight(1, 1)
        ],
        [1.0, 1.0, 0.0, 0.0].map(Gain::new)
    );
    let ring = Routing::ring(bus(4));
    for c in 0..4 {
        for j in 0..4 {
            assert_eq!(
                ring.weight(c, j),
                Gain::new(f32::from(u8::from(j == (c + 3) % 4))),
                "({c}, {j})"
            );
        }
    }
    assert!(Routing::none(bus(3)).is_none() && !ring.is_none());
    assert!(Routing::ring(BusTopology::MONO).is_none());
    assert_eq!(Routing::ring(BusTopology::STEREO), Routing::swap());
}

/// A routing is for one bus, and the layout says so: a routing for another width is
/// refused, and a layout's default is the swap at width 2 and none at any other.
#[test]
fn a_layout_takes_a_routing_for_its_own_bus_and_defaults_by_its_width() {
    assert_eq!(
        stereo().with_routing(Routing::ring(bus(3))).unwrap_err(),
        RoutingError::Width {
            routing: 3,
            family: 2
        }
    );
    assert_eq!(stereo().routing(), &Routing::swap());
    for w in [1u16, 3, 6] {
        let f = DelayLayout::new(bus(w), MAX, InterpolationMode::Linear);
        assert!(f.routing().is_none(), "width {w}");
        assert!(f.times().is_linked(), "width {w}");
    }
    // The stereo default is the swap whatever the interpolation or maximum.
    let f = DelayLayout::new(
        BusTopology::STEREO,
        MaxDelay::DEFAULT,
        InterpolationMode::None,
    );
    assert_eq!(f.routing(), &Routing::swap());
}

/// Channel times refuse a time that is not one, and the layout refuses a set that does not
/// fit it: a count that is not one for each channel past the first, and a time past the
/// longest delay.
#[test]
fn channel_times_refuse_what_is_not_a_time_and_what_does_not_fit() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.001] {
        assert!(
            matches!(
                ChannelTimes::each(&[Seconds(0.1), Seconds(bad)]),
                Err(ChannelTimesError::NotATime { channel: 2, .. })
            ),
            "{bad}"
        );
    }
    assert!(matches!(
        ChannelTimes::each(&[Seconds::ZERO; 16]),
        Err(ChannelTimesError::TooMany { count: 16, max: 15 })
    ));
    let times = ChannelTimes::each(&[Seconds(0.1), Seconds(0.2)]).unwrap();
    assert_eq!(times.fixed(), [Seconds(0.1), Seconds(0.2)]);
    assert!(!times.is_linked() && ChannelTimes::linked().is_linked());
    assert_eq!(
        stereo().with_times(times).unwrap_err(),
        ChannelTimesError::Length {
            width: 2,
            expected: 1,
            got: 2
        }
    );
    assert!(stereo().with_times(ChannelTimes::linked()).is_ok());
    let past = ChannelTimes::each(&[Seconds(1.5)]).unwrap();
    assert_eq!(
        stereo().with_times(past).unwrap_err(),
        ChannelTimesError::Past {
            channel: 1,
            time: Seconds(1.5),
            max: Seconds(1.0)
        }
    );
    assert!(
        stereo()
            .with_times(ChannelTimes::each(&[Seconds(1.0)]).unwrap())
            .is_ok(),
        "the longest is held"
    );
}
