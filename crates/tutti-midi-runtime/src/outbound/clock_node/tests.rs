use super::*;
use crate::ClockSends;
use tutti_graph::contract::Direct;
use tutti_graph::{EventKind, TransportPosition};
use tutti_rt::Publisher;
use tutti_types::{Beat, Bpm};
use tutti_ump::{Message, System};

const BLOCK: usize = 64;

/// A clock node sending the Beat Clock, driven block by block.
fn rig() -> Direct<ClockNode> {
    let meter = Publisher::new(MeterMap::default()).reader();
    let node = ClockNode::new(MidiGroup::FIRST, SampleRate::SR_48K, meter);
    node.controls.set_sends(ClockSends {
        timing: true,
        ..ClockSends::default()
    });
    Direct::new(node, SampleRate::SR_48K, BLOCK)
}

/// Runs a block rolling at 120 BPM from `beat`; the System messages it sent.
fn roll(d: &mut Direct<ClockNode>, beat: f64) -> Vec<System> {
    d.set_transport(TransportPosition::new(
        true,
        Bpm::of(120.0),
        Beat(beat),
        None,
    ));
    d.block();
    d.events_out(0)
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Midi(ump) => match ump.decode() {
                Ok(Message::System(_, system)) => Some(system),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// **`reset` returns the clock to a freshly prepared one's state**: a transport
/// that keeps rolling from where the last block ended sends no Continue, and
/// the same transport after a reset is a play edge mid-song, which sends one.
#[test]
fn reset_forgets_the_transport_the_clock_saw() {
    let mut d = rig();
    let first = roll(&mut d, 0.0);
    assert!(first.contains(&System::Start), "a play at beat 0 starts");
    // 64 frames at 120 BPM and 48 kHz.
    let next = BLOCK as f64 * 2.0 / 48_000.0;
    let running = roll(&mut d, next);
    assert!(
        !running.contains(&System::Continue),
        "a run-on is no play edge"
    );
    d.node.reset();
    let after = roll(&mut d, 2.0 * next);
    assert!(
        after.contains(&System::Continue),
        "a reset clock sees a play edge mid-song"
    );
}
