//! Float to integer conversion, the rule every integer output and export runs:
//! `convert_counting_clips` over one interleaved stereo run, from `f32` and
//! `f64` into 16-, 24- and 32-bit integers. The source is a sine at a level
//! past full scale, so the run holds in-range values, ties and clips.
//!
//! ```text
//! cargo bench -p tutti-buffers --bench convert
//! ```

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tutti_buffers::{
    ChannelLayout, I24, Interleaved, InterleavedMut, Sample, convert_counting_clips,
};

/// Frames in one run: a long device block or an export chunk.
const FRAMES: usize = 8192;

/// The source: a stereo sine at `1.25` of full scale.
fn source<S: Sample>() -> Vec<S> {
    (0..FRAMES * 2)
        .map(|i| S::from_f64(1.25 * (i as f64 * 0.013).sin()))
        .collect()
}

fn bench_one<A: Sample, B: Sample>(c: &mut Criterion, name: &str) {
    let src = source::<A>();
    let mut dst = vec![B::SILENCE; src.len()];
    let mut group = c.benchmark_group("convert");
    group.throughput(Throughput::Elements(src.len() as u64));
    group.bench_function(name, |b| {
        b.iter(|| {
            let src = Interleaved::new(black_box(&src), ChannelLayout::STEREO).unwrap();
            let dst = InterleavedMut::new(&mut dst, ChannelLayout::STEREO).unwrap();
            black_box(convert_counting_clips(src, dst).unwrap())
        });
    });
    group.finish();
}

fn convert(c: &mut Criterion) {
    bench_one::<f32, i16>(c, "f32_to_i16");
    bench_one::<f32, I24>(c, "f32_to_i24");
    bench_one::<f32, i32>(c, "f32_to_i32");
    bench_one::<f64, i16>(c, "f64_to_i16");
    bench_one::<f64, I24>(c, "f64_to_i24");
}

criterion_group!(benches, convert);
criterion_main!(benches);
