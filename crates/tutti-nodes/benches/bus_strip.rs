//! **What does one `BusStripNode` block cost, steady, ramping and fed?**
//!
//! Each case drives one strip through `process` with whole 128-frame blocks
//! (`tutti_graph::contract::Direct`), over broadband noise:
//!
//! - `steady`: the controls hold, so the block applies one gain per channel.
//! - `ramp`: the volume alternates between two values every block, so every
//!   block is a linear ramp.
//! - `fed`: the volume is fed per frame on its param port, as modulation
//!   does.
//!
//! `reference` is the floor a ramping stereo strip is measured against: a
//! hand-written channel-major gain ramp (`x · (a·(1−t) + b·t)`) and nothing
//! else. `empty` is the harness alone (a node that writes nothing); subtract
//! it from every row.
//!
//! ```text
//! cargo bench -p tutti-nodes --bench bus_strip
//! ```

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_buffers::{BusTopology, ChannelLayout, Float};
use tutti_graph::contract::Direct;
use tutti_graph::{Cx, Io, Node, ParamNode, Prepare, Shape, Status};
use tutti_nodes::{BusStripConfig, BusStripNode};
use tutti_rt::SplitMix64;
use tutti_units::{Amplitude, SampleRate};

const BLOCK: usize = 128;
const SR: SampleRate = SampleRate::SR_48K;

/// Broadband input from a seeded `SplitMix64`.
fn fill_noise<N: Node>(d: &mut Direct<N>) {
    let mut rng = SplitMix64::new(1);
    for ch in d.inputs_mut().iter_mut() {
        let ch = ch.into_samples_mut();
        for s in ch.iter_mut() {
            *s = f32::from_fraction_bits(rng.next_u64()) * 2.0 - 1.0;
        }
    }
}

/// The floor: a stereo gain ramp from 0.5 to 0.75, channel-major.
struct Reference;

/// The harness alone: a stereo node that writes nothing, so its time is
/// what `Direct` costs a block.
struct Empty;

impl Node for Empty {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::STEREO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Modified
    }

    fn reset(&mut self) {}
}

impl Node for Reference {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::STEREO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let n = io.frames();
        let (inputs, mut outputs) = io.split();
        for c in 0..2 {
            let (x, y) = (inputs.get(c), outputs.get(c));
            for (i, (y, x)) in y[..n].iter_mut().zip(&x[..n]).enumerate() {
                let t = (i + 1) as i32 as f32 / n as f32;
                *y = x * (0.5 * (1.0 - t) + 0.75 * t);
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

fn strip(c: &mut Criterion) {
    let mut g = c.benchmark_group("bus_strip");
    for width in [2u16, 6] {
        g.throughput(Throughput::Elements((BLOCK * width as usize) as u64));

        let mut d = Direct::new(
            BusStripNode::from(BusStripConfig::new(
                BusTopology::assumed(ChannelLayout::from(width)).expect("a bench width fits a bus"),
            )),
            SR,
            BLOCK,
        );
        fill_noise(&mut d);
        d.node.volume().store(Amplitude::new(0.5));
        assert!(ParamNode::<f32>::param_set(&*d.node).set(tutti_units::UnitParam::Balance, 0.25));
        d.block();
        g.bench_with_input(BenchmarkId::new("steady", width), &width, |b, _| {
            b.iter(|| {
                d.block();
                black_box(d.output().plane(0).samples()[BLOCK - 1]);
            })
        });

        let mut flip = false;
        g.bench_with_input(BenchmarkId::new("ramp", width), &width, |b, _| {
            b.iter(|| {
                flip = !flip;
                d.node
                    .volume()
                    .store(Amplitude::new(if flip { 0.75 } else { 0.5 }));
                d.block();
                black_box(d.output().plane(0).samples()[BLOCK - 1]);
            })
        });

        let sweep: Vec<f32> = (0..BLOCK).map(|i| 0.25 + i as f32 / BLOCK as f32).collect();
        d.feed(0, Some(&sweep));
        d.block();
        g.bench_with_input(BenchmarkId::new("fed", width), &width, |b, _| {
            b.iter(|| {
                d.block();
                black_box(d.output().plane(0).samples()[BLOCK - 1]);
            })
        });
    }

    g.throughput(Throughput::Elements((BLOCK * 2) as u64));
    let mut d = Direct::new(Reference, SR, BLOCK);
    fill_noise(&mut d);
    g.bench_function(BenchmarkId::new("reference", 2), |b| {
        b.iter(|| {
            d.block();
            black_box(d.output().plane(0).samples()[BLOCK - 1]);
        })
    });

    let mut d = Direct::new(Empty, SR, BLOCK);
    g.bench_function(BenchmarkId::new("empty", 2), |b| {
        b.iter(|| {
            d.block();
            black_box(d.output().plane(0).samples()[BLOCK - 1]);
        })
    });
    g.finish();
}

criterion_group!(benches, strip);
criterion_main!(benches);
