//! Every way the executor borrows a node's buffers: one hand-built graph that
//! reaches each form and bucket, rendered on one participant and in parallel.

use std::collections::{BTreeMap, BTreeSet};

use tutti_buffers::ChannelLayout;
use tutti_graph::testing::{self as common, Harness, Kind, bits, input_signal};
use tutti_graph::{
    Edge, EventEdge, EventInputPort, EventOutputPort, GraphSpec, InputPort, NodeKey, OutputPort,
    PortSource, Topology,
};

use crate::generator::{MAX_BLOCK, schedule, spec_for};

/// Every way the executor borrows a node's buffers is reached, and renders the
/// same on every participant count:
///
/// - the one-channel direct forms: a source, a one-in/one-out node in two
///   slots, and one in place;
/// - the stereo direct form in each of its shapes (0→2, 1→2, 2→1 including
///   two ports on one slot, 2→2 split, in place and half in place);
/// - the audio-only walk in each stack-table bucket: up to 4, up to 16, and
///   a 64-input sum (65 audio ports, no events);
/// - the general walk (event ports) in each of its buckets:
///   - `<4, 4>`, with and without an in-place channel;
///   - `<16, 4>`, six in-place channels plus an event input;
///   - `<MAX_PORTS, MAX_PORTS>`, reached both by 20 audio channels with
///     events and by six event inputs.
///
/// The random generator keeps nodes narrow, so the wide buckets are covered
/// here and nowhere else. The outputs of the event-free narrow chains are held
/// to the closed form their nodes compute (a gain, a sum, a fan, a constant
/// over the main inputs `x` and `x / 2`).
#[test]
fn every_borrow_form_is_reached_and_renders_alike_on_any_participant_count() {
    let key = NodeKey;
    let mut kinds = BTreeMap::new();
    let (src, split, inplace, six, twenty, sum, emit, consume) = (
        key(1),
        key(2),
        key(3),
        key(4),
        key(5),
        key(6),
        key(7),
        key(8),
    );
    let (pre6, mix6, mixwide, manyev, pre2, mix2) =
        (key(9), key(10), key(11), key(12), key(13), key(14));
    kinds.insert(
        src,
        Kind::Const {
            value: 0.375,
            width: 1,
        },
    );
    kinds.insert(split, Kind::Sum { inputs: 1 });
    kinds.insert(
        inplace,
        Kind::Gain {
            gain: 0.5,
            width: 1,
        },
    );
    kinds.insert(
        six,
        Kind::Gain {
            gain: 0.75,
            width: 6,
        },
    );
    kinds.insert(
        twenty,
        Kind::Gain {
            gain: 1.25,
            width: 20,
        },
    );
    kinds.insert(sum, Kind::Sum { inputs: 64 });
    kinds.insert(
        emit,
        Kind::Emitter {
            period: 5,
            phase: 2,
        },
    );
    kinds.insert(consume, Kind::Consumer { inputs: 1 });
    kinds.insert(
        pre6,
        Kind::Gain {
            gain: 0.5,
            width: 6,
        },
    );
    kinds.insert(
        mix6,
        Kind::Mixed {
            width: 6,
            events_in: 1,
            events_out: 0,
        },
    );
    kinds.insert(
        mixwide,
        Kind::Mixed {
            width: 20,
            events_in: 1,
            events_out: 1,
        },
    );
    kinds.insert(
        manyev,
        Kind::Mixed {
            width: 1,
            events_in: 6,
            events_out: 2,
        },
    );
    kinds.insert(
        pre2,
        Kind::Gain {
            gain: 0.25,
            width: 2,
        },
    );
    kinds.insert(
        mix2,
        Kind::Mixed {
            width: 2,
            events_in: 1,
            events_out: 1,
        },
    );

    let mut t = Topology {
        inputs: ChannelLayout::STEREO,
        ..Topology::default()
    };
    for (&k, kind) in &kinds {
        t.nodes.insert(k, spec_for(kind));
    }
    let out = |node, port| PortSource::Node(OutputPort::new(node, port));
    let mut wire = |node, port, from| {
        t.edges
            .insert(InputPort::new(node, port), vec![Edge::Direct(from)]);
    };
    wire(split, 0, PortSource::MainInput(0));
    wire(inplace, 0, out(split, 0));
    for c in 0..6 {
        wire(
            six,
            c,
            if c % 2 == 0 {
                PortSource::MainInput(1)
            } else {
                out(src, 0)
            },
        );
    }
    for c in 0..20 {
        wire(
            twenty,
            c,
            if c < 6 {
                out(six, c)
            } else {
                PortSource::MainInput(0)
            },
        );
    }
    // Every other port of the sum reads something different; the rest read
    // nothing, so the zero slot is borrowed many times over.
    let feeds: Vec<PortSource> = (0..20)
        .map(|c| out(twenty, c))
        .chain([out(inplace, 0), out(emit, 0), out(consume, 0)])
        .collect();
    for c in (0..64u16).step_by(2) {
        wire(sum, c, feeds[c as usize / 2 % feeds.len()]);
    }
    // `pre6` and `pre2` feed only their mixer, so the mixer's channels are
    // aliased in place; `mixwide` reads a main input, which cannot be.
    for c in 0..6 {
        wire(pre6, c, PortSource::MainInput(c % 2));
        wire(mix6, c, out(pre6, c));
    }
    for c in 0..20 {
        wire(mixwide, c, PortSource::MainInput(1 - c % 2));
    }
    for c in 0..2 {
        wire(pre2, c, PortSource::MainInput(c));
        wire(mix2, c, out(pre2, c));
    }
    // The direct stereo shapes: a stereo source, mono to stereo, stereo to
    // mono (once from two slots, once with both ports on one slot, so the
    // record dedupes a read), stereo to stereo split, fully in place and
    // half in place; and a three-wide node, which stays on the walk.
    let (st_src, fan12, sum2, sum2dup, st_pre, st_in, half, w3) = (
        key(20),
        key(21),
        key(22),
        key(23),
        key(24),
        key(25),
        key(26),
        key(27),
    );
    let direct_kinds = [
        (
            st_src,
            Kind::Const {
                value: 0.625,
                width: 2,
            },
        ),
        (
            fan12,
            Kind::Spec {
                behaviour: common::SpecBehaviour::Fan,
                ins: 1,
                outs: 2,
                latency: 0,
                tail: tutti_units::Tail::None,
            },
        ),
        (sum2, Kind::Sum { inputs: 2 }),
        (sum2dup, Kind::Sum { inputs: 2 }),
        (
            st_pre,
            Kind::Gain {
                gain: 0.5,
                width: 2,
            },
        ),
        (
            st_in,
            Kind::Gain {
                gain: 1.5,
                width: 2,
            },
        ),
        (
            half,
            Kind::Gain {
                gain: 0.75,
                width: 2,
            },
        ),
        (
            w3,
            Kind::Gain {
                gain: 2.0,
                width: 3,
            },
        ),
    ];
    for (k, kind) in direct_kinds {
        t.nodes.insert(k, spec_for(&kind));
        kinds.insert(k, kind);
    }
    let mut wire = |node, port, from| {
        t.edges
            .insert(InputPort::new(node, port), vec![Edge::Direct(from)]);
    };
    wire(fan12, 0, PortSource::MainInput(1));
    wire(sum2, 0, out(st_src, 0));
    wire(sum2, 1, out(fan12, 1));
    wire(sum2dup, 0, PortSource::MainInput(0));
    wire(sum2dup, 1, PortSource::MainInput(0));
    for c in 0..2 {
        wire(st_pre, c, PortSource::MainInput(c));
        wire(st_in, c, out(st_pre, c));
    }
    // `half` is the last reader of `fan12`'s port 0 only: channel 0 is in
    // place, channel 1 reads a main input.
    wire(half, 0, out(fan12, 0));
    wire(half, 1, PortSource::MainInput(1));
    for c in 0..3 {
        wire(w3, c, PortSource::MainInput(c % 2));
    }
    t.set_outputs(vec![
        out(sum, 0),
        out(inplace, 0),
        out(twenty, 19),
        out(mix6, 5),
        out(mixwide, 19),
        out(manyev, 0),
        out(mix2, 1),
        out(st_src, 1),
        out(sum2, 0),
        out(sum2dup, 0),
        out(st_in, 1),
        out(half, 0),
        out(half, 1),
        out(w3, 2),
    ]);
    let mut g = GraphSpec::new(t);
    let ev = |node, port| EventOutputPort::new(node, port);
    let mut connect = |node, port, from| {
        g.connect_events(EventInputPort::new(node, port), EventEdge::Direct(from));
    };
    connect(consume, 0, ev(emit, 0));
    connect(mix6, 0, ev(emit, 0));
    connect(mixwide, 0, ev(emit, 0));
    for p in 0..6 {
        connect(
            manyev,
            p,
            if p % 2 == 0 {
                ev(emit, 0)
            } else {
                ev(mixwide, 0)
            },
        );
    }
    connect(mix2, 0, ev(manyev, 1));
    let valid = g.validate().expect("valid");

    let mut harness = Harness::new(MAX_BLOCK);
    harness.switch(&valid, &kinds);
    let mut parallel = Harness::with_workers(MAX_BLOCK, 4);
    parallel.switch(&valid, &kinds);

    // The graph really reaches every form and bucket (derived from the
    // public op the same way `NodeTables::lower` and the executor do).
    let plan = harness.plan.clone().expect("switched");
    let mut forms = BTreeSet::new();
    for op in plan.ops() {
        if let tutti_graph::Op::Node {
            audio_in,
            audio_out,
            event_in,
            event_out,
            in_place,
            ..
        } = *op
        {
            let events = event_in.len() + event_out.len() > 0;
            let wide = audio_in.len().max(audio_out.len());
            let ewide = event_in.len().max(event_out.len());
            if events && in_place.0 != 0 {
                forms.insert("general, in place");
            }
            forms.insert(match (audio_in.len(), audio_out.len(), events) {
                (_, _, true) if wide <= 4 && ewide <= 4 => "general 4/4",
                (_, _, true) if wide <= 16 && ewide <= 4 => "general 16/4",
                (_, _, true) => "general wide",
                (0, 1, false) => "source",
                (1, 1, false) if in_place.get(0) => "in place",
                (1, 1, false) => "split",
                (0, 2, false) => "direct 0-2",
                (1, 2, false) => "direct 1-2",
                (2, 1, false) => "direct 2-1",
                (2, 2, false) => match in_place.0 {
                    0 => "direct 2-2 split",
                    0b11 => "direct 2-2 in place",
                    _ => "direct 2-2 half in place",
                },
                _ if wide <= 4 => "audio up to 4",
                _ if wide <= 16 => "audio up to 16",
                _ => "audio up to 64",
            });
        }
    }
    assert_eq!(
        forms,
        BTreeSet::from([
            "general 4/4",
            "general 16/4",
            "general wide",
            "general, in place",
            "source",
            "in place",
            "split",
            "direct 0-2",
            "direct 1-2",
            "direct 2-1",
            "direct 2-2 split",
            "direct 2-2 in place",
            "direct 2-2 half in place",
            "audio up to 4",
            "audio up to 16",
            "audio up to 64"
        ]),
        "the graph must reach every borrow form and bucket"
    );
    // Both routes into the widest general bucket, and in place in the two
    // narrower ones.
    let node = |k| {
        plan.ops()
            .iter()
            .find_map(|op| match *op {
                tutti_graph::Op::Node {
                    unit,
                    audio_in,
                    event_in,
                    in_place,
                    ..
                } if plan.units()[unit.index()].key == k => {
                    Some((audio_in.len(), event_in.len(), in_place.0))
                }
                _ => None,
            })
            .expect("placed")
    };
    assert_eq!(node(mixwide).0, 20);
    assert_eq!(node(manyev).1, 6);
    assert_eq!(node(mix6).2, 0b11_1111, "six in-place channels");
    assert_eq!(node(mix2).2, 0b11, "two in-place channels");

    // The outputs the event-free narrow chains write, in the order of
    // `set_outputs` above, are the closed forms of their nodes over the main
    // inputs `x` and `x / 2` (the harness feeds channel `c` the signal scaled
    // by `2^-c`).
    let mut frame = 0u64;
    for which in 0..5 {
        for n in schedule(which, 7, 300) {
            let input = input_signal::<f32>(frame, n);
            let got = harness.block(n, &input);
            assert_eq!(
                bits(&got),
                bits(&parallel.block(n, &input)),
                "the parallel render differs in the block at frame {frame}"
            );
            for (i, &x) in input.iter().enumerate() {
                let y = x * 0.5;
                let closed = [
                    (1, x * 0.5),
                    (2, x * 1.25),
                    (7, 0.625),
                    (8, (0.0 + 0.625) + y),
                    (9, (0.0 + x) + x),
                    (10, (y * 0.5) * 1.5),
                    (11, y * 0.75),
                    (12, y * 0.75),
                    (13, x * 2.0),
                ];
                for (channel, want) in closed {
                    assert_eq!(
                        got[channel][i].to_bits(),
                        want.to_bits(),
                        "output {channel} at frame {}",
                        frame + i as u64
                    );
                }
            }
            frame += n as u64;
        }
    }
}
