// The arithmetic of `Timebase::ticks` and `duration` is pinned in
// `tutti-coremidi-ffi`; these tests are about how `Pairing` uses the clocks.
use super::*;
use std::time::Duration;

const TEN_MS: Duration = Duration::from_millis(10);
const HOST_NOW: Duration = Duration::from_secs(50);

fn pairing(os: Instant) -> Pairing {
    Pairing {
        paired: Paired::new(os, HOST_NOW),
    }
}

// A packet stamped 10 ms before the reading (50 s of host time) is 10 ms before
// it; no stamp ("now") is the reading.
#[test]
fn a_packets_stamp_places_it_before_the_reading() {
    let os = Instant::now() + Duration::from_secs(100);
    let pairing = pairing(os);
    assert_eq!(
        pairing.instant_of_stamp(Some(HOST_NOW - TEN_MS)),
        os - TEN_MS
    );
    assert_eq!(pairing.instant_of_stamp(None), os);
}

// A packet leaving 10 ms after the reading is stamped 10 ms after the reading's
// host time, and one leaving 10 ms before it 10 ms before: the departure is the
// arrival's inverse wherever the arrival reads (`instant_of_stamp` reads a stamp
// at or before the reading, as a received packet's is).
#[test]
fn a_departure_is_stamped_in_host_time_after_the_reading() {
    let os = Instant::now() + Duration::from_secs(100);
    let pairing = pairing(os);
    assert_eq!(pairing.host_time_of(os + TEN_MS), Some(HOST_NOW + TEN_MS));
    let earlier = os - TEN_MS;
    assert_eq!(pairing.host_time_of(earlier), Some(HOST_NOW - TEN_MS));
    assert_eq!(
        pairing.instant_of_stamp(pairing.host_time_of(earlier)),
        earlier
    );
}

// What CoreMIDI holds a packet until is its stamp: one sent `Now` has none
// (delivered at once) and one sent 2 s ahead is stamped 2 s after the reading.
#[test]
fn a_now_packet_has_no_stamp_and_a_scheduled_one_its_host_time() {
    let os = Instant::now() + Duration::from_secs(100);
    let two_s = Duration::from_secs(2);
    let mut reading = Some(pairing(os));
    assert_eq!(stamp_of(SendAt::Now, &mut reading), None);
    assert_eq!(
        stamp_of(SendAt::At(os + two_s), &mut reading),
        Some(HOST_NOW + two_s)
    );
}
