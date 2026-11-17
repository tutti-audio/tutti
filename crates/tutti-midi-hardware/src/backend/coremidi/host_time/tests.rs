use super::*;
use std::time::Duration;

const TEN_MS: Duration = Duration::from_millis(10);

/// Ticks of `nanos` nanoseconds at `base`: `nanos * denom / numer`.
fn ticks(base: Timebase, nanos: u64) -> u64 {
    nanos * u64::from(base.denom()) / u64::from(base.numer())
}

fn pairing(os: Instant) -> Pairing {
    Pairing {
        base: Timebase::get(),
        paired: Paired::new(os, Duration::from_secs(50)),
    }
}

// A packet stamped 10 ms of ticks before the reading (50 s of host time) is
// 10 ms before it; a 0 stamp is the reading.
#[test]
fn a_packets_stamp_places_it_before_the_reading() {
    let os = Instant::now() + Duration::from_secs(100);
    let pairing = pairing(os);
    let ten_ms_ago = ticks(pairing.base, 49_990_000_000);
    assert_eq!(pairing.instant_of(ten_ms_ago), os - TEN_MS);
    assert_eq!(pairing.instant_of(0), os);
}

// A packet leaving 10 ms after the reading is stamped 10 ms of ticks after the
// reading's host time (50 s), and one leaving 10 ms before it 10 ms before: the
// departure is the arrival's inverse wherever the arrival reads (`instant_of`
// reads a packet's stamp at or before the reading, as a received packet's is).
#[test]
fn a_departure_is_stamped_in_host_ticks_after_the_reading() {
    let os = Instant::now() + Duration::from_secs(100);
    let pairing = pairing(os);
    let base = pairing.base;
    assert_eq!(
        pairing.host_time_of(os + TEN_MS),
        ticks(base, 50_010_000_000)
    );
    let earlier = os - TEN_MS;
    assert_eq!(pairing.host_time_of(earlier), ticks(base, 49_990_000_000));
    assert_eq!(pairing.instant_of(pairing.host_time_of(earlier)), earlier);
}
