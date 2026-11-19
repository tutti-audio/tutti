//! **What do the FFT-based analyses cost on a few seconds of audio?**
//!
//! Times `stft` plus `istft`, `detect_onsets` (spectral flux) and `yin_track`
//! over 4 s of a harmonic tone with a little noise at 48 kHz. These are the
//! three callers of the crate's real-input transforms.
//!
//! ```text
//! cargo bench -p tutti-measure --bench fft_analyses
//! ```

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tutti_buffers::Mono;
use tutti_measure::PitchHop;
use tutti_measure::{
    DetectionFunction, OnsetConfig, SampleRange, StftConfig, YinConfig, detect_onsets,
    istft_transform, stft,
};
use tutti_units::{SampleRate, Samples};

const SR: SampleRate = SampleRate::SR_48K;

fn signal() -> Vec<f32> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..4 * 48_000)
        .map(|i| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let noise = ((state >> 33) as f32 / (1u64 << 31) as f32 - 1.0) * 0.02;
            let t = i as f32 / 48_000.0;
            let burst = if (i / 12_000) % 2 == 0 { 1.0 } else { 0.4 };
            burst
                * (0.5 * (std::f32::consts::TAU * 220.0 * t).sin()
                    + 0.25 * (std::f32::consts::TAU * 440.0 * t).sin())
                + noise
        })
        .collect()
}

fn benches(c: &mut Criterion) {
    let x = signal();

    let mut g = c.benchmark_group("fft_analyses");
    g.sample_size(30);

    g.bench_function("stft_istft_2048_hop512", |b| {
        let req = StftConfig::new(Samples::new(2048), Samples::new(512));
        b.iter(|| {
            let s = stft(&req, Mono::new(black_box(&x)), SampleRange::All, SR).unwrap();
            black_box(istft_transform::<f32>(&s))
        });
    });

    g.bench_function("onsets_flux_2048_hop512", |b| {
        let cfg = OnsetConfig::new(DetectionFunction::SpectralFlux);
        b.iter(|| black_box(detect_onsets(&cfg, Mono::new(black_box(&x)), SR).unwrap()));
    });

    g.bench_function("yin_track_hop512", |b| {
        let cfg = YinConfig::new();
        let hop = PitchHop::of(Samples::new(512));
        b.iter(|| {
            black_box(tutti_measure::yin_track(&cfg, Mono::new(black_box(&x)), SR, hop).unwrap())
        });
    });

    g.finish();
}

criterion_group!(fft_analyses, benches);
criterion_main!(fft_analyses);
