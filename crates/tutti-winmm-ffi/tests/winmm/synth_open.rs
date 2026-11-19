use tutti_fixtures::midi::{GS_RESET, GS_WAVETABLE};
use tutti_ump::{SysexMessage, Ump};
use tutti_winmm_ffi::{MidiOut, Output, OutputPort, ports};

/// The built-in synth, or a failure that lists what is there.
fn synth() -> OutputPort {
    let ports = ports::<Output>();
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
        out.send(&Ump([0x2090_3C64, 0, 0, 0])).expect("note on");
        out.send(&Ump([0x2080_3C00, 0, 0, 0])).expect("note off");
        out.long(SysexMessage::from_framed(&GS_RESET).expect("a framed message"))
            .expect("the GS reset is sent whole");
        // Dropping resets and closes the port, so the next open is not refused
        // as allocated.
    }
}
