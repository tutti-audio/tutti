//! What does a stretching voice cost per block?
//!
//! A stereo placed `VoiceNode` playing a sine through its phase vocoder,
//! engaged (stretch 1.5, +200 cents), in 512-frame blocks. The vocoder runs one
//! frame per hop, so the time per block is the FFT pair, the per-bin phase work,
//! the overlap-add and the cubic read of the pitch shift, together with the
//! voice's read of the source frames the vocoder asks for. Throughput is in
//! frames.
//!
//! The transport winds back to the start every `WRAP` blocks, which costs the
//! voice one seat (and its primed read) per wrap: well under the noise of the
//! blocks between.

use std::f64::consts::TAU;
use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tutti_buffers::Wave;
use tutti_graph::contract::Direct;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::{
    MemorySource, Playback, SlotId, Voice, VoiceNode, VoicePool, VoiceSource, VoiceWindow,
};
use tutti_units::{Beat, Bpm, Cents, SampleRate, Seconds, StretchFactor};

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 512;

/// Blocks between rewinds: 3.2 s at 48 kHz, inside the 4 s wave.
const WRAP: usize = 300;

fn stretch(c: &mut Criterion) {
    let mut wave = Wave::zero(2, RATE, Seconds::new(4.0));
    for i in 0..wave.len().get() {
        let s = ((TAU * 440.0 * i as f64 / RATE.get()).sin() * 0.5) as f32;
        wave.set(0, i, s);
        wave.set(1, i, s);
    }
    let source = MemorySource::placed(Arc::new(wave), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    let play = Playback {
        stretch: StretchFactor::new(1.5),
        pitch: Cents::new(200.0),
        ..Playback::default()
    };
    let node = VoiceNode::<f32>::new(Voice {
        source: VoiceSource::Memory(source),
        play,
    });
    let mut direct = Direct::new(node, RATE, BLOCK);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut blocks = 0usize;
    let mut block = move || {
        if blocks.is_multiple_of(WRAP) {
            transport.seek(Beat::new(0.0));
        }
        blocks += 1;
        let env = transport.env(BLOCK, RATE);
        direct.block_in(&env);
        transport.advance(BLOCK as i64, RATE);
    };

    let mut group = c.benchmark_group("stretch");
    group.throughput(Throughput::Elements(BLOCK as u64));
    // Past the vocoder's latency, so every measured block runs frames.
    for _ in 0..16 {
        block();
    }
    group.bench_function("voice/1.5x+200c", |b| {
        b.iter(|| {
            block();
            black_box(());
        });
    });
    group.finish();
}

/// The worst block while 64 stretching voices reseat together, in 64-frame blocks.
///
/// Each iteration seeks the transport (every voice unseats and primes, 8 frames a
/// frame, for 586 frames: about 10 blocks) and runs 16 blocks, timing each; the
/// figure is the slowest of them. A bench, not a test: it is the cost a block bears,
/// not a limit.
fn reseat(c: &mut Criterion) {
    const VOICES: u128 = 64;
    const BLOCK: usize = 64;
    let mut wave = Wave::zero(2, RATE, Seconds::new(4.0));
    for i in 0..wave.len().get() {
        let s = ((TAU * 440.0 * i as f64 / RATE.get()).sin() * 0.5) as f32;
        wave.set(0, i, s);
        wave.set(1, i, s);
    }
    let wave = Arc::new(wave);
    let (pool, handle) = VoicePool::<f32>::new().with_handle();
    let mut direct = Direct::new(pool, RATE, BLOCK);
    for id in 0..VOICES {
        let source = MemorySource::placed(Arc::clone(&wave), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave");
        let play = Playback {
            stretch: StretchFactor::new(1.5),
            pitch: Cents::new(200.0),
            ..Playback::default()
        };
        handle
            .add(SlotId(id), source, play)
            .expect("the command queue has room");
    }
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let seeker = Arc::clone(&transport);
    let mut block = move || {
        let env = transport.env(BLOCK, RATE);
        direct.block_in(&env);
        transport.advance(BLOCK as i64, RATE);
    };
    let mut group = c.benchmark_group("stretch");
    group.bench_function("reseat_64_voices/worst_block", |b| {
        b.iter_custom(|iterations| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iterations {
                seeker.seek(Beat::new(0.0));
                let mut worst = std::time::Duration::ZERO;
                for _ in 0..16 {
                    let start = tutti_platform::Instant::now();
                    block();
                    worst = worst.max(start.elapsed());
                }
                total += worst;
            }
            total
        });
    });
    group.finish();
}

criterion_group!(benches, stretch, reseat);
criterion_main!(benches);
