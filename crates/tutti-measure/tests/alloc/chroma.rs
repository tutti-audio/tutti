//! A chroma push allocates nothing once its meter is made.
//!
//! The crate carries no `assert_no_alloc` dev-dependency (see its Cargo.toml),
//! so this binary counts allocations with its own global allocator. It counts
//! per thread, so the harness's other threads do not show up in a count.

use super::Discard;
use tutti_buffers::Mono;
use tutti_fixtures::allocations;
use tutti_measure::{ChromaConfig, Framing, Measure, StreamMeter};
use tutti_units::{SampleRate, Samples};

/// Construction builds the buffers and plans the FFT, so every push, the first
/// included, and a `reset` allocate nothing.
#[test]
fn no_push_allocates() {
    let cfg = ChromaConfig::new().framing(Framing::new(Samples::new(2048), Samples::new(512)));
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
        meter.push(Mono::new(&frame), &mut Discard).unwrap();
    });
    assert_eq!(first, 0, "the first push");

    let again = allocations(|| {
        meter.push(Mono::new(&frame), &mut Discard).unwrap();
    });
    assert_eq!(again, 0, "a second push");

    let reset = allocations(|| {
        meter.reset();
        meter.push(Mono::new(&frame), &mut Discard).unwrap();
    });
    assert_eq!(reset, 0, "reset and a push");
}

/// A push of 16-bit audio converts block by block on the stack: it allocates
/// nothing either, for a chunk longer than the conversion block.
#[test]
fn an_integer_push_allocates_nothing() {
    let cfg = ChromaConfig::new().framing(Framing::new(Samples::new(2048), Samples::new(512)));
    let long: Vec<i16> = (0..20_000)
        .map(|i| ((std::f32::consts::TAU * 440.0 * i as f32 / 22_050.0).sin() * 8_000.0) as i16)
        .collect();
    let mut meter = cfg.measure(SampleRate::SR_22K05).unwrap();
    let pushed = allocations(|| meter.push(Mono::new(&long), &mut Discard).unwrap());
    assert_eq!(pushed, 0, "an i16 push");
}
