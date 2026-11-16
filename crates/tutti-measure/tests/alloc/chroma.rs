//! A chroma push allocates nothing once its meter is made.
//!
//! The crate carries no `assert_no_alloc` dev-dependency (see its Cargo.toml),
//! so this binary counts allocations with its own global allocator. It counts
//! per thread, so the harness's other threads do not show up in a count.

use tutti_buffers::Mono;
use tutti_fixtures::allocations;
use tutti_measure::{ChromaConfig, Framing};
use tutti_types::{SampleRate, Samples};

/// Construction builds the buffers and plans the FFT, so every push, the first
/// included, and a `reset` allocate nothing.
#[test]
fn no_push_allocates() {
    let cfg = ChromaConfig::new().framing(Framing::new(Samples(2048), Samples(512)));
    let frame: Vec<f32> = (0..2048)
        .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / 22_050.0).sin() * 0.5)
        .collect();
    let rate = SampleRate::SR_22K05;
    let mut meter = None;
    let built = allocations(|| meter = Some(cfg.measure(rate).unwrap()));
    assert!(built > 0, "construction builds the buffers");
    let mut meter = meter.unwrap();

    // Construction planned the transform, so even the first push allocates
    // nothing.
    let first = allocations(|| {
        meter.push(Mono::new(&frame)).unwrap();
    });
    assert_eq!(first, 0, "the first push");

    let again = allocations(|| {
        meter.push(Mono::new(&frame)).unwrap();
    });
    assert_eq!(again, 0, "a second push");

    let reset = allocations(|| {
        meter.reset();
        meter.push(Mono::new(&frame)).unwrap();
    });
    assert_eq!(reset, 0, "reset and a push");
}
