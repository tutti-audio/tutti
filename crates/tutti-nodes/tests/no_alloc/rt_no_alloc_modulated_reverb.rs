//! The corner sweep of the chorus, the flanger and Freeverb: hostile controls (NaN, `+-inf`,
//! the extremes of each range) written to the cells and driven on the ports, at widths 1, 2
//! and 6 for the swept delay, with the output finite and within the bound the feedback sets
//! and no allocation on the audio thread.

use tutti_graph::ParamNode;
use tutti_nodes::{FreeverbControls, FreeverbNode, ModDelayControls, ModDelayNode, Stagger, Sweep};
use tutti_types::UnitParam::{Damping, Depth, Feedback, Rate, RoomSize};
use tutti_types::{BusTopology, ChannelLayout, PhaseIncrement};

use super::rt_no_alloc_dynamics::{Corners, addr, sweep};

/// What a lane takes: not a number, infinite, huge, negative, zero and in range, each read at
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

fn bus(width: u16) -> BusTopology {
    BusTopology::assumed(ChannelLayout::from(width)).expect("one bus")
}

/// The chorus and the flanger at every corner, at widths 1, 2 and 6: the rate from below its
/// floor to 1 kHz, the depth from nothing to a second of delay (held to the sweep's greatest),
/// the feedback from below nothing to far past the stable bound (held under it), and each as a
/// hostile lane. The loop gain is held to 0.99, so a bounded input stays under `1 / 0.01`
/// times itself.
#[test]
fn the_swept_delay_corners_are_allocation_free_and_finite() {
    static CORNERS: Corners = Corners {
        cells: &[
            (addr(Rate, 0), -3.0, 1_000.0),
            (addr(Depth, 0), 0.0, 1.0),
            (addr(Feedback, 0), -1.0, 5.0),
        ],
        lanes: &[0, 1, 2],
        lane_values: &[HOSTILE],
        bound: 151.0,
        frames: 19_200,
    };
    static HELD: Corners = Corners {
        cells: &[
            (addr(Rate, 0), f32::NAN, f32::INFINITY),
            (addr(Depth, 0), f32::NEG_INFINITY, f32::NAN),
            (addr(Feedback, 0), f32::INFINITY, f32::NAN),
        ],
        lanes: &[],
        lane_values: &[],
        bound: 151.0,
        frames: 19_200,
    };
    for width in [1u16, 2, 6] {
        for corners in [&CORNERS, &HELD] {
            sweep(|| ModDelayNode::chorus(bus(width)), corners, 1.5, None);
            sweep(|| ModDelayNode::flanger(bus(width)), corners, 1.5, None);
            sweep(
                || {
                    ModDelayNode::new(
                        bus(width),
                        Sweep::FLANGER,
                        Stagger::of_step(PhaseIncrement(0.37)),
                        ModDelayControls::FLANGER,
                    )
                    .expect("a step fits any width")
                },
                corners,
                1.5,
                None,
            );
        }
    }
    sweep(
        || ModDelayNode::general_midi_chorus(BusTopology::STEREO),
        &CORNERS,
        1.5,
        None,
    );
}

/// Freeverb at every corner, the live node and the General MIDI send: the room and the damping
/// from far below `0` to far past `1` and not a number, and each as a hostile lane. The comb
/// loop gain is held to 0.98 and the all-passes' response sums to under 3 each, so a bounded
/// input stays under `0.015 * 8 * 50 * 81` times itself.
#[test]
fn freeverb_corners_are_allocation_free_and_finite() {
    static CORNERS: Corners = Corners {
        cells: &[
            (addr(RoomSize, 0), -1.0e30, 1.0e30),
            (addr(Damping, 0), -2.0, 9.0),
        ],
        lanes: &[0, 1],
        lane_values: &[HOSTILE],
        bound: 1_000.0,
        frames: 19_200,
    };
    static HELD: Corners = Corners {
        cells: &[
            (addr(RoomSize, 0), f32::NAN, f32::INFINITY),
            (addr(Damping, 0), f32::NEG_INFINITY, f32::NAN),
        ],
        lanes: &[],
        lane_values: &[],
        bound: 1_000.0,
        frames: 19_200,
    };
    for corners in [&CORNERS, &HELD] {
        sweep(
            || FreeverbNode::live(FreeverbControls::GENERAL_MIDI),
            corners,
            1.5,
            None,
        );
    }
    sweep(FreeverbNode::general_midi, &HELD, 1.5, None);
    // The send has no cells to write: the sweep above drives nothing but its input.
    assert_eq!(FreeverbNode::general_midi().param_set().params().count(), 0);
    assert_eq!(
        FreeverbNode::live(FreeverbControls::GENERAL_MIDI)
            .param_set()
            .params()
            .count(),
        2
    );
}
