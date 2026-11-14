use std::time::{Duration, Instant};

use tutti_fixtures::midi::{GS_RESET, GS_WAVETABLE};
use tutti_graph::{Losses, Offset};
use tutti_midi_hardware::{DeviceInfo, Devices, OpenOutput};
use tutti_midi_runtime::{Outgoing, SendAt};
use tutti_ump::{MidiGroup, SysexMessage, Ump};

/// Long enough to be a failure, not a race.
const PATIENCE: Duration = Duration::from_secs(5);

fn synth(devices: &Devices) -> DeviceInfo {
    let outputs: Vec<_> = devices
        .list()
        .into_iter()
        .filter(|d| d.direction.is_output())
        .collect();
    outputs
        .iter()
        .find(|d| d.name.contains(GS_WAVETABLE))
        .cloned()
        .unwrap_or_else(|| {
            let names: Vec<_> = outputs.iter().map(|d| d.name.as_str()).collect();
            panic!("the built-in GS Wavetable Synth output is missing; outputs: {names:?}")
        })
}

fn at(send: SendAt, ump: Ump) -> Outgoing {
    Outgoing {
        at: Offset::ZERO,
        send,
        ump,
    }
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

#[test]
fn the_synth_is_listed_as_an_output_with_a_stable_id_and_a_clean_input_list() {
    let devices = Devices::open().unwrap();
    let synth = synth(&devices);
    assert!(!synth.direction.is_input(), "{synth:?}");
    // A second listing names the same device by the same id.
    let again = Devices::open().unwrap();
    assert!(again.list().iter().any(|d| d.id == synth.id));
    // Inputs, of which a runner has none, are listed or not but never a
    // nameless entry.
    assert!(
        devices
            .list()
            .iter()
            .filter(|d| d.direction.is_input())
            .all(|d| !d.name.is_empty())
    );
}

#[test]
fn the_synth_is_sent_every_event_at_its_time_and_opens_again_after_a_close() {
    let devices = Devices::open().unwrap();
    let id = synth(&devices).id;
    let note_on = Ump([0x2090_3C64, 0, 0, 0]);
    let note_off = Ump([0x2080_3C00, 0, 0, 0]);
    let reset: Vec<Ump> = SysexMessage::from_framed(&GS_RESET)
        .unwrap()
        .packets(MidiGroup::FIRST)
        .collect();
    for _ in 0..2 {
        let mut out = devices.open_output(&id).expect("the synth opens");
        let now = Instant::now();
        let later = now + Duration::from_millis(90);
        let mut batch = vec![
            at(SendAt::Now, note_on),
            at(SendAt::At(now + Duration::from_millis(60)), note_off),
        ];
        batch.extend(reset.iter().map(|&ump| at(SendAt::At(later), ump)));
        assert_eq!(out.queue_timed(&batch), batch.len());
        drained(&out);
        assert_eq!(out.losses(), Losses::NONE);
        assert!(out.is_alive());
        // Each was held to its time: the last was due 90 ms in.
        assert!(
            now.elapsed() >= Duration::from_millis(85),
            "{:?}",
            now.elapsed()
        );
    }
}
