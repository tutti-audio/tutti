use super::*;
use crate::checks::declared::{
    DIRECTION_BIDIRECTIONAL, DIRECTION_INPUT, DIRECTION_OUTPUT, MIDI1_RESTRICTED,
    MIDI1_UNRESTRICTED, UI_HINT_SENDER,
};
use tutti_ump::{BlockDirection, FunctionBlockCount, FunctionBlocks, Midi1Port, UmpVersion};

fn info() -> PlatformInfo {
    PlatformInfo {
        midi1: false,
        midi2: false,
        rx_jr: false,
        tx_jr: false,
        static_blocks: false,
        block_count: 0,
        version_major: 0,
        version_minor: 0,
    }
}

fn block() -> PlatformBlock {
    PlatformBlock {
        number: 0,
        name: String::new(),
        active: true,
        direction: DIRECTION_BIDIRECTIONAL,
        ui_hint: UI_HINT_SENDER,
        midi1: 0,
        first_group: 0,
        groups: 1,
        ci_version: 0,
        max_sysex8_streams: 0,
    }
}

/// M2-104 §7.1.2: each flag of the service's endpoint info is the Endpoint Info
/// Notification's flag of the same name, and the version and count are its bytes.
#[test]
fn each_endpoint_info_flag_is_the_notifications_flag_of_its_name() {
    let read = endpoint_info(&PlatformInfo {
        midi1: true,
        midi2: false,
        rx_jr: true,
        tx_jr: false,
        static_blocks: true,
        block_count: 3,
        version_major: 1,
        version_minor: 1,
    })
    .unwrap();
    assert_eq!(
        read,
        EndpointInfoNotification {
            version: UmpVersion { major: 1, minor: 1 },
            static_function_blocks: true,
            function_blocks: FunctionBlocks::Count(FunctionBlockCount::saturating(3)),
            midi2: false,
            midi1: true,
            rx_jr: true,
            tx_jr: false,
        }
    );
    let other = endpoint_info(&PlatformInfo {
        midi2: true,
        tx_jr: true,
        ..info()
    })
    .unwrap();
    assert!(other.midi2 && other.tx_jr && !other.midi1 && !other.rx_jr);
}

/// M2-104 §7.1.2: a count of Function Blocks above 32 is reserved, and is kept as read.
#[test]
fn a_block_count_above_32_is_the_reserved_count() {
    let read = endpoint_info(&PlatformInfo {
        block_count: 40,
        ..info()
    })
    .unwrap();
    assert!(matches!(read.function_blocks, FunctionBlocks::Reserved(_)));
    assert_eq!(read.function_blocks.count(), None);
}

/// M2-104 §7.1.8: the service's direction and MIDI 1.0 port numbers are the spec's, and a
/// block's other fields and its name pass through, an empty name as none.
#[test]
fn a_blocks_numbers_read_as_the_specs_values() {
    let read = function_block(&PlatformBlock {
        number: 9,
        name: "Keys".into(),
        active: false,
        direction: DIRECTION_OUTPUT,
        midi1: MIDI1_RESTRICTED,
        first_group: 5,
        groups: 1,
        ci_version: 2,
        max_sysex8_streams: 7,
        ..block()
    });
    assert_eq!(read.name.as_deref(), Some("Keys"));
    assert!(!read.info.active);
    assert_eq!(read.info.direction, BlockDirection::Output);
    assert_eq!(read.info.midi1, Midi1Port::Restricted);
    assert_eq!(read.info.ui_hint, UiHint::Sender);
    assert_eq!(read.info.id.value().map(|id| id.get()), Some(9));
    assert_eq!(read.info.first_group.value().map(|g| g.get()), Some(5));
    assert_eq!(read.info.max_sysex8_streams.get(), 7);
    let nameless = function_block(&PlatformBlock {
        direction: DIRECTION_INPUT,
        midi1: MIDI1_UNRESTRICTED,
        ..block()
    });
    assert_eq!(nameless.name, None);
    assert_eq!(nameless.info.direction, BlockDirection::Input);
    assert_eq!(nameless.info.midi1, Midi1Port::Unrestricted);
}

/// M2-104 §7.1.8: a direction of `0b00`, which the spec reserves, reads as the reserved
/// direction, and a number that is no byte reads as reserved too.
#[test]
fn a_reserved_direction_is_kept_as_read() {
    for direction in [0, -1, 300] {
        let read = function_block(&PlatformBlock {
            direction,
            ..block()
        });
        assert!(
            matches!(read.info.direction, BlockDirection::Reserved(_)),
            "{direction}: {:?}",
            read.info.direction
        );
    }
}

/// The UI hint is two bits: a service number above them reads as its low two bits.
#[test]
fn a_ui_hint_is_its_low_two_bits() {
    let read = |ui_hint| {
        function_block(&PlatformBlock { ui_hint, ..block() })
            .info
            .ui_hint
    };
    assert_eq!(
        [0, 1, 2, 3, 6].map(read),
        [
            UiHint::Unknown,
            UiHint::Receiver,
            UiHint::Sender,
            UiHint::Both,
            UiHint::Sender
        ]
    );
}
