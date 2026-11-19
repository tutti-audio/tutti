// The arithmetic of `Timebase::ticks` and `duration` is pinned in
// `tutti-coremidi-ffi`; these tests are about how `Pairing` uses it.
use super::*;
use std::time::Duration;

const TEN_MS: Duration = Duration::from_millis(10);
const HOST_NOW: Duration = Duration::from_secs(50);

fn pairing(os: Instant) -> Pairing {
    Pairing {
        base: Timebase::get(),
        paired: Paired::new(os, HOST_NOW),
    }
}

// A packet stamped 10 ms of ticks before the reading (50 s of host time) is
// 10 ms before it; a 0 stamp is the reading.
#[test]
fn a_packets_stamp_places_it_before_the_reading() {
    let os = Instant::now() + Duration::from_secs(100);
    let pairing = pairing(os);
    let ten_ms_ago = pairing.base.ticks(HOST_NOW - TEN_MS);
    assert_eq!(pairing.instant_of(ten_ms_ago), os - TEN_MS);
    assert_eq!(pairing.instant_of(0), os);
}

// A packet leaving 10 ms after the reading is stamped 10 ms of ticks after the
// reading's host time, and one leaving 10 ms before it 10 ms before: the
// departure is the arrival's inverse wherever the arrival reads (`instant_of`
// reads a packet's stamp at or before the reading, as a received packet's is).
#[test]
fn a_departure_is_stamped_in_host_ticks_after_the_reading() {
    let os = Instant::now() + Duration::from_secs(100);
    let pairing = pairing(os);
    let base = pairing.base;
    assert_eq!(
        pairing.host_time_of(os + TEN_MS),
        base.ticks(HOST_NOW + TEN_MS)
    );
    let earlier = os - TEN_MS;
    assert_eq!(pairing.host_time_of(earlier), base.ticks(HOST_NOW - TEN_MS));
    assert_eq!(pairing.instant_of(pairing.host_time_of(earlier)), earlier);
}

// What CoreMIDI holds a packet until is its stamp: one sent `Now` is stamped 0
// (delivered at once) and one sent 2 s ahead is stamped 2 s of ticks after the
// reading.
#[test]
fn a_now_packet_is_stamped_zero_and_a_scheduled_one_its_host_time() {
    let os = Instant::now() + Duration::from_secs(100);
    let two_s = Duration::from_secs(2);
    let mut reading = Some(pairing(os));
    assert_eq!(stamp_of(SendAt::Now, &mut reading), 0);
    assert_eq!(
        stamp_of(SendAt::At(os + two_s), &mut reading),
        Timebase::get().ticks(HOST_NOW + two_s)
    );
}

// A stamp already read as a duration places a packet as its ticks do: 10 ms
// before the reading, and the zero stamp ("now") at it.
#[test]
fn a_duration_stamp_places_a_packet_before_the_reading() {
    let os = Instant::now() + Duration::from_secs(100);
    let pairing = pairing(os);
    assert_eq!(pairing.instant_at(HOST_NOW - TEN_MS), os - TEN_MS);
    assert_eq!(pairing.instant_at(Duration::ZERO), os);
}
