use tutti_fixtures::midi::{GS_RESET, GS_WAVETABLE};
use tutti_winmm_ffi::{MidiOut, output_caps, output_count};

/// The position of the built-in synth, or a failure that lists what is there.
fn synth() -> u32 {
    let names: Vec<String> = (0..output_count())
        .filter_map(|i| output_caps(i).ok().map(|c| c.name))
        .collect();
    let found = names.iter().position(|n| n.contains(GS_WAVETABLE));
    let Some(index) = found else {
        panic!("the built-in GS Wavetable Synth output is missing; outputs: {names:?}");
    };
    u32::try_from(index).expect("a position fits u32")
}

#[test]
fn the_synth_takes_short_messages_and_a_sysex_and_opens_again_after_a_close() {
    let index = synth();
    for round in 0..2 {
        let mut out = MidiOut::open(index)
            .unwrap_or_else(|e| panic!("the synth opens in round {round}: {e:?}"));
        out.short(0x0064_3C90).expect("note on");
        out.short(0x0000_3C80).expect("note off");
        out.long(&GS_RESET).expect("the GS reset is sent whole");
        // Dropping resets and closes the port, so the next open is not refused
        // as allocated.
    }
}
