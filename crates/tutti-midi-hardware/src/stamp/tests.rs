use super::*;

// An event stamped 30 ms before the clock's reading arrived 30 ms before the
// OS's.
#[test]
fn an_event_arrived_as_long_before_the_reading_as_its_stamp_is() {
    let os = Instant::now() + Duration::from_secs(10);
    let clock = Duration::from_secs(5);
    assert_eq!(
        arrival(os, clock, clock - Duration::from_millis(30)),
        os - Duration::from_millis(30)
    );
    assert_eq!(arrival(os, clock, clock), os);
}

// A stamp after the reading lands on it.
#[test]
fn a_stamp_after_the_reading_lands_on_it() {
    let os = Instant::now();
    let clock = Duration::from_secs(5);
    assert_eq!(arrival(os, clock, clock + Duration::from_millis(1)), os);
}

// `departure` inverts `arrival`: an instant 30 ms after the reading is the
// clock's reading plus 30 ms, 30 ms before it minus 30 ms, and the clock never
// reads below zero.
#[test]
fn a_departure_is_the_clock_reading_at_the_instant() {
    let os = Instant::now() + Duration::from_secs(10);
    let clock = Duration::from_secs(5);
    let ms = Duration::from_millis;
    assert_eq!(departure(os, clock, os + ms(30)), Some(clock + ms(30)));
    assert_eq!(departure(os, clock, os - ms(30)), Some(clock - ms(30)));
    assert_eq!(departure(os, clock, os), Some(clock));
    assert_eq!(departure(os, ms(10), os - ms(30)), Some(Duration::ZERO));
    let stamped = clock - ms(30);
    assert_eq!(
        departure(os, clock, arrival(os, clock, stamped)),
        Some(stamped)
    );
}

// A pairing is taken at the midpoint of the two readings of `Instant` the clock
// was read between, and a send time is the clock's reading plus as long as it is
// after that midpoint.
#[test]
fn a_pairing_is_the_midpoint_of_the_readings_around_the_clock() {
    let before = Instant::now();
    let after = before + Duration::from_millis(20);
    let paired = Paired::midpoint(before, after, Duration::from_secs(7));
    let at = before + Duration::from_secs(1);
    // The midpoint is 10 ms after `before`, so `at` is 990 ms after it.
    assert_eq!(
        paired.departure(at),
        Some(Duration::from_secs(7) + Duration::from_millis(990))
    );
    assert_eq!(
        paired.arrival(Duration::from_secs(7)),
        before + Duration::from_millis(10)
    );
}

// Readings out of order pair at the earlier one rather than before it.
#[test]
fn readings_out_of_order_pair_at_the_first() {
    let before = Instant::now() + Duration::from_millis(5);
    let after = before - Duration::from_millis(5);
    let paired = Paired::midpoint(before, after, Duration::from_secs(1));
    assert_eq!(paired.arrival(Duration::from_secs(1)), before);
}
