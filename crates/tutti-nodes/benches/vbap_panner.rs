//! **What does one block of VBAP panning cost, at 2 and 6 speakers, for a
//! 64-frame and a 1024-frame block?**
//!
//! A 64-frame block at 48 kHz has 1.333 ms (1024 frames: 21.3 ms); divide
//! a case's time by that for the fraction of the budget one panned source
//! consumes.
//!
//! `vbap_static` holds the source still, warmed up for two seconds of audio
//! first so the 50 ms bearing ramp has landed (a shorter warmup measures the
//! tail of the initial ramp instead); `vbap_moving` writes a new bearing every
//! block, which keeps the ramp permanently in flight — the case where the
//! gain vector genuinely changes between grid points. Both feed a stereo
//! pair at the default width, the branch that solves two virtual sources.
//!
//! The panner is a graph node, called by hand through
//! `tutti_graph::contract::Direct` (no graph around it, no allocation per
//! block).

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_buffers::{Bed, Float, PlanesMut};
use tutti_graph::ParamNode;
use tutti_graph::contract::Direct;
use tutti_nodes::testing::aim;
use tutti_rt::SplitMix64;
use tutti_units::{ArcDegrees, Azimuth, Elevation, SampleRate};

/// Warmup before measuring, in frames: two seconds at 48 kHz, 40 ramp
/// lengths, so a held source has landed.
const WARMUP_FRAMES: usize = 96_000;

/// Deterministic broadband input from a seeded `SplitMix64`.
fn fill_noise(mut channels: PlanesMut<'_, '_>) {
    let mut rng = SplitMix64::new(1);
    for mut c in channels.iter_mut() {
        for x in c.samples_mut() {
            *x = f32::from_fraction_bits(rng.next_u64()) * 2.0 - 1.0;
        }
    }
}

fn bench(c: &mut Criterion) {
    for moving in [false, true] {
        let name = if moving { "vbap_moving" } else { "vbap_static" };
        let mut g = c.benchmark_group(name);
        for (bed, speakers, block) in [
            (Bed::Stereo, 2u16, 64usize),
            (Bed::Surround51, 6, 64),
            (Bed::Stereo, 2, 1024),
            (Bed::Surround51, 6, 1024),
        ] {
            g.throughput(Throughput::Elements(block as u64));
            let node = vbap::<f32>(bed);
            let params = node.param_set();
            aim(&params, Azimuth::new(20.0), Elevation::LEVEL);
            let mut d = Direct::new(node, SampleRate::SR_48K, block);
            fill_noise(d.inputs_mut());
            for _ in 0..WARMUP_FRAMES / block {
                d.block();
            }
            let mut bearing = Azimuth::FRONT;
            let id = BenchmarkId::new(format!("{speakers}sp"), block);
            g.bench_with_input(id, &speakers, |b, _| {
                b.iter(|| {
                    if moving {
                        bearing = bearing.rotate_by(ArcDegrees::new(7.0));
                        aim(&params, bearing, Elevation::LEVEL);
                    }
                    d.block();
                    black_box(d.output().plane(0).samples()[block - 1]);
                })
            });
        }
        g.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);

/// A panner over `bed`.
fn vbap<F: tutti_buffers::Float>(bed: tutti_buffers::Bed) -> tutti_nodes::VbapPannerNode<F> {
    let bed = tutti_dsp::BedLayout::try_from(bed).expect("a bed with speakers to pan between");
    tutti_nodes::VbapPannerNode::from(tutti_nodes::VbapPannerConfig::new(bed))
}
