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
    let paired = Paired::read(|| Ok::<_, std::convert::Infallible>(s(10))).unwrap();
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
