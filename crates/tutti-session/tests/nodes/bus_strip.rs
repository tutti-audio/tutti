//! The mixer strip on a placed bus: a bus edge folds into and out of it by the
//! speakers it declares.
//!
//! Every figure is analytic: a source writes a constant exactly representable
//! value, and a fold's gain is the fold's stated rule, not another render's.

use tutti_buffers::{BusTopology, ChannelLayout, ChannelTopology, InterleavedMut, MaxBlock};
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status, Unforkable};
use tutti_nodes::{BusStripConfig, BusStripNode};
use tutti_session::{MainOutput, OutputPort, Session, SessionBuilder, ValueErrorKind};
use tutti_units::{Balance, Samples};

/// Constant `values` on its output channels, declaring `out` as its bus.
struct Src {
    values: Vec<f32>,
    out: BusTopology,
}

impl Node for Src {
    fn shape(&self) -> Shape {
        Shape::source(self.out)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (c, &v) in self.values.iter().enumerate() {
            io.output(c).fill(v);
        }
        Status::Constant
    }
    fn reset(&mut self) {}
}

fn offline(builder: SessionBuilder, device: u16) -> Session {
    builder
        .outputs(ChannelLayout::from(device))
        .max_block(MaxBlock::of(Samples::new(64)))
        .build()
        .expect("builds")
}

/// Frame 0 of the second device block (the first installs the plan), after the
/// session commits. `device` channels wide.
fn heard(s: &mut Session, device: u16) -> Vec<f32> {
    let _ = s.update();
    let layout = ChannelLayout::from(device);
    let mut out = vec![-9.0f32; usize::from(device) * 64];
    for _ in 0..2 {
        s.render(&mut InterleavedMut::new(&mut out, layout).unwrap());
    }
    out[..usize::from(device)].to_vec()
}

/// A mono source wired into a stereo strip's bus is heard on both sides, the lone
/// centre at unity into each (the engine's mono-to-stereo fold), not on the left
/// alone.
#[test]
fn a_mono_source_reaches_both_sides_of_a_stereo_strip() {
    let mut s = offline(Session::builder(), 2);
    let g = s.graph();
    let src = g.add(Unforkable(Src {
        values: vec![0.5],
        out: BusTopology::MONO,
    }));
    let strip = g.add(BusStripNode::from(BusStripConfig::new(BusTopology::STEREO)));
    g.connect(src.bus_output(0), strip.bus_input(0))
        .expect("a mono bus folds onto a stereo one")
        .unfaded();
    for c in 0..2 {
        g.set_sources(MainOutput::channel(c), [OutputPort::new(strip, c)])
            .unwrap()
            .unfaded();
    }
    assert_eq!(heard(&mut s, 2), [0.5, 0.5]);
}

/// A stereo strip's bus folds onto a declared 5.1 main output: its pair lands on the
/// front left and right, and the other four speakers are silent.
#[test]
fn a_stereo_strip_feeds_a_declared_surround_main_output() {
    let five_one = ChannelTopology::default_for(ChannelLayout::from(6u16));
    let mut s = offline(Session::builder().main_output_topology(five_one), 6);
    let g = s.graph();
    let src = g.add(Unforkable(Src {
        values: vec![0.25, 0.75],
        out: BusTopology::STEREO,
    }));
    let strip = g.add(BusStripNode::from(BusStripConfig::new(BusTopology::STEREO)));
    g.connect(src, strip).expect("same width").unfaded();
    g.connect(strip.bus_output(0), MainOutput)
        .expect("a placed stereo bus folds onto 5.1")
        .unfaded();
    assert_eq!(heard(&mut s, 6), [0.25, 0.75, 0.0, 0.0, 0.0, 0.0]);
}

/// A balance is addressable only where a strip has a left/right pair: a stereo
/// strip takes it, and a mono strip refuses it by name and writes nothing.
#[test]
fn a_strip_without_a_pair_refuses_a_balance_by_name() {
    let mut s = offline(Session::builder(), 2);
    let stereo = s
        .graph()
        .add(BusStripNode::from(BusStripConfig::new(BusTopology::STEREO)));
    let mono = s
        .graph()
        .add(BusStripNode::from(BusStripConfig::new(BusTopology::MONO)));
    s.set(stereo.balance().to(Balance::new(0.5)))
        .expect("a stereo strip balances");
    assert_eq!(
        s.set(mono.balance().to(Balance::new(0.5)))
            .unwrap_err()
            .kind,
        ValueErrorKind::NoSuchTarget
    );
}
