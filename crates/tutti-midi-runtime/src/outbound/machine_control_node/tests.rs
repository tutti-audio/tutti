use super::*;
use tutti_graph::TransportPosition;
use tutti_graph::contract::Direct;
use tutti_types::{Bpm, SampleRate};

/// One block under a transport at beat 0 that `playing` or not; how many
/// events the node sent.
fn look(d: &mut Direct<MachineControlNode>, playing: bool) -> usize {
    d.set_transport(TransportPosition::new(
        playing,
        Bpm::of(120.0),
        Beat(0.0),
        None,
    ));
    d.block();
    d.events_out(0).len()
}

/// **`reset` returns the node to a freshly prepared one's state**: a stopped
/// look then a playing one sends PLAY; the first look after a reset is no
/// change to report.
#[test]
fn reset_forgets_the_last_look() {
    let new = || MachineControlNode::new(MidiGroup::FIRST, Recipient::AllCall, MtcRate::Fps30);
    let mut kept = Direct::new(new(), SampleRate::SR_48K, 64);
    assert_eq!(look(&mut kept, false), 0, "the first look sends nothing");
    assert!(look(&mut kept, true) > 0, "a start sends PLAY");

    let mut reset = Direct::new(new(), SampleRate::SR_48K, 64);
    assert_eq!(look(&mut reset, false), 0);
    reset.node.reset();
    assert_eq!(
        look(&mut reset, true),
        0,
        "the look after a reset is a first"
    );
}
