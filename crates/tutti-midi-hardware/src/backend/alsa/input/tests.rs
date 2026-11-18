use super::*;

/// **Events leave the port in the order they arrived**, whatever two pairings
/// of the clocks say: an event placed before the one ahead of it is put at its
/// instant, and one placed after it keeps its own.
#[test]
fn an_event_is_never_placed_before_the_one_ahead_of_it() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let mut latest = None;
    assert_eq!(
        in_order(&mut latest, t0 + ms(5)),
        t0 + ms(5),
        "the first is as placed"
    );
    assert_eq!(
        in_order(&mut latest, t0 + ms(7)),
        t0 + ms(7),
        "a later one keeps its place"
    );
    assert_eq!(
        in_order(&mut latest, t0 + ms(6)),
        t0 + ms(7),
        "one placed before the one ahead is put with it"
    );
    assert_eq!(in_order(&mut latest, t0 + ms(8)), t0 + ms(8));
}

/// **A held pairing is read again when none is held, when the event came after
/// it, and once it is older than `PAIRING_AGE`**; an event at or before its
/// clock reading, inside the age, reuses it.
#[test]
fn a_pairing_is_read_again_when_missing_late_or_older_than_its_age() {
    let t0 = Instant::now();
    let s = Duration::from_secs;
    let paired = Paired::new(t0, s(10));
    let held = Some((paired, t0));
    assert!(pairing_is_stale(None, s(9), t0));
    assert!(!pairing_is_stale(held, s(9), t0));
    assert!(!pairing_is_stale(held, s(10), t0 + PAIRING_AGE));
    assert!(pairing_is_stale(
        held,
        s(10),
        t0 + PAIRING_AGE + Duration::from_millis(1)
    ));
    assert!(
        pairing_is_stale(held, s(11), t0),
        "an event after the pairing"
    );
}

/// **An event's instant is as long before `now` as the pairing says, however
/// late the pump reads it**: events stamped 0, 50 and 100 ms on the queue keep
/// that spacing when they are all read long after, which a read-time stamp
/// would collapse to nothing.
#[test]
fn events_keep_the_spacing_they_were_stamped_with_when_read_late() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let queue_now = ms(500);
    let read_at = t0 + ms(400);
    let mut arrivals = Arrivals::default();
    let mut pairings = 0;
    let instants: Vec<Instant> = [0, 50, 100]
        .into_iter()
        .map(|stamp| {
            arrivals.at(ms(stamp), read_at, || {
                pairings += 1;
                Some(Paired::new(read_at, queue_now))
            })
        })
        .collect();
    assert_eq!(instants[1] - instants[0], ms(50));
    assert_eq!(instants[2] - instants[1], ms(50));
    assert_eq!(
        instants[2],
        read_at - ms(400),
        "stamped 400 ms before the clock read"
    );
    assert_eq!(pairings, 1, "one pairing serves events inside its age");
}

/// A pairing that cannot be read places the event at `now`.
#[test]
fn an_unreadable_clock_places_an_event_at_the_read() {
    let now = Instant::now();
    let mut arrivals = Arrivals::default();
    assert_eq!(arrivals.at(Duration::from_secs(1), now, || None), now);
}
