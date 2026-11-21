use super::*;
use crate::test_support::{RATE, render_f64};
use core::num::NonZeroU16;
use tutti_buffers::{Mono, Planes};
use tutti_graph::contract::{Direct, assert_param_fork, drive};

/// A table of one frame per slice of `frames`, in `F`.
fn table<F: Float>(frames: &[&[F]]) -> Arc<Wavetable<F>> {
    let views: Vec<Mono<'_, F>> = frames.iter().map(|f| Mono::new(f)).collect();
    Wavetable::new(&views).expect("a table")
}

/// One cycle of a sine in `N = 2048` samples.
fn sine_cycle<F: Float>() -> Vec<F> {
    (0..2048)
        .map(|i| F::from_design(libm::sin(core::f64::consts::TAU * f64::from(i) / 2048.0)))
        .collect()
}

fn sine_node(frequency: f64) -> WavetableNode<f32> {
    WavetableNode::from(
        WavetableConfig::new(NonZeroU16::MIN, table(&[&sine_cycle::<f32>()]))
            .frequency(Hz::new(frequency)),
    )
}

/// Two frames of a constant, `0.25` and `0.75`: every level of each is its constant.
fn two_levels_node() -> WavetableNode<f32> {
    WavetableNode::from(WavetableConfig::new(
        NonZeroU16::MIN,
        table(&[&[0.25f32; 16][..], &[0.75f32; 16][..]]),
    ))
}

fn render(node: WavetableNode<f32>, frames: usize) -> Vec<f32> {
    let mut d = Direct::new(node, RATE, frames);
    d.block();
    d.output().plane(0).samples().to_vec()
}

#[test]
fn a_fork_shares_no_cell_and_shares_the_table() {
    let node = sine_node(220.0);
    assert_param_fork(node.clone());
    let table = Arc::clone(node.wave.table());
    let before = Arc::strong_count(&table);
    let fork = ParamNode::<f32>::fork_fresh(&node);
    assert_eq!(Arc::strong_count(&table), before + 1);
    assert!(Arc::ptr_eq(fork.wave.table(), &table));
}

#[test]
fn the_shape_is_a_source_of_the_width_declaring_frequency_level_then_position() {
    let node = WavetableNode::from(
        WavetableConfig::new(NonZeroU16::new(3).unwrap(), table(&[&sine_cycle::<f32>()]))
            .frequency(Hz::new(330.0))
            .level(Amplitude::new(0.25))
            .position(Normalized::of(0.5)),
    );
    let shape = Node::<f32>::shape(&node);
    assert_eq!(shape.audio_in().count(), 0);
    assert_eq!(shape.audio_out().count(), 3);
    assert_eq!(shape.tail, Tail::None);
    assert_eq!(
        shape.params.as_slice(),
        &[UnitParam::Frequency, UnitParam::Volume, UnitParam::Position][..]
    );
    assert_eq!(Node::<f32>::param_base(&node, 0), Some(330.0));
    assert_eq!(Node::<f32>::param_base(&node, 1), Some(0.25));
    assert_eq!(Node::<f32>::param_base(&node, 2), Some(0.5));
}

#[test]
fn every_channel_of_the_width_carries_the_one_signal() {
    let mut d = Direct::new(
        WavetableNode::from(WavetableConfig::new(
            NonZeroU16::new(3).unwrap(),
            table(&[&sine_cycle::<f32>()]),
        )),
        RATE,
        256,
    );
    d.block();
    let out = d.output();
    assert!(out.plane(0).samples().iter().any(|&v| v.abs() > 0.5));
    for c in 1..3 {
        assert_eq!(out.plane(c).samples(), out.plane(0).samples());
    }
}

/// In an `f64` graph a sine table at 1 kHz is `sin(2 pi f t)` within the table's
/// interpolation bound, which the node adds nothing to.
#[test]
fn in_an_f64_graph_a_sine_table_is_a_sine_within_the_interpolation_bound() {
    let t = table(&[&sine_cycle::<f64>()]);
    let bound = t.interpolation_error(1) + 1e-12;
    let node = WavetableNode::from(
        WavetableConfig::new(NonZeroU16::MIN, t)
            .frequency(Hz::new(1_000.0))
            .level(Amplitude::new(1.0 / 3.0)),
    );
    let out = render_f64(node, RATE, &[], 1_000);
    for (n, &y) in out[0].iter().enumerate() {
        let exact = libm::sin(core::f64::consts::TAU * 1_000.0 * n as f64 / RATE.get()) / 3.0;
        assert!(
            (y - exact).abs() <= bound / 3.0,
            "sample {n}: {}",
            y - exact
        );
    }
}

