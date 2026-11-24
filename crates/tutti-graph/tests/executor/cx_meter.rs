//! `Cx::meter` and `Cx::tempo`: the project's meter and tempo map, lent by the
//! host for a block, reach every node, on the one-participant executor and on every
//! participant of a pool.
//!
//! Each meter probe writes the beats per bar of the meter it reads at beat 9,
//! or -1 when the block has none. Under 4/4 then 7/8 from beat 8 that is 7.
//! Each tempo probe writes the map's tempo at beat 9 in BPM, or -1 when the
//! block has none.

use tutti_graph::testing as common;

use common::{WORKERS, test_pool};
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_graph::{
    Cx, Editor, Io, Node, NodeKey, OutputPort, PortSource, Prepare, Shape, Status,
    TransportChanges, TransportPosition, Unforkable,
};
use tutti_music::{Meter, MeterChange, MeterMap, TimeSignature};
use tutti_tempo::{TempoMap, TempoPoint, Transition};
use tutti_units::{Beat, Bpm, SampleRate, Samples};

const MAX_BLOCK: usize = 64;
const PROBES: u64 = 24;

/// A node writing what `read` makes of its `Cx`.
struct Probe(fn(&Cx<'_>) -> f32);

fn beats_per_bar(cx: &Cx<'_>) -> f32 {
    cx.meter
        .map_or(-1.0, |m| m.at(Beat::new(9.0)).beats_per_bar().get() as f32)
}

fn tempo_at_beat_9(cx: &Cx<'_>) -> f32 {
    cx.tempo
        .map_or(-1.0, |t| t.tempo_at(Beat::new(9.0)).get() as f32)
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill((self.0)(cx));
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// One block of `PROBES` probes of `read` on `workers` participants (one-participant
/// for one), rendered with `meter` and `tempo`: each probe's output.
fn render(
    workers: usize,
    read: fn(&Cx<'_>) -> f32,
    meter: Option<&MeterMap>,
    tempo: Option<&TempoMap>,
) -> Vec<Vec<f32>> {
    let (mut ed, mut exec) = Editor::new(Prepare::new(
        SampleRate::SR_48K,
        MaxBlock::of(Samples::new(MAX_BLOCK)),
    ));
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
    for k in 1..=PROBES {
        ed.insert(NodeKey(k), "probe", Unforkable(Probe(read)));
    }
    ed.wire_outputs((1..=PROBES).map(|k| PortSource::Node(OutputPort::new(NodeKey(k), 0))))
        .unwrap();
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
        tempo,
    );
    chans
}

/// **A node reads the meter the block was lent, with no pool or on a pool**, and
/// none when the host lends none.
#[test]
fn a_node_reads_the_meter_the_block_was_lent() {
    let map = MeterMap::new([
        MeterChange::new(Beat::new(0.0), TimeSignature::of(4, 4)),
        MeterChange::new(Beat::new(8.0), TimeSignature::of(7, 8)),
    ]);
    for workers in std::iter::once(1).chain(WORKERS) {
        for (meter, want) in [(Some(&map), 7.0), (None, -1.0)] {
            for (p, chan) in render(workers, beats_per_bar, meter, None)
                .iter()
                .enumerate()
            {
                assert!(
                    chan.iter().all(|&x| x == want),
                    "probe {p}, {workers} workers: wanted {want}, read {:?}",
                    &chan[..2]
                );
            }
        }
    }
}

/// **A node reads the tempo map the block was lent, with no pool or on a pool**, and
/// none when the host lends none. 120 BPM, then 90 from beat 8: 90 at beat 9.
#[test]
fn a_node_reads_the_tempo_map_the_block_was_lent() {
    let map = TempoMap::new([
        TempoPoint {
            beat: Beat::new(0.0),
            tempo: Bpm::of(120.0),
            transition: Transition::Step,
        },
        TempoPoint {
            beat: Beat::new(8.0),
            tempo: Bpm::of(90.0),
            transition: Transition::Step,
        },
    ])
    .expect("a map");
    for workers in std::iter::once(1).chain(WORKERS) {
        for (tempo, want) in [(Some(&map), 90.0), (None, -1.0)] {
            for (p, chan) in render(workers, tempo_at_beat_9, None, tempo)
                .iter()
                .enumerate()
            {
                assert!(
                    chan.iter().all(|&x| x == want),
                    "probe {p}, {workers} workers: wanted {want}, read {:?}",
                    &chan[..2]
                );
            }
        }
    }
}
