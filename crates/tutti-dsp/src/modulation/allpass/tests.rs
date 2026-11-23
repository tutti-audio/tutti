use super::*;

/// The sweep at the LFO's trough, centre and peak, for depth 0, half and 1,
/// from the definition `min + (max - min) * (lfo + 1) / 2 * depth`.
#[test]
fn the_sweep_runs_from_min_to_max_by_lfo_and_depth() {
    let (lo, hi) = (Hz::new(200.0), Hz::new(4_000.0));
    let at =
        |lfo: f64, depth: f64| sweep_hz(lo, hi, Bipolar::clamped(lfo), Bipolar::of(depth)).get();
    // Depth 1: the whole range.
    assert_eq!(at(-1.0, 1.0), 200.0);
    assert_eq!(at(0.0, 1.0), 2_100.0);
    assert_eq!(at(1.0, 1.0), 4_000.0);
    // Depth 0: parked at the floor whatever the LFO does.
    for lfo in [-1.0, 0.0, 1.0] {
        assert_eq!(at(lfo, 0.0), 200.0);
    }
    // Half depth: half the excursion.
    assert_eq!(at(1.0, 0.5), 2_100.0);
    assert_eq!(at(0.0, 0.5), 1_150.0);
}

/// A zero-width range does not move whatever the LFO and depth.
#[test]
fn an_empty_range_is_a_fixed_frequency() {
    let f = Hz::new(440.0);
    assert_eq!(
        sweep_hz(f, f, Bipolar::clamped(0.7), Bipolar::of(1.0)).get(),
        440.0
    );
}
