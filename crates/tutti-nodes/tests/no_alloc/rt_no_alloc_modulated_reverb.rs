//! The corner sweep of the chorus, the flanger and Freeverb: hostile controls (NaN, `+-inf`,
//! the extremes of each range) written to the cells and driven on the ports, at widths 1, 2
//! and 6 for the swept delay, with the output finite and within the bound the feedback sets
//! and no allocation on the audio thread.

use crate::harness::bus;
use tutti_buffers::ChannelTopology;
use tutti_graph::ParamNode;
use tutti_nodes::{
    FreeverbConfig, FreeverbNode, ModDelayConfig, ModDelayControls, ModDelayLayout, ModDelayNode,
    PlateReverbConfig, PlateReverbNode, Stagger, Sweep,
};
use tutti_units::PhaseIncrement;
use tutti_units::UnitParam::{
    Cutoff, Damping, Decay, DelayTime, Depth, Diffusion, Feedback, Rate, RoomSize,
};

use super::rt_no_alloc_dynamics::{Corners, addr, sweep};

/// What a control signal takes: not a number, infinite, huge, negative, zero and in range, each read at
/// a grid corner by a rate (Hz), a depth (seconds) and a feedback.
const HOSTILE: &[f32] = &[
    f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    -1.0e30,
    1.0e30,
    -3.0,
    0.0,
    0.005,
    0.5,
    2.0,
];

/// The chorus and the flanger at every corner, at widths 1, 2 and 6: the rate from below its
/// floor to 1 kHz, the depth from nothing to a second of delay (held to the sweep's greatest),
/// the feedback from below nothing to far past the stable bound (held under it), and each as a
/// hostile control signal. The loop gain is held to 0.99, so a bounded input stays under `1 / 0.01`
/// times itself.
#[test]
fn the_swept_delay_corners_are_allocation_free_and_finite() {
    static CORNERS: Corners = Corners {
        cells: &[
            (addr(Rate, 0), -3.0, 1_000.0),
            (addr(Depth, 0), 0.0, 1.0),
            (addr(Feedback, 0), -1.0, 5.0),
        ],
        signals: &[0, 1, 2],
        signal_values: &[HOSTILE],
        bound: 151.0,
        frames: 19_200,
    };
    static HELD: Corners = Corners {
        cells: &[
            (addr(Rate, 0), f32::NAN, f32::INFINITY),
            (addr(Depth, 0), f32::NEG_INFINITY, f32::NAN),
            (addr(Feedback, 0), f32::INFINITY, f32::NAN),
        ],
        signals: &[],
        signal_values: &[],
        bound: 151.0,
        frames: 19_200,
    };
    for width in [1u16, 2, 6] {
        for corners in [&CORNERS, &HELD] {
            sweep(
                || ModDelayNode::<f32>::from(ModDelayConfig::chorus(bus(width))),
                corners,
                1.5,
                None,
            );
            sweep(
                || ModDelayNode::<f32>::from(ModDelayConfig::flanger(bus(width))),
                corners,
                1.5,
                None,
            );
            sweep(
                || {
                    ModDelayNode::<f32>::from(
                        ModDelayConfig::new(
                            ModDelayLayout::builder(bus(width), Sweep::FLANGER)
                                .stagger(Stagger::of_step(PhaseIncrement::new(0.37)))
                                .build()
                                .unwrap(),
                        )
                        .controls(ModDelayControls::FLANGER),
                    )
                },
                corners,
                1.5,
                None,
            );
        }
    }
    sweep(
        || ModDelayNode::<f32>::from(ModDelayConfig::general_midi_chorus(ChannelTopology::STEREO)),
        &CORNERS,
        1.5,
        None,
    );
}

/// Freeverb at every corner: the room and the damping
/// from far below `0` to far past `1` and not a number, and each as a hostile control signal. The comb
/// loop gain is held to 0.98 and the all-passes' response sums to under 3 each, so a bounded
/// input stays under `0.015 * 8 * 50 * 81` times itself.
#[test]
fn freeverb_corners_are_allocation_free_and_finite() {
    static CORNERS: Corners = Corners {
        cells: &[
            (addr(RoomSize, 0), -1.0e30, 1.0e30),
            (addr(Damping, 0), -2.0, 9.0),
        ],
        signals: &[0, 1],
        signal_values: &[HOSTILE],
        bound: 1_000.0,
        frames: 19_200,
    };
    static HELD: Corners = Corners {
        cells: &[
            (addr(RoomSize, 0), f32::NAN, f32::INFINITY),
            (addr(Damping, 0), f32::NEG_INFINITY, f32::NAN),
        ],
        signals: &[],
        signal_values: &[],
        bound: 1_000.0,
        frames: 19_200,
    };
    for corners in [&CORNERS, &HELD] {
        sweep(
            || FreeverbNode::<f32>::from(FreeverbConfig::new()),
            corners,
            1.5,
            None,
        );
    }
    // The sweeps drive the node's two cells.
    assert_eq!(
        FreeverbNode::<f32>::from(FreeverbConfig::new())
            .param_set()
            .params()
            .count(),
        2
    );
}

/// The plate at every corner: each control from far below its range to far past it and not a
/// number. Every coefficient is clamped by the design (the decay to 50 ms to 8 s, the damping
/// pole under 1), so the loop never gains and a bounded input stays bounded.
#[test]
fn plate_corners_are_allocation_free_and_finite() {
    static CORNERS: Corners = Corners {
        cells: &[
            (addr(Decay, 0), -1.0e30, 1.0e30),
            (addr(Damping, 0), -2.0, 9.0),
            (addr(DelayTime, 0), -1.0, 9.0),
            (addr(Cutoff, 0), -1.0e30, 1.0e30),
            (addr(Depth, 0), -5.0, 5.0),
            (addr(Diffusion, 0), -2.0, 9.0),
        ],
        signals: &[],
        signal_values: &[],
        bound: 1_000.0,
        frames: 19_200,
    };
    static HELD: Corners = Corners {
        cells: &[
            (addr(Decay, 0), f32::NAN, f32::INFINITY),
            (addr(Damping, 0), f32::NEG_INFINITY, f32::NAN),
            (addr(DelayTime, 0), f32::NAN, f32::INFINITY),
            (addr(Cutoff, 0), f32::INFINITY, f32::NAN),
            (addr(Depth, 0), f32::NAN, f32::NEG_INFINITY),
            (addr(Diffusion, 0), f32::INFINITY, f32::NAN),
        ],
        signals: &[],
        signal_values: &[],
        bound: 1_000.0,
        frames: 19_200,
    };
    for corners in [&CORNERS, &HELD] {
        sweep(
            || PlateReverbNode::<f32>::from(PlateReverbConfig::new()),
            corners,
            1.5,
            None,
        );
    }
    // Every control the sweep writes is a cell of the node: the swept ports are the plate's six.
    let node = PlateReverbNode::<f32>::from(PlateReverbConfig::new());
    assert_eq!(node.param_set().params().count(), 6);
}
