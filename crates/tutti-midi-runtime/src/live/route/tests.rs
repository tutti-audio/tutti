use super::*;

const FIRST: MidiInputPorts = MidiInputPorts::FIRST_GROUP;
const IN: NodeKey = NodeKey(1);
const INPUTS: &[NodeKey] = &[IN];

/// The ports of `layout`'s `group` for `channels`, and its channelless port.
fn ports_of(layout: MidiInputPorts, group: u8, channels: &[u8]) -> BTreeSet<EventOutputPort> {
    let group = MidiGroup::of(group);
    channels
        .iter()
        .map(|&c| layout.channel(IN, group, MidiChannel::of(c)).unwrap())
        .chain(layout.channelless(IN, group))
        .collect()
}

/// Every port of `layout`.
fn every_port(layout: MidiInputPorts) -> BTreeSet<EventOutputPort> {
    layout.listening(IN, None, None).collect()
}

fn ch(c: u8) -> Option<MidiChannel> {
    Some(MidiChannel::of(c))
}

/// **A channel rule gives its channel's port and the channelless one; an
/// any-channel rule all seventeen; two rules on one target the union.**
#[test]
fn a_rule_gives_its_targets_its_channels_ports() {
    let ports = compile_routes(
        FIRST,
        INPUTS,
        [
            RouteRule {
                group: Some(MidiGroup::FIRST),
                channel: ch(2),
                targets: &["lead"],
            },
            RouteRule {
                group: Some(MidiGroup::FIRST),
                channel: ch(5),
                targets: &["lead"],
            },
            RouteRule {
                group: Some(MidiGroup::FIRST),
                channel: None,
                targets: &["pad"],
            },
        ],
        None,
    );
    assert_eq!(ports["lead"], ports_of(FIRST, 0, &[2, 5]));
    assert_eq!(ports["pad"], every_port(FIRST));
    assert_eq!(ports.len(), 2, "no fallback, no other listener");
}

/// **The compile is total over the rule set, and the fallback holds only
/// unrouted ports.** Over every subset of four candidate rules (channel
/// 0, 5, 15 and any, alternating between two targets), with a distinct
/// fallback:
///
/// - every port reaches some listener;
/// - each rule's targets hold every port of its channel;
/// - the fallback holds exactly the ports no rule's channel names;
/// - no listener appears that is neither a target nor the fallback.
#[test]
fn the_compile_is_total_and_the_fallback_holds_only_unrouted_ports() {
    let candidates = [
        (ch(0), ["a"]),
        (ch(5), ["b"]),
        (ch(15), ["a"]),
        (None, ["b"]),
    ];
    for mask in 0u8..16 {
        let rules: Vec<RouteRule<'_, &str>> = candidates
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, (channel, targets))| RouteRule {
                group: Some(MidiGroup::FIRST),
                channel: *channel,
                targets,
            })
            .collect();
        let ports = compile_routes(FIRST, INPUTS, rules.iter().copied(), Some("fallback"));

        let reached: BTreeSet<EventOutputPort> = ports.values().flatten().copied().collect();
        assert_eq!(
            reached,
            every_port(FIRST),
            "rule set {mask:#06b}: every port reaches a listener"
        );
        let mut routed = BTreeSet::new();
        for rule in &rules {
            let theirs: BTreeSet<EventOutputPort> = FIRST
                .listening(IN, Some(MidiGroup::FIRST), rule.channel)
                .collect();
            for target in rule.targets {
                assert!(
                    ports[target].is_superset(&theirs),
                    "rule set {mask:#06b}: {target} holds its rule's ports"
                );
            }
            routed.extend(theirs);
        }
        let unrouted: BTreeSet<EventOutputPort> = every_port(FIRST)
            .into_iter()
            .filter(|p| !routed.contains(p))
            .collect();
        assert_eq!(
            ports.get("fallback").cloned().unwrap_or_default(),
            unrouted,
            "rule set {mask:#06b}: the fallback holds the unrouted ports only"
        );
        assert!(
            ports.keys().all(|k| ["a", "b", "fallback"].contains(k)),
            "rule set {mask:#06b}: no listener out of nowhere"
        );
    }
}

/// **A rule with no target covers nothing**: its channel still falls
/// through to the fallback, and it names no listener.
#[test]
fn a_rule_without_targets_leaves_its_channel_to_the_fallback() {
    let ports = compile_routes(
        FIRST,
        INPUTS,
        [RouteRule::<&str> {
            group: Some(MidiGroup::FIRST),
            channel: ch(0),
            targets: &[],
        }],
        Some("fallback"),
    );
    assert_eq!(ports["fallback"], every_port(FIRST));
    assert_eq!(ports.len(), 1);
}

/// Without a fallback an uncovered port reaches nobody, and with
/// nothing declared the result is empty.
#[test]
fn without_a_fallback_an_unrouted_port_reaches_nobody() {
    assert!(compile_routes::<&str>(FIRST, INPUTS, [], None).is_empty());
    let ports = compile_routes(
        FIRST,
        INPUTS,
        [RouteRule {
            group: Some(MidiGroup::FIRST),
            channel: ch(9),
            targets: &["drums"],
        }],
        None,
    );
    assert_eq!(ports.len(), 1);
    assert_eq!(ports["drums"], ports_of(FIRST, 0, &[9]));
}

/// **A rule names a Group as well as a channel**: channel 1 of Group 1 and
/// channel 1 of Group 2 reach different listeners, an any-Group rule reaches
/// the channel in every Group, and a rule for a Group the node does not hear
/// reaches nobody (M2-104 Appendix B.1).
#[test]
fn a_rule_names_a_group_as_well_as_a_channel() {
    let two = MidiInputPorts::through(MidiGroup::of(1));
    let ports = compile_routes(
        two,
        INPUTS,
        [
            RouteRule {
                group: Some(MidiGroup::FIRST),
                channel: ch(1),
                targets: &["one"],
            },
            RouteRule {
                group: Some(MidiGroup::of(1)),
                channel: ch(1),
                targets: &["two"],
            },
            RouteRule {
                group: None,
                channel: ch(7),
                targets: &["both"],
            },
            RouteRule {
                group: Some(MidiGroup::of(5)),
                channel: ch(2),
                targets: &["unheard"],
            },
        ],
        None,
    );
    assert_eq!(ports["one"], ports_of(two, 0, &[1]));
    assert_eq!(ports["two"], ports_of(two, 1, &[1]));
    assert_eq!(
        ports["both"],
        &ports_of(two, 0, &[7]) | &ports_of(two, 1, &[7])
    );
    assert!(!ports.contains_key("unheard"));
}
