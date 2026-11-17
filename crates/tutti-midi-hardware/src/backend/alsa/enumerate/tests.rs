use super::*;
use tutti_alsa_ffi::{DIR_BIDIRECTION, DIR_INPUT, DIR_OUTPUT};

fn block(direction: u32, first_group: u32, num_groups: u32, flags: u32) -> UmpBlock {
    UmpBlock {
        id: 0,
        active: true,
        flags,
        direction,
        first_group,
        num_groups,
        midi_ci_version: 0,
        sysex8_streams: 0,
        ui_hint: 0,
        name: None,
    }
}

// A sequencer block the UMP spec cannot hold is left out, not clamped: a
// reserved direction (M2-104 §7.1.8 `0b00`), a span past Group 16, a number
// above 31.
/// Spec: m2-104/6.1.1#1, m2-104/6.1.1#2
#[test]
fn a_block_the_spec_cannot_hold_is_refused() {
    assert!(block_of(&block(0, 0, 1, 0)).is_none(), "reserved direction");
    assert!(
        block_of(&block(DIR_INPUT, 15, 2, 0)).is_none(),
        "past Group 16"
    );
    assert!(block_of(&block(DIR_INPUT, 0, 0, 0)).is_none(), "no Groups");
    let mut high = block(DIR_INPUT, 0, 1, 0);
    high.id = 32;
    assert!(block_of(&high).is_none(), "block 32");
    assert!(block_of(&block(DIR_OUTPUT, 15, 1, 0)).is_some());
}

// A MIDI 1.0 port block reads as one, restricted when the sequencer says
// low speed, and only over a single Group (§7.1.8).
#[test]
fn a_midi1_port_block_keeps_its_bandwidth() {
    use tutti_ump::Midi1Port;
    let port = |flags, groups| block_of(&block(DIR_BIDIRECTION, 0, groups, flags)).unwrap();
    assert_eq!(port(BLOCK_IS_MIDI1, 1).midi1(), Midi1Port::Unrestricted);
    assert_eq!(
        port(BLOCK_IS_MIDI1 | BLOCK_IS_LOWSPEED, 1).midi1(),
        Midi1Port::Restricted
    );
    assert_eq!(port(BLOCK_IS_MIDI1, 2).midi1(), Midi1Port::No, "a span");
    assert_eq!(port(0, 1).midi1(), Midi1Port::No);
}

// A client with no UMP Endpoint record is MIDI 1.0 with no blocks, however
// the rest reads.
#[test]
fn a_client_with_no_endpoint_record_is_midi1() {
    let cap = capability_of(None);
    assert_eq!(cap, UmpCapability::midi1());
}
