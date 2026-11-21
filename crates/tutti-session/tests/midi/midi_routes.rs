//! Route rules through the session: what a rule wires from the hardware
//! input's ports, a union of rules, and a disabled rule.
//!
//! The rest of it is in `midi.rs`
//! (`route_rules_wire_the_hardware_input`: one channel's ports and the
//! channelless one, an any-channel rule, the fallback, unrouting) and in
//! `tutti-midi-runtime`'s `compile_routes` tests; what neither had is the
//! union of two rules on one target and a rule that is disabled, which are
//! here.

use std::collections::BTreeSet;

use crate::common::{Keyed, with_input};
use tutti_graph::NodeKey;
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_midi_runtime::{CHANNELLESS_PORT, MIDI_INPUT_PORTS};
use tutti_session::{Feeder, MidiError, MidiRouteRule, Session};
use tutti_ump::MidiChannel;

/// A stand-in synth: one MIDI event input.
#[derive(Clone)]
struct Sink;

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn session() -> (Keyed, NodeKey) {
    with_input(None)
}

/// The input ports that feed `node` by the routes.
fn ports(s: &Session, input: NodeKey, node: NodeKey) -> BTreeSet<u16> {
    s.graph_ref()
        .feed(node, Feeder::Routes)
        .iter()
        .map(|e| {
            assert_eq!(e.node(), input);
            u16::try_from(e.index()).unwrap()
        })
        .collect()
}

fn channelless() -> u16 {
    u16::try_from(CHANNELLESS_PORT).unwrap()
}

/// **Two channel rules on one target take the union of their ports and the
/// channelless one, beside an any-channel rule on another target taking all
/// seventeen.**
#[test]
fn two_rules_on_one_target_take_the_union() {
    let (mut s, input) = session();
    let lead = s.graph().add(ForkByClone(Sink));
    let pad = s.graph().add(ForkByClone(Sink));
    s.midi()
        .route(
            MidiRouteRule::new()
                .channel(MidiChannel::of(2))
                .to(lead.erase()),
        )
        .unwrap();
    s.midi()
        .route(
            MidiRouteRule::new()
                .channel(MidiChannel::of(5))
                .to(lead.erase()),
        )
        .unwrap();
    s.midi()
        .route(MidiRouteRule::new().to(pad.erase()))
        .unwrap();
    assert_eq!(
        ports(&s, input, lead.erase()),
        BTreeSet::from([2, 5, channelless()])
    );
    assert_eq!(
        ports(&s, input, pad.erase()),
        (0..u16::try_from(MIDI_INPUT_PORTS).unwrap()).collect()
    );
}

/// **A disabled rule routes nothing, and arming it wires its target again**,
/// through the host's `set_routes`.
#[test]
fn a_disabled_rule_routes_nothing_until_it_is_armed() {
    let (mut s, input) = session();
    let lead = s.graph().add(ForkByClone(Sink));
    let armed = MidiRouteRule::new()
        .channel(MidiChannel::FIRST)
        .to(lead.erase());
    s.midi().set_routes(input, [&armed], None, Some).unwrap();
    assert!(!ports(&s, input, lead.erase()).is_empty(), "armed");

    let off = MidiRouteRule::new()
        .channel(MidiChannel::FIRST)
        .to(lead.erase())
        .disabled();
    s.midi().set_routes(input, [&off], None, Some).unwrap();
    assert!(ports(&s, input, lead.erase()).is_empty(), "disabled");

    s.midi().set_routes(input, [&armed], None, Some).unwrap();
    assert!(!ports(&s, input, lead.erase()).is_empty(), "re-armed");
}

/// A node with no event input: it takes no MIDI.
#[derive(Clone)]
struct Mute;

impl Node for Mute {
    fn shape(&self) -> Shape {
        Shape::EMPTY
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A rule naming a target that takes no MIDI is refused at the call**, with
/// the target named, and nothing is declared.
#[test]
fn a_rule_to_a_target_without_an_event_input_is_refused() {
    let (mut s, input) = session();
    let mute = s.graph().add(ForkByClone(Mute));
    let lead = s.graph().add(ForkByClone(Sink));
    let refused = s
        .midi()
        .route(MidiRouteRule::new().to(lead.erase()).to(mute.erase()));
    assert!(
        matches!(refused, Err(MidiError::NoEventInput(n)) if n == mute.erase()),
        "{refused:?}"
    );
    assert!(
        ports(&s, input, lead.erase()).is_empty(),
        "the refused rule wired none of its targets"
    );
}

/// **A fallback that takes no MIDI is refused at the call**, and the fallback
/// in force is kept.
#[test]
fn a_fallback_without_an_event_input_is_refused() {
    let (mut s, input) = session();
    let mute = s.graph().add(ForkByClone(Mute));
    let lead = s.graph().add(ForkByClone(Sink));
    s.midi().set_route_fallback(Some(lead.erase())).unwrap();
    let refused = s.midi().set_route_fallback(Some(mute.erase()));
    assert!(
        matches!(refused, Err(MidiError::NoEventInput(n)) if n == mute.erase()),
        "{refused:?}"
    );
    assert!(!ports(&s, input, lead.erase()).is_empty(), "kept");
}

/// **`set_routes` from a node with no event outputs is refused as not an input.**
#[test]
fn set_routes_from_a_node_that_is_not_an_input_is_refused() {
    let (mut s, _input) = session();
    let mute = s.graph().add(ForkByClone(Mute));
    let lead = s.graph().add(ForkByClone(Sink));
    let rule = MidiRouteRule::new().to(lead.erase());
    let refused = s.midi().set_routes(mute.erase(), [&rule], None, Some);
    assert!(
        matches!(refused, Err(MidiError::NotAnInput(n)) if n == mute.erase()),
        "{refused:?}"
    );
}
