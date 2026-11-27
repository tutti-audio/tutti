//! The output every backend offers: the platform's target output (`target`), opened,
//! sent to at a time, closed and opened again. A missing target is a failure with its
//! name, never a skip. Opening the Windows synth needs an audio endpoint, which CI
//! installs a virtual audio driver for.

use std::time::Duration;
use tutti_platform::Instant;

use tutti_fixtures::midi::GS_RESET;
use tutti_graph::Offset;
use tutti_midi_hardware::{Devices, OpenOutput};
use tutti_midi_runtime::{Outgoing, SendAt};
use tutti_ump::{MidiGroup, SysexMessage, Ump};
use tutti_units::Losses;

use super::target::{HOLDS_EVENTS_TO_THEIR_TIME, output};

/// Long enough to be a failure, not a race.
const PATIENCE: Duration = Duration::from_secs(30);

fn at(send: SendAt, ump: Ump) -> Outgoing {
    Outgoing::new(Offset::ZERO, send, ump)
}

/// Waits until the output has nothing left to send.
fn drained(out: &OpenOutput) {
    tutti_fixtures::wait_until("the output's events to be sent", PATIENCE, || {
        out.pending() == 0
    });
}

/// The proof that `devices`' backend is ready.
fn started(devices: &Devices) -> tutti_midi_hardware::Started {
    tutti_fixtures::block_on(devices.start()).expect("the backend starts")
}

#[test]
fn a_listed_output_has_the_same_id_in_a_second_listing_and_no_input_is_nameless() {
    let devices = Devices::open(&tutti_platform::Executor::new()).unwrap();
    let target = output(&devices);
    // A second listing names the same device by the same id.
    let again = Devices::open(&tutti_platform::Executor::new()).unwrap();
    assert_eq!(output(&again).id, target.id);
    // Inputs are listed or not but never a nameless entry.
    assert!(
        devices
            .list()
            .iter()
            .filter(|d| d.direction.is_input())
            .all(|d| !d.name.is_empty())
    );
}

#[test]
fn an_output_is_sent_every_event_at_its_time_and_opens_again_after_a_close() {
    let devices = Devices::open(&tutti_platform::Executor::new()).unwrap();
    let id = output(&devices).id;
    let note_on = Ump([0x2090_3C64, 0, 0, 0]);
    let note_off = Ump([0x2080_3C00, 0, 0, 0]);
    let reset: Vec<Ump> = SysexMessage::from_framed(&GS_RESET)
        .unwrap()
        .packets(MidiGroup::FIRST)
        .collect();
    for round in 0..2 {
        let mut out = devices
            .open_output(started(&devices), &id)
            .unwrap_or_else(|e| panic!("the output opens in round {round}: {e:?}"));
        let now = Instant::now();
        let later = now + Duration::from_millis(90);
        let mut batch = vec![
            at(SendAt::Now, note_on),
            at(SendAt::At(now + Duration::from_millis(60)), note_off),
        ];
        batch.extend(reset.iter().map(|&ump| at(SendAt::At(later), ump)));
        assert_eq!(out.queue_timed(&batch).packets(), batch.len());
        drained(&out);
        assert_eq!(out.losses(), Losses::NONE);
        assert!(out.is_alive());
        // On a backend whose own thread holds an event until it is due, the last was
        // due 90 ms in and a hold never ends early; only lateness depends on load.
        if HOLDS_EVENTS_TO_THEIR_TIME {
            assert!(
                now.elapsed() >= Duration::from_millis(85),
                "{:?}",
                now.elapsed()
            );
        }
    }
}

/// With the app's clone of the executor kept past both, dropping `Devices` and
/// then the output closes the port, and the output opens again at once: the output
/// ends its sender before the executor's last clone drops.
#[test]
fn an_output_dropped_before_the_executors_last_clone_is_free_to_open_again() {
    let executor = tutti_platform::Executor::new();
    let devices = Devices::open(&executor).unwrap();
    let id = output(&devices).id;
    let out = devices
        .open_output(started(&devices), &id)
        .expect("the output opens");
    drop(devices);
    drop(out);
    drop(executor);
    let devices = Devices::open(&tutti_platform::Executor::new()).unwrap();
    let again = devices.open_output(started(&devices), &id);
    assert!(again.is_ok(), "the output opens again: {:?}", again.err());
}
