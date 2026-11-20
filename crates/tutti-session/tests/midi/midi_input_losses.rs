//! A `MidiInput` handle reads what its port lost: the events its device's inlet
//! had no room for, cumulative for the entry and carried across an unplug and a
//! replug on the same cells.

use std::time::Duration;

use tutti_platform::Instant;

use crate::common::blocks;
use tutti_graph::Losses;
use tutti_midi_device::{DeviceInfo, EndpointId, PortDirection};
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, persistent_endpoint};
use tutti_session::{Session, midi_input};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Ump, Voice2};
use tutti_units::SampleRate;

const KEYS: u32 = 1;
/// The hardware inlet's capacity: what a device may push between two blocks.
const INLET: u64 = 2048;

fn session() -> (Session, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![], vec![], usize::MAX);
    let s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
        .sample_rate(SampleRate::of(48_000.0))
        .build()
        .expect("builds");
    (s, fake)
}

fn plug(fake: &FakeCounters) {
    fake.plug(persistent_endpoint(KEYS, "Keys"), PortDirection::Input);
}

fn keys(s: &mut Session) -> DeviceInfo {
    s.midi()
        .devices()
        .into_iter()
        .find(|d| d.name == "Keys")
        .expect("Keys is listed")
}

/// `n` events from the device before any block reads them, all stamped with
/// the one reading of the clock, so they fall in one block whatever the load.
fn push(fake: &FakeCounters, n: u64) {
    let at = Instant::now();
    for i in 0..n {
        let ump = Ump::from(Message::midi2(
            MidiChannel::FIRST,
            Voice2::note_on(NoteNumber::of((i % 128) as u8), NoteVelocity::new(0x8000)),
        ));
        let _ = fake.send_input(EndpointId::from_raw(u64::from(KEYS)), ump, at);
    }
}

/// **A device whose inlet overflows raises its port's lost count**: 3,000
/// events pushed before any block reads them against an inlet of 2,048 lose
/// the 952 that did not fit, read through the handle; nothing else is lost.
#[test]
fn an_inlet_that_overflows_raises_the_ports_lost_count() {
    let (mut s, fake) = session();
    plug(&fake);
    let _ = s.update();
    let saved = keys(&mut s);
    s.pool(midi_input("keys", saved)).expect("declares");
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    assert_eq!(port.losses(), Losses::NONE);
    push(&fake, 3_000);
    assert_eq!(port.losses(), Losses::lost(3_000 - INLET));
}

/// **The lost count outlives an unplug and keeps counting after the replug**:
/// the second connection's inlet starts empty, and overflowing it adds to the
/// total on the same port, which a handle read again shows too.
#[test]
fn the_lost_count_survives_an_unplug_and_keeps_counting_after_the_replug() {
    let (mut s, fake) = session();
    plug(&fake);
    let _ = s.update();
    let saved = keys(&mut s);
    s.pool(midi_input("keys", saved)).expect("declares");
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    push(&fake, 3_000);
    let first = 3_000 - INLET;
    assert_eq!(port.losses().lost, first);

    fake.unplug(EndpointId::from_raw(u64::from(KEYS)));
    let _ = s.update();
    assert_eq!(
        port.losses().lost,
        first,
        "the count stays while it is away"
    );

    plug(&fake);
    let _ = s.update();
    blocks(&s, 4);
    assert_eq!(port.losses().lost, first, "a connect counts nothing");
    push(&fake, 3_000);
    assert_eq!(
        port.losses().lost,
        2 * first,
        "cumulative across the replug"
    );
    assert_eq!(s.midi_input(&"keys").unwrap().losses().lost, 2 * first);
}

/// `n` events from the device stamped ten seconds ahead (one reading of the
/// clock for all), so they wait in the inlet's list for their frame.
fn push_ahead(fake: &FakeCounters, n: u64) {
    let at = Instant::now() + Duration::from_secs(10);
    for i in 0..n {
        let ump = Ump::from(Message::midi2(
            MidiChannel::FIRST,
            Voice2::note_on(NoteNumber::of((i % 128) as u8), NoteVelocity::new(0x8000)),
        ));
        let _ = fake.send_input(EndpointId::from_raw(u64::from(KEYS)), ump, at);
    }
}

/// **What the inlet evicts from its waiting list is the port's, across a
/// replug**: 100 events more than the full list holds are evicted, read through
/// the handle as `evicted`; the second connection's inlet, a new one, adds its
/// own to the same total.
#[test]
fn an_inlet_eviction_raises_the_ports_count_and_carries_across_a_replug() {
    let (mut s, fake) = session();
    plug(&fake);
    let _ = s.update();
    let saved = keys(&mut s);
    s.pool(midi_input("keys", saved)).expect("declares");
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    assert_eq!(port.losses(), Losses::NONE);
    // The engine's clock reads, so an arrival is placed ahead of its block.
    blocks(&s, 2);
    push_ahead(&fake, INLET);
    blocks(&s, 2);
    push_ahead(&fake, 100);
    blocks(&s, 2);
    assert_eq!(port.losses().evicted, 100);

    fake.unplug(EndpointId::from_raw(u64::from(KEYS)));
    let _ = s.update();
    blocks(&s, 4);
    let _ = s.update();
    assert_eq!(
        port.losses().evicted,
        100,
        "the count stays while it is away"
    );

    plug(&fake);
    let _ = s.update();
    assert!(!port.timing().is_closed(), "the cells reopened");
    blocks(&s, 2);
    push_ahead(&fake, INLET);
    blocks(&s, 2);
    push_ahead(&fake, 100);
    blocks(&s, 2);
    assert_eq!(port.losses().evicted, 200, "cumulative across the replug");
}

/// **An input node whose block translates more than its mailbox holds counts
/// the rest as refused on its port**: 2,000 events due in one block overrun the
/// mailbox, and the port's reading says so, with nothing lost at the inlet.
#[test]
fn an_input_node_mailbox_overflow_counts_as_refused_on_the_port() {
    let (mut s, fake) = session();
    plug(&fake);
    let _ = s.update();
    let saved = keys(&mut s);
    s.pool(midi_input("keys", saved)).expect("declares");
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    blocks(&s, 2);
    push(&fake, 2_000);
    blocks(&s, 2);
    let losses = port.losses();
    assert!(losses.refused > 0, "{losses:?}");
    assert_eq!(losses.lost, 0);
}

/// **An arrival past its block is counted late on the port**, across a replug:
/// an event stamped a second before now reaches a block that began after it.
#[test]
fn a_late_arrival_is_counted_on_the_port_across_a_replug() {
    let (mut s, fake) = session();
    plug(&fake);
    let _ = s.update();
    let saved = keys(&mut s);
    s.pool(midi_input("keys", saved)).expect("declares");
    let _ = s.update();
    let port = s.midi_input(&"keys").expect("loaded");
    let late = |fake: &FakeCounters| {
        let ump = Ump::from(Message::midi2(
            MidiChannel::FIRST,
            Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0x8000)),
        ));
        let at = Instant::now() - Duration::from_millis(500);
        let _ = fake.send_input(EndpointId::from_raw(u64::from(KEYS)), ump, at);
    };
    blocks(&s, 400);
    late(&fake);
    blocks(&s, 2);
    assert_eq!(port.losses().late, 1);

    fake.unplug(EndpointId::from_raw(u64::from(KEYS)));
    let _ = s.update();
    plug(&fake);
    let _ = s.update();
    blocks(&s, 400);
    late(&fake);
    blocks(&s, 2);
    assert_eq!(port.losses().late, 2, "cumulative across the replug");
}
