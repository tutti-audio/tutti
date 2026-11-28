//! What the service declares of its endpoints.

use tutti_ump::{FunctionBlockCount, FunctionBlocks, UmpVersion};
use tutti_winmidi2_ffi::{Endpoint, Loopback, endpoints, ensure_service_available};

fn loopback(which: Loopback) -> Endpoint {
    ensure_service_available().expect("the MIDI service is available");
    endpoints()
        .expect("the service lists its endpoints")
        .into_iter()
        .find(|e| e.loopback == Some(which))
        .unwrap_or_else(|| panic!("the service lists diagnostic loopback {which:?}"))
}

/// **A diagnostic loopback endpoint declares what the service creates it with**, which is
/// its diagnostics transport's: it supports the MIDI 1.0 and the MIDI 2.0 protocols
/// (`MidiEndpointCapabilities_SupportsMidi1Protocol | ..Midi2Protocol`, which the service
/// keeps as the endpoint's supported protocols until discovery says otherwise), and it runs no
/// UMP Endpoint discovery, so it declares no JR Timestamps, no UMP version, no Function Block
/// and no static flag. What stream configuration it agrees is not pinned: the diagnostics
/// transport sets none, and the service's source does not state the value a client reads.
#[test]
fn a_loopback_endpoint_declares_both_protocols_and_nothing_else() {
    for which in [Loopback::A, Loopback::B] {
        let declared = loopback(which)
            .declared
            .expect("the service answers for a diagnostic loopback endpoint");
        assert!(
            declared.info.midi1 && declared.info.midi2,
            "{which:?}: {declared:?}"
        );
        assert!(
            !declared.info.rx_jr && !declared.info.tx_jr && !declared.info.static_function_blocks
        );
        assert_eq!(declared.info.version, UmpVersion { major: 0, minor: 0 });
        assert_eq!(
            declared.info.function_blocks,
            FunctionBlocks::Count(FunctionBlockCount::saturating(0))
        );
        assert!(declared.blocks.is_empty(), "{which:?}: {declared:?}");
    }
}
