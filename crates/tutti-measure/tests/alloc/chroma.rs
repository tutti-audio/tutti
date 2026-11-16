//! A chroma step allocates nothing once its state holds the buffers.
//!
//! The crate carries no `assert_no_alloc` dev-dependency (see its Cargo.toml),
//! so this binary counts allocations with its own global allocator. It counts
//! per thread, so the harness's other threads do not show up in a count.

use tutti_buffers::Mono;
use tutti_fixtures::allocations;
use tutti_measure::{ChromaConfig, Framing, Tuning, TuningOffset};
use tutti_types::{Cents, SampleRate, Samples};

/// After one push has built the buffers and planned the FFT, further pushes
/// and a `reset` allocate nothing, and the pushed frame count is the same as a
/// fresh meter's.
#[test]
fn pushes_after_the_first_do_not_allocate() {
    let cfg = ChromaConfig::new().framing(Framing::new(Samples(2048), Samples(512)));
    let fixed = cfg.tuning(Tuning::Fixed(TuningOffset::of(Cents(10.0))));
    let frame: Vec<f32> = (0..2048)
        .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / 22_050.0).sin() * 0.5)
        .collect();
    let rate = SampleRate::SR_22K05;
    let mut meter = None;
    let built = allocations(|| meter = Some(cfg.measure(rate).unwrap()));
    assert!(built > 0, "construction builds the buffers");
    let mut meter = meter.unwrap();

    // The first push sizes the sample buffer and plans the transform.
    let first = allocations(|| {
        meter.push(Mono::new(&frame)).unwrap();
    });
    assert!(first > 0, "the first push builds the buffers");

    let again = allocations(|| {
        meter.push(Mono::new(&frame)).unwrap();
    });
    assert_eq!(again, 0, "a second push");

    let reset = allocations(|| {
        meter.reset();
        meter.push(Mono::new(&frame)).unwrap();
    });
    assert_eq!(reset, 0, "reset and a push");

    // A meter of another tuning is its own meter: it builds its own buffers.
    let mut other = fixed.measure(rate).unwrap();
    let rebuilt = allocations(|| {
        other.push(Mono::new(&frame)).unwrap();
    });
    assert!(rebuilt > 0, "a new meter builds its own");
}
