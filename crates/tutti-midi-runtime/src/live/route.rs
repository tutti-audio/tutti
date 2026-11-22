//! Inbound MIDI routing compiled to wiring: which of a [`MidiInputNode`]'s
//! ports each listener takes.
//!
//! **Routing is wiring.** The input node sends each event out of its
//! channel's port in its Group, so a rule "channel 3 of Group 1 reaches the
//! lead synth" is nothing but event edges from that port and the Group's
//! channelless port to the synth.
//! [`compile_routes`] turns a set of rules and a fallback into those port sets,
//! as a pure function: the host owns the rules (as entities, or a list) and
//! wires the result.
//!
//! [`MidiInputNode`]: super::MidiInputNode

use std::collections::{BTreeMap, BTreeSet};

use tutti_graph::{EventOutputPort, NodeKey};
use tutti_ump::{MidiChannel, MidiGroup};

use super::input_node::MidiInputPorts;

/// One inbound routing rule, as [`compile_routes`] reads it: a Group and
/// channel filter and the listeners it reaches.
///
/// `T` is the host's name for a listener (a graph node, an ECS entity). A
/// disabled rule is one the host leaves out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteRule<'a, T> {
    /// `None` matches every Group the input node hears, `Some(g)` only
    /// Group `g`: a channel is the channel of one Group (M2-104 Appendix B.1).
    pub group: Option<MidiGroup>,
    /// `None` matches every channel, `Some(c)` only channel `c`.
    pub channel: Option<MidiChannel>,
    /// The listeners this rule feeds. A rule with none routes nothing, and
    /// covers nothing: its channel still falls through to the fallback.
    pub targets: &'a [T],
}

/// Compiles inbound routing rules into the ports of the input nodes `inputs`
/// (each laid out as `layout`) that each listener takes edges from.
///
/// - A rule gives each of its targets [`MidiInputPorts::listening`] of its
///   Group and channel: that channel's port and its Group's channelless one,
///   or every port of the Group for an any-channel rule (of every Group for
///   an any-Group one). A target several rules name takes the union.
/// - `fallback` takes every port no rule covers: a channel no rule names, and
///   a Group's channelless port while no rule with a target covers it.
///
/// So with a fallback the compile is total (every port reaches some listener),
/// and without one an uncovered port reaches nobody: silence, rather than a
/// note arriving at whichever listener happened to come first. Every listener
/// in the result takes at least one port.
///
/// ```
/// use tutti_graph::NodeKey;
/// use tutti_midi_runtime::{compile_routes, MidiInputPorts, RouteRule};
/// use tutti_ump::{MidiChannel, MidiGroup};
///
/// let input = NodeKey(1);
/// let layout = MidiInputPorts::FIRST_GROUP;
/// let (lead, sampler) = ("lead", "sampler");
/// let ports = compile_routes(
///     layout,
///     &[input],
///     [RouteRule {
///         group: Some(MidiGroup::FIRST),
///         channel: Some(MidiChannel::FIRST),
///         targets: &[lead],
///     }],
///     Some(sampler),
/// );
/// let channel = |c| layout.channel(input, MidiGroup::FIRST, MidiChannel::of(c)).unwrap();
/// let channelless = layout.channelless(input, MidiGroup::FIRST).unwrap();
/// assert_eq!(ports[&lead], [channel(0), channelless].into());
/// // The unrouted channels.
/// assert_eq!(ports[&sampler], (1..16).map(channel).collect());
/// ```
pub fn compile_routes<'a, T: Copy + Ord + 'a>(
    layout: MidiInputPorts,
    inputs: &[NodeKey],
    rules: impl IntoIterator<Item = RouteRule<'a, T>>,
    fallback: Option<T>,
) -> BTreeMap<T, BTreeSet<EventOutputPort>> {
    let mut ports: BTreeMap<T, BTreeSet<EventOutputPort>> = BTreeMap::new();
    let mut covered: BTreeSet<EventOutputPort> = BTreeSet::new();
    for rule in rules.into_iter().filter(|r| !r.targets.is_empty()) {
        let theirs: BTreeSet<EventOutputPort> = inputs
            .iter()
            .flat_map(|&node| layout.listening(node, rule.group, rule.channel))
            .collect();
        if theirs.is_empty() {
            // A Group the node does not hear: nothing to take.
            continue;
        }
        covered.extend(theirs.iter().copied());
        for &target in rule.targets {
            ports
                .entry(target)
                .or_default()
                .extend(theirs.iter().copied());
        }
    }
    if let Some(target) = fallback {
        let uncovered: BTreeSet<EventOutputPort> = inputs
            .iter()
            .flat_map(|&node| layout.listening(node, None, None))
            .filter(|p| !covered.contains(p))
            .collect();
        if !uncovered.is_empty() {
            ports.entry(target).or_default().extend(uncovered);
        }
    }
    ports
}

#[cfg(test)]
mod tests;
