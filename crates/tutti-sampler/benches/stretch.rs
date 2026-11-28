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
use tutti_sampler::testing::{MockTransport, placed_source};
use tutti_sampler::{Playback, SlotId, VoiceNode, VoicePool, VoiceWindow};
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
    let source = placed_source(Arc::new(wave), VoiceWindow::from(Beat::new(0.0)));
    let play = Playback {
        stretch: StretchFactor::new(1.5),
        pitch: Cents::new(200.0),
        ..Playback::default()
    };
    let node = VoiceNode::<f32>::new(source, play).expect("a finite gain and a nonempty width");
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

/// The worst block while `voices` stretching voices (all seated alike, so their analysis
/// frames fall on the same blocks) reseat together, in blocks of `block` frames.
///
/// Each iteration seeks the transport (every voice unseats and primes) and runs 24
/// blocks, timing each; the figure is the slowest of them. A bench, not a test: it is
/// the cost a block bears, not a limit.
fn reseat_case(c: &mut Criterion, name: &str, voices: u128, block: usize) {
    let mut wave = Wave::zero(2, RATE, Seconds::new(4.0));
    for i in 0..wave.len().get() {
        let s = ((TAU * 440.0 * i as f64 / RATE.get()).sin() * 0.5) as f32;
        wave.set(0, i, s);
        wave.set(1, i, s);
    }
    let wave = Arc::new(wave);
    let (pool, handle) = VoicePool::<f32>::new().with_handle();
    let mut direct = Direct::new(pool, RATE, block);
    for id in 0..voices {
        let source = placed_source(Arc::clone(&wave), VoiceWindow::from(Beat::new(0.0)));
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
    let mut run = move || {
        let env = transport.env(block, RATE);
        direct.block_in(&env);
        transport.advance(block as i64, RATE);
    };
    let mut group = c.benchmark_group("stretch");
    group.bench_function(name, |b| {
        b.iter_custom(|iterations| {
            let mut total = std::time::Duration::ZERO;
            for _ in 0..iterations {
                seeker.seek(Beat::new(0.0));
                let mut worst = std::time::Duration::ZERO;
                for _ in 0..24 {
                    let start = tutti_platform::Instant::now();
                    run();
                    worst = worst.max(start.elapsed());
                }
                total += worst;
            }
            total
        });
    });
    group.finish();
}

/// One stereo voice reseating in 64-frame blocks (the bar: under 1.33 ms), and 64
/// voices reseating together in 256-frame blocks (under 5.33 ms).
fn reseat(c: &mut Criterion) {
    reseat_case(c, "reseat_1_voice/64", 1, 64);
    reseat_case(c, "reseat_64_voices/256", 64, 256);
    reseat_case(c, "reseat_64_voices/64", 64, 64);
}

/// What `PhaseVocoder::reset` costs for 64 stereo vocoders on the voice's geometry: the
/// share of a reseating block that is only the reset.
fn reset(c: &mut Criterion) {
    use tutti_buffers::ChannelLayout;
    use tutti_dsp::{ColaGeometry, HopSize, PhaseVocoder, StftGeometry, WindowLength};
    use tutti_units::Samples;
    let geometry = StftGeometry::new(
        RATE,
        WindowLength::new(Samples::new(2048)).unwrap(),
        HopSize::new(Samples::new(512)).unwrap(),
    );
    let mut vocoders: Vec<PhaseVocoder<f32>> = (0..64)
        .map(|_| {
            PhaseVocoder::new(
                ChannelLayout::STEREO,
                ColaGeometry::try_from(geometry).unwrap(),
            )
        })
        .collect();
    let mut group = c.benchmark_group("stretch");
    group.bench_function("reset_64_vocoders", |b| {
        b.iter(|| {
            for v in &mut vocoders {
                v.reset();
            }
            black_box(&vocoders);
        });
    });
    group.finish();
}

criterion_group!(benches, stretch, reseat, reset);
criterion_main!(benches);