#[test]
fn level_scales_the_output_exactly() {
    let full = render(sine_node(997.0), 512);
    let half = render(
        WavetableNode::from(
            WavetableConfig::new(NonZeroU16::MIN, table(&[&sine_cycle::<f32>()]))
                .frequency(Hz::new(997.0))
                .level(Amplitude::new(0.5)),
        ),
        512,
    );
    for (a, b) in full.iter().zip(&half) {
        assert_eq!(*b, a * 0.5);
    }
}

/// Position `p` of a table of the constants `0.25` and `0.75` is `0.25 + 0.5 p`, exactly in
/// `f32` at `p` a multiple of a quarter.
#[test]
fn a_set_position_is_the_blend_of_its_two_frames() {
    let node = two_levels_node();
    let cell = node.position();
    let mut d = Direct::new(node, RATE, 32);
    for (p, want) in [(0.0, 0.25f32), (0.25, 0.375), (0.5, 0.5), (1.0, 0.75)] {
        cell.store(Normalized::of(p));
        d.block();
        for &v in d.output().plane(0).samples() {
            assert!((v - want).abs() <= f32::EPSILON, "position {p}: {v}");
        }
    }
}

#[test]
fn a_driven_position_moves_per_frame_and_a_non_finite_value_holds() {
    let mut signal = vec![0.0f32; 16];
    signal[4..8].fill(1.0);
    signal[8] = f32::NAN;
    signal[9] = f32::INFINITY;
    signal[10..].fill(0.5);
    let mut d = Direct::new(two_levels_node(), RATE, 16);
    d.feed(2, Some(&signal));
    d.block();
    let out = d.output().plane(0).samples();
    let want = |i: usize| match i {
        0..4 => 0.25,
        4..10 => 0.75,
        _ => 0.5,
    };
    for (i, &v) in out.iter().enumerate() {
        assert!((v - want(i)).abs() <= f32::EPSILON, "frame {i}: {v}");
    }
}

#[test]
fn a_driven_frequency_changes_the_pitch_per_frame() {
    let mut d = Direct::new(sine_node(1_000.0), RATE, 96);
    let signal: Vec<f32> = (0..96)
        .map(|i| if i < 48 { 1_000.0 } else { 2_000.0 })
        .collect();
    d.feed(0, Some(&signal));
    d.block();
    let out = d.output().plane(0).samples();
    let bound = 1e-6;
    // 1 kHz for 48 samples is one cycle, so the 2 kHz half starts at phase 0.
    for (n, &y) in out.iter().enumerate().skip(48) {
        let exact = libm::sin(core::f64::consts::TAU * 2_000.0 * (n - 48) as f64 / RATE.get());
        assert!((f64::from(y) - exact).abs() < bound, "sample {n}");
    }
}

#[test]
fn a_non_finite_driven_frequency_and_level_hold_the_last_finite_one() {
    let mut signal = vec![1_000.0f32; 16];
    signal[8] = f32::NAN;
    signal[9] = f32::INFINITY;
    let mut d = Direct::new(sine_node(500.0), RATE, 16);
    d.feed(0, Some(&signal));
    d.block();
    for (n, &y) in d.output().plane(0).iter().enumerate() {
        let exact = libm::sin(core::f64::consts::TAU * 1_000.0 * n as f64 / RATE.get());
        assert!((f64::from(y) - exact).abs() < 1e-6, "sample {n}");
    }
    let mut level = Direct::new(sine_node(1_000.0), RATE, 4);
    level.feed(1, Some(&[0.5, f32::NAN, f32::NEG_INFINITY, 0.5]));
    level.block();
    assert!(
        level
            .output()
            .plane(0)
            .samples()
            .iter()
            .all(|v| v.is_finite())
    );
}

#[test]
fn an_unprepared_node_renders_silence() {
    let mut node = sine_node(440.0);
    let ones = [1.0f32; 32];
    let out = drive::<f32>(&mut node, RATE, Planes::default(), &[None, Some(&ones)]);
    assert!(out[0].iter().all(|&v| v == 0.0));
}

#[test]
fn reset_restarts_the_cycle() {
    let mut d = Direct::new(sine_node(300.0), RATE, 64);
    d.block();
    let first = d.output().plane(0).samples().to_vec();
    Node::<f32>::reset(&mut *d.node);
    d.block();
    assert_eq!(d.output().plane(0).samples(), &first[..]);
}

#[test]
fn a_set_frequency_is_heard_on_the_next_block() {
    let node = sine_node(1_000.0);
    let cell = node.frequency();
    let mut d = Direct::new(node, RATE, 48);
    d.block();
    cell.store(Hz::new(2_000.0));
    d.block();
    let exact = libm::sin(core::f64::consts::TAU * 2_000.0 * 12.0 / RATE.get());
    assert!((f64::from(d.output().plane(0).samples()[12]) - exact).abs() < 1e-6);
}
