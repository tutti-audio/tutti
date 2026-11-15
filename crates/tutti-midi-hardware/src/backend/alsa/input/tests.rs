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
