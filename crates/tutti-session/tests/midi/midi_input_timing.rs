//! A `MidiInput` handle reads its timing: the latest correction its clock map
//! made and the count of its re-anchors. The cells belong to the port's entry:
//! they are the same across an unplug and a replug, open while a device is
//! connected and closed while it is away.
//!
//! The map is fed the engine's block readings. An offline test renders far
//! faster than the OS clock moves, so a wait of the OS clock between two blocks
//! makes the next reading late; the exact figures are the runtime tests'
//! (`input_inlets`), which control the clock.

use std::time::{Duration, Instant};

use crate::common::blocks;
use tutti_midi_device::{DeviceInfo, EndpointId, PortDirection};
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, persistent_endpoint};
use tutti_session::{Session, midi_input};
use tutti_types::{Resyncs, SampleRate, Seconds};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Ump, Voice2};

const KEYS: u32 = 1;
/// One block's time at 48 kHz.
const BLOCK_SECONDS: f64 = 512.0 / 48_000.0;

fn session() -> (Session, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareOptions::new())
        .sample_rate(SampleRate::of(48_000.0))
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

/// A block, then an event from the device: the device's thread reads the new
/// block reading when it places the event.
fn block_and_event(s: &Session, fake: &FakeCounters) {
    blocks(s, 1);
    let ump = Ump::from(Message::midi2(
        MidiChannel::FIRST,
        Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0x8000)),
    ));
    let _ = fake.send_input(EndpointId::from_raw(u64::from(KEYS)), ump, Instant::now());
}

/// Two readings 50 ms of the OS clock apart: the first builds the map, the
/// second is late by at least 39 ms, past the 20 ms bound. One wait, not a poll.
fn one_late_reading(s: &Session, fake: &FakeCounters) {
    block_and_event(s, fake);
    std::thread::sleep(Duration::from_millis(50));
    block_and_event(s, fake);
}

/// **The readings held from before the first connect stay live**: an input whose
/// device is absent at load reads closed 0; the device's arrival opens the same
/// cells, a late reading counts and shows on them, an unplug closes them with
/// what they read, and a replug opens them again with the count carried on
/// (the second connection's first map is no resync, its late reading is the
/// second).
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
    one_late_reading(&s, &fake);
    assert_eq!(resyncs.get(), Resyncs::new(1));
    let error = timing.get().get();
    assert!(error >= 0.050 - BLOCK_SECONDS - 1e-3, "error {error} s");

    unplug(&fake);
    let _ = s.update();
    assert!(timing.is_closed() && resyncs.is_closed(), "device away");
    assert_eq!(resyncs.get(), Resyncs::new(1), "the count stays");
    assert_eq!(timing.get().get(), error, "the last reading stays");

    plug(&fake);
    let _ = s.update();
    assert!(!timing.is_closed() && !resyncs.is_closed(), "reconnected");
    assert_eq!(resyncs.get(), Resyncs::new(1), "a connect counts nothing");
    one_late_reading(&s, &fake);
    assert_eq!(
        resyncs.get(),
        Resyncs::new(2),
        "cumulative across the replug"
    );
    // The handle read again is the same cells.
    assert_eq!(port.resyncs().get(), Resyncs::new(2));
    assert_eq!(s.midi_input(&"keys").unwrap().timing().get(), timing.get());
}
