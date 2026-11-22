use std::time::Duration;
use tutti_platform::Instant;

use tutti_fixtures::midi::GS_RESET;
use tutti_graph::Offset;
use tutti_midi_hardware::{Devices, OpenOutput};
use tutti_midi_runtime::{Outgoing, SendAt};
use tutti_ump::{MidiGroup, SysexMessage, Ump};
use tutti_units::Losses;

use super::gs_synth::synth;

/// Long enough to be a failure, not a race.
const PATIENCE: Duration = Duration::from_secs(30);

fn at(send: SendAt, ump: Ump) -> Outgoing {
    Outgoing::new(Offset::ZERO, send, ump)
}

/// Waits until the output has nothing left to send.
fn drained(out: &OpenOutput) {
    let start = Instant::now();
    while out.pending() > 0 {
        assert!(
            start.elapsed() < PATIENCE,
            "{} events never sent",
            out.pending()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The proof that `devices`' backend is ready: winmm has nothing to start.
fn started(devices: &Devices) -> tutti_midi_hardware::Started {
    tutti_fixtures::block_on(devices.start()).expect("winmm has nothing to start")
}

#[test]
fn the_synth_is_sent_every_event_at_its_time_and_opens_again_after_a_close() {
    let devices = Devices::open(&tutti_platform::Executor::new()).unwrap();
    let id = synth(&devices).id;
    let note_on = Ump([0x2090_3C64, 0, 0, 0]);
    let note_off = Ump([0x2080_3C00, 0, 0, 0]);
    let reset: Vec<Ump> = SysexMessage::from_framed(&GS_RESET)
        .unwrap()
        .packets(MidiGroup::FIRST)
        .collect();
    for round in 0..2 {
        let mut out = devices
            .open_output(started(&devices), &id)
            .unwrap_or_else(|e| panic!("the synth opens in round {round}: {e:?}"));
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
        // Each was held to its time: the last was due 90 ms in, and a hold never
        // ends early; only lateness depends on load.
        assert!(
            now.elapsed() >= Duration::from_millis(85),
            "{:?}",
            now.elapsed()
        );
    }
}

/// With the app's clone of the executor kept past both, dropping `Devices` and
/// then the output closes the port, and the synth opens again at once: the output
/// ends its sender before the executor's last clone drops.
#[test]
fn an_output_dropped_before_the_executors_last_clone_frees_the_synth_for_a_reopen() {
    let executor = tutti_platform::Executor::new();
    let devices = Devices::open(&executor).unwrap();
    let id = synth(&devices).id;
    let out = devices
        .open_output(started(&devices), &id)
        .expect("the synth opens");
    drop(devices);
    drop(out);
    drop(executor);
    let devices = Devices::open(&tutti_platform::Executor::new()).unwrap();
    let again = devices.open_output(started(&devices), &id);
    assert!(again.is_ok(), "the synth opens again: {:?}", again.err());
}
