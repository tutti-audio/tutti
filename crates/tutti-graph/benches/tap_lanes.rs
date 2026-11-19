//! A tap's lanes: the executor-side write and the reader-side read of one
//! 512-frame block of two channels, the cost the SPSC rings under a tap add
//! to every audio block.
//!
//! `write` is `TapWriter::push_interleaved` (a block of interleaved frames
//! split into the lanes), `read` is `TapReader::pop_into` (the lanes
//! interleaved back out), and `round_trip` is both, which keeps the rings
//! near empty as a drained tap does.
//!
//! ```text
//! cargo bench -p tutti-graph --bench tap_lanes
//! ```

use std::hint::black_box;
use std::time::Duration;
use tutti_buffers::{Interleaved, InterleavedMut};
use tutti_platform::Instant;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tutti_graph::TapReader;
use tutti_units::Samples;

const FRAMES: usize = 512;
const WIDTH: usize = 2;

fn tap_lanes(c: &mut Criterion) {
    let block: Vec<f32> = (0..FRAMES * WIDTH).map(|i| i as f32 * 1e-3).collect();
    let mut out = vec![0.0f32; FRAMES * WIDTH];
    let mut group = c.benchmark_group("tap_lanes");
    group.throughput(Throughput::Elements((FRAMES * WIDTH) as u64));

    // Two blocks of room: a write finds space and a read finds a block.
    let capacity = Samples::new(FRAMES * 4);

    group.bench_function("write_512x2", |b| {
        let (mut reader, mut writer) = TapReader::pair(WIDTH, capacity);
        b.iter_custom(|iters| {
            let mut spent = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                writer.push_interleaved(
                    Interleaved::new(black_box(&block), writer.layout()).unwrap(),
                );
                spent += start.elapsed();
                black_box(
                    reader
                        .pop_into(InterleavedMut::new(&mut out, reader.layout()).unwrap())
                        .unwrap(),
                );
            }
            spent
        });
    });

    group.bench_function("read_512x2", |b| {
        let (mut reader, mut writer) = TapReader::pair(WIDTH, capacity);
        b.iter_custom(|iters| {
            let mut spent = Duration::ZERO;
            for _ in 0..iters {
                writer.push_interleaved(Interleaved::new(&block, writer.layout()).unwrap());
                let start = Instant::now();
                black_box(
                    reader
                        .pop_into(
                            InterleavedMut::new(black_box(&mut out), reader.layout()).unwrap(),
                        )
                        .unwrap(),
                );
                spent += start.elapsed();
            }
            spent
        });
    });

    group.bench_function("round_trip_512x2", |b| {
        let (mut reader, mut writer) = TapReader::pair(WIDTH, capacity);
        b.iter(|| {
            writer.push_interleaved(Interleaved::new(black_box(&block), writer.layout()).unwrap());
            black_box(
                reader
                    .pop_into(InterleavedMut::new(black_box(&mut out), reader.layout()).unwrap())
                    .unwrap(),
            );
        });
    });
    group.finish();
}

criterion_group!(benches, tap_lanes);
criterion_main!(benches);
