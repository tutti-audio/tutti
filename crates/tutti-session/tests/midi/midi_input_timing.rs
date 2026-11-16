//! A `MidiInput` handle reads its timing: the latest correction its clock map
//! made and the count of its re-anchors. The map is fed the engine's block
//! readings. An offline test renders far faster than the OS clock moves: a block
//! advances the graph's frame by 512 and the OS time by almost nothing, so the
//! reading comes early by about one block (10.67 ms at 48 kHz); a wait of the
//! OS clock between two blocks makes the next one late.

use std::time::{Duration, Instant};

use crate::common::blocks;
use tutti_midi_device::EndpointId;
use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
use tutti_session::{Session, midi_input};
use tutti_types::{Resyncs, SampleRate, Seconds};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Ump, Voice2};

const KEYS: u64 = 1;
/// One block's time at 48 kHz.
const BLOCK_SECONDS: f64 = 512.0 / 48_000.0;

fn rig() -> (Session, FakeCounters) {
    let (backend, fake) = FakeBackend::build_with(vec![endpoint(KEYS, "Keys")], vec![], usize::MAX);
    let mut s = Session::builder()
        .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareOptions::new())
        .sample_rate(SampleRate::of(48_000.0))
        .build()
        .expect("builds");
    let _ = s.update();
    let keys = s.midi().devices().remove(0);
    s.pool(midi_input("keys", keys)).expect("declares");
    let _ = s.update();
    (s, fake)
}

/// A block, then an event from the device: the device's thread reads the new
/// block reading when it places the event.
fn block_and_event(s: &Session, fake: &FakeCounters) {
    blocks(s, 1);
    let ump = Ump::from(Message::midi2(
        MidiChannel::FIRST,
        Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0x8000)),
    ));
    let _ = fake.send_input(EndpointId::from_raw(KEYS), ump, Instant::now());
}

/// **A port nothing has been read from reads no timing and no resyncs.**
#[test]
fn a_fresh_input_reads_no_correction_and_no_resyncs() {
    let (s, _fake) = rig();
    let input = s.midi_input(&"keys").expect("loaded");
    assert_eq!(input.timing().get(), Seconds::new(0.0));
    assert_eq!(input.resyncs().get(), Resyncs::ZERO);
}

/// **A reading that jumps past `RESYNC` shows on `resyncs()` and `timing()`**:
/// the device's clock map is built on the first block's reading; 50 ms of the
/// OS clock later the next block, one block of frames on, reads late by at
/// least 39 ms. That is a re-anchor, counted once, and the gauge reads its
/// whole error: positive (late) and past the 20 ms bound.
#[test]
fn a_reading_past_the_resync_bound_counts_and_the_gauge_reads_the_whole_error() {
    let (s, fake) = rig();
    let input = s.midi_input(&"keys").expect("loaded");
    block_and_event(&s, &fake);
    assert_eq!(input.resyncs().get(), Resyncs::ZERO);
    // One wait, not a poll: the OS clock must move on past the bound.
    std::thread::sleep(Duration::from_millis(50));
    block_and_event(&s, &fake);
    assert_eq!(input.resyncs().get(), Resyncs::new(1));
    let error = input.timing().get().get();
    assert!(error >= 0.050 - BLOCK_SECONDS - 1e-3, "error {error} s");
}

/// **A small early reading is followed: it shows on `timing()` as a step within
/// an eighth of the bound, and counts no resync.** Blocks rendered at once put
/// the second reading about one block early, which the map follows by an
/// eighth.
#[test]
fn a_reading_within_the_bound_is_followed_and_counts_no_resync() {
    let (s, fake) = rig();
    let input = s.midi_input(&"keys").expect("loaded");
    block_and_event(&s, &fake);
    block_and_event(&s, &fake);
    assert_eq!(input.resyncs().get(), Resyncs::ZERO);
    let step = input.timing().get().get();
    assert!(step != 0.0 && step.abs() <= 0.020 / 8.0, "step {step} s");
}
