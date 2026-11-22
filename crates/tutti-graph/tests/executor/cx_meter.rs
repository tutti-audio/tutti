//! `Cx::meter`: the project's meter, lent by the host for a block, reaches
//! every node, on the serial executor and on every participant of a pool.
//!
//! Each probe writes the beats per bar of the meter it reads at beat 9, or
//! -1 when the block has none. Under 4/4 then 7/8 from beat 8 that is 7.

use tutti_graph::testing as common;

use common::{WORKERS, test_pool};
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_graph::{
    Cx, Editor, Io, Node, NodeKey, OutputPort, PortSource, Prepare, Shape, Status,
    TransportChanges, TransportPosition, Unforkable,
};
use tutti_music::{Meter, MeterChange, MeterMap, TimeSignature};
use tutti_units::{Beat, SampleRate, Samples};

const MAX_BLOCK: usize = 64;
const PROBES: u64 = 24;

struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let seen = cx
            .meter
            .map_or(-1.0, |m| m.at(Beat::new(9.0)).beats_per_bar().get() as f32);
        io.output(0).fill(seen);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// One block of `PROBES` probes on `workers` participants (serial for one),
/// rendered with `meter`: each probe's output.
fn render(workers: usize, meter: Option<&MeterMap>) -> Vec<Vec<f32>> {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(MAX_BLOCK)),
    ));
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
    for k in 1..=PROBES {
        ed.insert(NodeKey(k), "probe", Unforkable(Probe));
    }
    ed.spec_mut()
        .topology
        .set_outputs((1..=PROBES).map(|k| PortSource::Node(OutputPort::new(NodeKey(k), 0))));
    ed.commit().expect("commits");
    exec.apply_pending();
    assert_eq!(exec.is_parallel(), workers > 1, "{workers} workers");
    let mut chans = vec![vec![0.0f32; MAX_BLOCK]; PROBES as usize];
    let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
    exec.render_applied(
        MAX_BLOCK,
        &TransportPosition::default(),
        &TransportChanges::NONE,
        &[],
        &mut outs,
        meter,
    );
    chans
}

/// **A node reads the meter the block was lent, serial or on a pool**, and
/// none when the host lends none.
#[test]
fn a_node_reads_the_meter_the_block_was_lent() {
    let map = MeterMap::new([
        MeterChange::new(Beat::new(0.0), TimeSignature::of(4, 4)),
        MeterChange::new(Beat::new(8.0), TimeSignature::of(7, 8)),
    ]);
    for workers in std::iter::once(1).chain(WORKERS) {
        for (meter, want) in [(Some(&map), 7.0), (None, -1.0)] {
            for (p, chan) in render(workers, meter).iter().enumerate() {
                assert!(
                    chan.iter().all(|&x| x == want),
                    "probe {p}, {workers} workers: wanted {want}, read {:?}",
                    &chan[..2]
                );
            }
        }
    }
}
