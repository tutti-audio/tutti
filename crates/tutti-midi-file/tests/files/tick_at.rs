//! The beat-to-tick inverse: `MetricalView::tick_at` and `Tick::from_beat` against
//! the tick grid of the division (a tick is `1/ticks_per_quarter` of a quarter note).

use crate::common::{metrical, track};
use tutti_midi_file::{SmfDivision, StandardMidiFile, Tick};
use tutti_ump::TicksPerQuarterNote;
use tutti_units::Beat;

fn tpq(ticks: u16) -> TicksPerQuarterNote {
    TicksPerQuarterNote::of(ticks)
}

fn file(ticks_per_quarter: u16) -> StandardMidiFile {
    crate::common::format0(SmfDivision::per_quarter(ticks_per_quarter), track(vec![])).unwrap()
}

#[test]
fn tick_at_inverts_beat_at_for_every_tick_at_each_division() {
    for tpq in [480, 96, 100] {
        let file = file(tpq);
        for t in (0..=40_000).chain([1 << 40, (1 << 52) + 1]) {
            assert_eq!(
                metrical(&file).tick_at(metrical(&file).beat_at(Tick(t))),
                Some(Tick(t)),
                "tick {t} at {tpq}"
            );
        }
    }
}

#[test]
fn whole_and_fractional_beats_land_on_their_analytic_ticks() {
    assert_eq!(
        metrical(&file(480)).tick_at(Beat::new(1.5)),
        Some(Tick(720))
    );
    assert_eq!(metrical(&file(96)).tick_at(Beat::new(4.0)), Some(Tick(384)));
    assert_eq!(
        metrical(&file(100)).tick_at(Beat::new(0.25)),
        Some(Tick(25))
    );
    assert_eq!(Tick::from_beat(Beat::new(0.0), tpq(480)), Some(Tick(0)));
    assert_eq!(Tick::from_beat(Beat::new(-0.0), tpq(480)), Some(Tick(0)));
}

#[test]
fn an_off_grid_beat_rounds_to_the_nearer_tick() {
    // 480 per quarter: 0.4 and 0.6 of a tick either side of tick 10.
    assert_eq!(
        Tick::from_beat(Beat::new(10.4 / 480.0), tpq(480)),
        Some(Tick(10))
    );
    assert_eq!(
        Tick::from_beat(Beat::new(10.6 / 480.0), tpq(480)),
        Some(Tick(11))
    );
    // 100 per quarter: 0.504 beat is 50.4 ticks.
    assert_eq!(Tick::from_beat(Beat::new(0.504), tpq(100)), Some(Tick(50)));
    assert_eq!(Tick::from_beat(Beat::new(0.506), tpq(100)), Some(Tick(51)));
}

#[test]
fn a_beat_halfway_between_two_ticks_goes_to_the_later() {
    // 128 per quarter makes k + 1/2 ticks exact in binary: 1/256 beat is half a tick.
    for (half, later) in [(1.0, 1), (3.0, 2), (5.0, 3), (257.0, 129)] {
        assert_eq!(
            Tick::from_beat(Beat::new(half / 256.0), tpq(128)),
            Some(Tick(later)),
            "{half}/256 beat"
        );
    }
    // Just under a half stays on the earlier tick.
    assert_eq!(
        Tick::from_beat(Beat::new((1.0_f64 / 256.0).next_down()), tpq(128)),
        Some(Tick(0))
    );
}

#[test]
fn a_beat_before_zero_is_refused_even_within_half_a_tick() {
    assert_eq!(metrical(&file(480)).tick_at(Beat::new(-1.0)), None);
    assert_eq!(metrical(&file(480)).tick_at(Beat::new(-0.25 / 480.0)), None);
    assert_eq!(
        metrical(&file(480)).tick_at(Beat::new(f64::NEG_INFINITY)),
        None
    );
}

#[test]
fn a_beat_past_the_last_tick_or_not_a_number_is_refused() {
    // The last tick, u64::MAX, is 2^64 - 1; at 1 tick per quarter 2^64 beats is one past it.
    assert_eq!(Tick::from_beat(Beat::new(2f64.powi(64)), tpq(1)), None);
    assert_eq!(
        Tick::from_beat(Beat::new(2f64.powi(63)), tpq(1)),
        Some(Tick(1 << 63))
    );
    assert_eq!(Tick::from_beat(Beat::new(f64::INFINITY), tpq(480)), None);
    assert_eq!(Tick::from_beat(Beat::new(f64::NAN), tpq(480)), None);
}
