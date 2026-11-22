//! A sampler's read of one block from a position ring: four consecutive taps
//! per output frame on each channel, read per tap through
//! [`PosClaim::sample`] or once per channel through
//! [`PosClaim::copy_channel`], then summed as the taps would be.
//!
//! ```text
//! cargo bench -p tutti-rt --bench pos_ring
//! ```

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tutti_buffers::{ChannelLayout, Interleaved};
use tutti_rt::PosRing;

/// Output frames in one block.
const BLOCK: usize = 512;
/// Interleaved channels.
const CHANNELS: usize = 2;
/// Slots in the ring.
const RING: usize = 4096;

fn gather(c: &mut Criterion) {
    let layout = ChannelLayout::clamped(CHANNELS);
    let (mut writer, mut reader) = PosRing::<f32>::new(RING, layout, 0).unwrap();
    let data: Vec<f32> = (0..RING / 2 * CHANNELS).map(|i| i as f32).collect();
    writer
        .push(Interleaved::new(&data, layout).unwrap())
        .unwrap();
    let span = BLOCK + 3;
    let mut scratch = vec![0.0f32; span];
    let mut out = vec![0.0f32; BLOCK];

    let mut group = c.benchmark_group("pos_ring_gather");
    group.throughput(Throughput::Elements((BLOCK * CHANNELS) as u64));
    group.bench_function("per_tap", |b| {
        b.iter(|| {
            let claim = reader.claim(0, [(0, span as u64), (0, 0)]);
            for ch in 0..CHANNELS {
                for (i, o) in out.iter_mut().enumerate() {
                    let s = i as u64;
                    *o = (0..4).map(|t| claim.sample(s + t, ch)).sum();
                }
                black_box(&out);
            }
        });
    });
    group.bench_function("copy_channel", |b| {
        b.iter(|| {
            let claim = reader.claim(0, [(0, span as u64), (0, 0)]);
            for ch in 0..CHANNELS {
                claim.copy_channel(0, ch, &mut scratch);
                for (o, w) in out.iter_mut().zip(scratch.windows(4)) {
                    *o = w.iter().sum();
                }
                black_box(&out);
            }
        });
    });
    group.finish();
}

criterion_group!(benches, gather);
criterion_main!(benches);
