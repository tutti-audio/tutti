use tutti_fixtures::midi::{GS_RESET, GS_WAVETABLE};
use tutti_winmm_ffi::{MidiOut, OutputPort, output_ports};

/// The built-in synth, or a failure that lists what is there.
fn synth() -> OutputPort {
    let ports = output_ports();
    let names: Vec<String> = ports.iter().map(|p| p.caps().name.clone()).collect();
    let Some(port) = ports
        .into_iter()
        .find(|p| p.caps().name.contains(GS_WAVETABLE))
    else {
        panic!("the built-in GS Wavetable Synth output is missing; outputs: {names:?}");
    };
    port
}

#[test]
fn the_synth_takes_short_messages_and_a_sysex_and_opens_again_after_a_close() {
    let port = synth();
    for round in 0..2 {
        let mut out = MidiOut::open(&port)
            .unwrap_or_else(|e| panic!("the synth opens in round {round}: {e:?}"));
        out.short(0x0064_3C90).expect("note on");
        out.short(0x0000_3C80).expect("note off");
        out.long(&GS_RESET).expect("the GS reset is sent whole");
        // Dropping resets and closes the port, so the next open is not refused
        // as allocated.
    }
}
