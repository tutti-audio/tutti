use super::*;

const BLOCK: Seconds = Seconds::new(64.0 / 48_000.0);

fn at(load: f64) -> Seconds {
    BLOCK * load
}

/// The first block seeds the mean: it reads exactly that block's load.
#[test]
fn the_first_block_seeds_the_mean() {
    let (mut w, load, _) = link();
    w.record(at(0.2), BLOCK);
    assert!((load.get().get() - 0.2).abs() < 1e-12, "{:?}", load.get());
}

/// After the seed, one block moves the mean by the exact one-pole step for
/// its budget, `1 - exp(-budget / tau)`, whatever the block size.
#[test]
fn the_smoothing_step_is_the_one_pole_coefficient_of_the_budget() {
    for frames in [64.0f64, 1024.0] {
        let block = Seconds::new(frames / 48_000.0);
        let (mut w, load, _) = link();
        w.record(block * 0.2, block);
        w.record(block * 0.8, block);
        let a = 1.0 - (-(block.get()) / LOAD_MEAN_TIME_CONSTANT.get()).exp();
        let want = 0.2 + a * (0.8 - 0.2);
        assert!((load.get().get() - want).abs() < 1e-12, "{frames}");
    }
}

/// Under a constant load the mean converges to it at the time constant:
/// five time constants of audio leave `0.6 * e^-5` of the step, for either
/// block size, and one time constant of audio leaves `0.6 / e` of it.
#[test]
fn the_mean_converges_to_a_constant_load_at_its_time_constant() {
    for frames in [64.0f64, 1024.0] {
        let block = Seconds::new(frames / 48_000.0);
        for (constants, left) in [(5.0, (-5.0f64).exp()), (1.0, (-1.0f64).exp())] {
            let (mut w, load, _) = link();
            w.record(block * 0.2, block);
            let blocks = (constants * (LOAD_MEAN_TIME_CONSTANT / block)).round() as usize;
            for _ in 0..blocks {
                w.record(block * 0.8, block);
            }
            let remaining = 0.8 - load.get().get();
            // Rounding the block count to whole blocks is off by under half
            // a block of the time constant.
            let bound = 0.6 * left * (block / LOAD_MEAN_TIME_CONSTANT);
            assert!(
                (remaining - 0.6 * left).abs() <= bound + 1e-9,
                "{frames} x {constants}: {remaining}"
            );
        }
    }
}

/// A block over budget counts once, one at its budget does not, and every
/// timed block counts in `blocks`.
#[test]
fn only_a_block_past_its_budget_counts_as_over() {
    let (mut w, _, blocks) = link();
    w.record(at(0.5), BLOCK);
    w.record(at(1.0), BLOCK);
    w.record(at(2.5), BLOCK);
    let r = blocks.get();
    assert_eq!((r.blocks, r.over_budget), (3, 1));
}

/// A block with no budget records nothing: not a block, not a load.
#[test]
fn a_block_with_no_budget_records_nothing() {
    let (mut w, load, blocks) = link();
    w.record(Seconds::new(1.0), Seconds::new(0.0));
    assert_eq!(blocks.get(), BlockCounts::default());
    assert_eq!(load.get(), Load::default());
}

/// A NaN or infinite elapsed time leaves the mean finite, after it and after
/// the next block; an infinite one counts over budget.
#[test]
fn a_nonfinite_elapsed_time_leaves_the_mean_finite() {
    let (mut w, load, blocks) = link();
    w.record(at(0.5), BLOCK);
    w.record(Seconds::new(f64::NAN), BLOCK);
    assert!(load.get().get().is_finite());
    w.record(Seconds::new(f64::INFINITY), BLOCK);
    assert!(load.get().get().is_finite(), "{:?}", load.get());
    w.record(at(0.5), BLOCK);
    assert!(load.get().get().is_finite(), "{:?}", load.get());
    let r = blocks.get();
    assert_eq!((r.blocks, r.over_budget), (4, 1));
}

/// A clone of the gauge reads the value the original reads.
#[test]
fn a_gauge_clone_reads_the_same_load() {
    let (mut w, load, _) = link();
    let clone = load.clone();
    w.record(at(0.75), BLOCK);
    assert_eq!(clone.get(), load.get());
    assert!((clone.get().get() - 0.75).abs() < 1e-12);
}

/// Each tally only grows across reads, and a read resets nothing.
#[test]
fn the_counts_only_grow_across_reads() {
    let (mut w, _, blocks) = link();
    let mut last = blocks.get();
    for i in 0..8u64 {
        w.record(at(0.5 + 0.25 * (i % 4) as f64), BLOCK);
        w.silenced();
        w.dropped_events(i);
        w.truncated();
        let now = blocks.get();
        assert!(now.blocks > last.blocks && now.silenced > last.silenced);
        assert!(now.over_budget >= last.over_budget);
        assert!(now.dropped_events >= last.dropped_events);
        assert_eq!(blocks.get(), now, "a read leaves the counts");
        last = now;
    }
    assert_eq!(
        last,
        BlockCounts {
            blocks: 8,
            over_budget: 2,
            silenced: 8,
            dropped_events: 28,
            truncated: 8,
        }
    );
}

/// A call's budget is its whole frames over the rate, however many graph
/// blocks it splits into: 8192 frames in two blocks of 4096 at 48 kHz last
/// 8192/48000 s, not one block's 4096/48000 s.
#[test]
fn the_budget_is_the_whole_calls_frames_over_the_rate() {
    let mut budget = Budget::new();
    budget.add(Samples::new(4096), SampleRate::SR_48K);
    budget.add(Samples::new(4096), SampleRate::SR_48K);
    assert!((budget.seconds().get() - 8192.0 / 48_000.0).abs() < 1e-12);
}

/// A call that straddles a rate change is held to each piece's own rate.
#[test]
fn a_rate_change_mid_call_budgets_each_piece_at_its_rate() {
    let mut budget = Budget::new();
    budget.add(Samples::new(480), SampleRate::SR_48K);
    budget.add(Samples::new(960), SampleRate::SR_96K);
    assert!((budget.seconds().get() - (480.0 / 48_000.0 + 960.0 / 96_000.0)).abs() < 1e-12);
}
