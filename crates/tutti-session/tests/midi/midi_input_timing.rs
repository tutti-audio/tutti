//! A `MidiInput` handle reads its timing: the latest correction its clock map
//! made and the count of its re-anchors. The cells belong to the port's entry:
//! they are the same across an unplug and a replug, open while a device is
//! connected and closed while it is away.
//!
//! The session dates its blocks by the frames rendered (`BlockDates::Frames`), so
//! no figure here depends on the wall clock. Each arrival is stamped at the date
//! the playhead reads back for the block before it, so the map is fed readings
//! exactly on its prediction and corrects nothing. A reading far from the map's
//! prediction needs a clock that stalls, which a frame-dated session has not:
//! the figures for a late reading and a re-anchor, and the count carried across a
//! reconnect, are the runtime tests' (`input_inlets`) and the clock map's.

use std::time::Duration;

use tutti_platform::Instant;

use crate::common::blocks;
use tutti_midi_device::{DeviceInfo, EndpointId, PortDirection};
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, persistent_endpoint};
use tutti_session::{BlockDates, Session, midi_input};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Ump, Voice2};
use tutti_units::{Resyncs, SampleRate, Seconds};

const KEYS: u32 = 1;

fn session() -> (Session, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let (s, _bg) = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(48_000.0))
        .block_dates(BlockDates::Frames)
        .build()
        .expect("builds");
    (s, fake)
}

fn plug(fake: &FakeCounters) {
    fake.plug(persistent_endpoint(KEYS, "Keys"), PortDirection::Input);
}

fn unplug(fake: &FakeCounters) {
    fake.unplug(EndpointId::from_raw(u64::from(KEYS)));
}

/// The listing of `Keys`, once plugged.
fn keys(s: &mut Session) -> DeviceInfo {
    s.midi()
        .devices()
        .into_iter()
        .find(|d| d.name == "Keys")
        .expect("Keys is listed")
}

/// The `Instant` the OS clock reads at `ns`, a date the playhead reads back: one
/// pairing of the two readings of the same clock.
fn instant_at(ns: i64) -> Instant {
    let (now, base) = (Instant::now(), tutti_platform::monotonic_ns());
    let span = Duration::from_nanos((ns - base).unsigned_abs());
    if ns >= base { now + span } else { now - span }
}

/// A block, then an event from the device stamped at that block's date: the
/// device's thread reads the new block reading when it places the event.
fn block_and_event(s: &Session, fake: &FakeCounters) {
    blocks(s, 1);
    let date = s.playhead().at().expect("the block is dated");
    let ump = Ump::from(Message::midi2(
        MidiChannel::FIRST,
        Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0x8000)),
    ));
    let _ = fake.send_input(EndpointId::from_raw(u64::from(KEYS)), ump, instant_at(date));
}

/// Two blocks, each followed by an event on its date: the first builds the map,
/// the second is observed against its prediction.
fn two_readings(s: &Session, fake: &FakeCounters) {
    block_and_event(s, fake);
    block_and_event(s, fake);
}

/// **The readings held from before the first connect stay live**: an input whose
/// device is absent at load reads closed 0; the device's arrival opens the same
/// cells, readings on the block dates correct nothing (under a nanosecond: the
/// dates are exact) and count no resync, an unplug closes them with what they
/// read, and a replug opens them again with the count carried on (a connect
/// counts nothing).
#[test]
fn the_readings_held_from_before_the_first_connect_live_across_a_replug() {
    let (mut s, fake) = session();
    plug(&fake);
    let _ = s.update();
    let saved = keys(&mut s);
    unplug(&fake);
    let _ = s.update();
    s.pool(midi_input("keys", saved)).expect("declares");
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    let (timing, resyncs) = (port.timing(), port.resyncs());
    assert!(timing.is_closed() && resyncs.is_closed(), "no device yet");
    assert_eq!(
        (timing.get(), resyncs.get()),
        (Seconds::new(0.0), Resyncs::ZERO)
    );

    plug(&fake);
    let _ = s.update();
    assert!(!timing.is_closed() && !resyncs.is_closed(), "connected");
    two_readings(&s, &fake);
    assert_eq!(
        resyncs.get(),
        Resyncs::ZERO,
        "dates on the map are no resync"
    );
    let error = timing.get().get();
    assert!(error.abs() < 1e-9, "error {error} s");

    unplug(&fake);
    let _ = s.update();
    assert!(timing.is_closed() && resyncs.is_closed(), "device away");
    assert_eq!(resyncs.get(), Resyncs::ZERO, "the count stays");
    assert_eq!(timing.get().get(), error, "the last reading stays");

    plug(&fake);
    let _ = s.update();
    assert!(!timing.is_closed() && !resyncs.is_closed(), "reconnected");
    assert_eq!(resyncs.get(), Resyncs::ZERO, "a connect counts nothing");
    two_readings(&s, &fake);
    assert_eq!(resyncs.get(), Resyncs::ZERO, "still none across the replug");
    // The handle read again is the same cells.
    assert_eq!(port.resyncs().get(), Resyncs::ZERO);
    assert_eq!(s.midi_input(&"keys").unwrap().timing().get(), timing.get());
}
