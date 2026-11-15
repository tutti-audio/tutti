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
    let paired = Paired::read(|| {
        std::thread::sleep(Duration::from_millis(20));
        Ok::<_, std::convert::Infallible>(Duration::from_secs(7))
    })
    .unwrap();
    let after = Instant::now();
    let at = before + Duration::from_secs(1);
    let departs = paired.departure(at).unwrap();
    // The midpoint is at least 10 ms after `before` and before `after`, so the
    // second between `before` and `at` is a second less that, past the reading.
    assert!(departs <= Duration::from_secs(8) - Duration::from_millis(10));
    assert!(departs >= Duration::from_secs(8) - (after - before));
}

// A clock that cannot be read pairs nothing.
#[test]
fn an_unreadable_clock_pairs_nothing() {
    assert_eq!(
        Paired::read(|| Err::<Duration, _>("no clock")).unwrap_err(),
        "no clock"
    );
}
