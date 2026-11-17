//! The feedback delay network's audio-thread path allocates nothing at any corner of its
//! controls: each layout (the fewest lines at the shortest decay, the most at the longest, one
//! that can freeze), each control at both ends of its range while it moves every block, and
//! the two corners of all of them together. The node is driven alone in a graph by
//! `tutti_graph::contract::BlockRig`: its `prepare` (which sizes the lines, the pre-delay and
//! the scratch a design is made in) runs when the rig is built, outside the gate; the gate
//! walks only `process`, which designs, aims the glides, renders and tracks the tail. The
//! output stays finite and bounded. The park (the clear, the zero fill of the cut block, the
//! idle blocks and the wake), a lane driving the slow controls (the longest tail a block takes
//! is designed from them) and a block of non-finite input run under it too.

use tutti_graph::ParamSet;
use tutti_graph::Status;
use tutti_graph::contract::{BlockRig, Direct};
use tutti_nodes::{FdnControls, FdnLayout, FdnNode, MaxDecay};
use tutti_types::{SampleRate, Seconds, UnitAddr, UnitParam};

/// A bound a stable loop stays far under, wet gain at +24 dB and the longest decay included:
/// an unstable one passes it in a few blocks.
const BOUND: f32 = 1.0e4;

/// One control: its address and the two ends of its range, as the cell holds them.
struct Control {
    addr: UnitAddr,
    lo: f32,
    hi: f32,
}

fn controls(freezable: bool) -> Vec<Control> {
    let on = |param: UnitParam, lo, hi| Control {
        addr: UnitAddr::first(param),
        lo,
        hi,
    };
    let mut all = vec![
        Control {
            addr: UnitAddr::new(UnitParam::Decay, 0),
            lo: 0.05,
            hi: 8.0,
        },
        Control {
            addr: UnitAddr::new(UnitParam::Decay, 1),
            lo: 0.05,
            hi: 8.0,
        },
        Control {
            addr: UnitAddr::new(UnitParam::Decay, 2),
            lo: 0.05,
            hi: 8.0,
        },
        on(UnitParam::RoomSize, 0.0, 1.0),
        on(UnitParam::Diffusion, 0.0, 1.0),
        on(UnitParam::DelayTime, 0.0, 0.5),
        on(UnitParam::Depth, -1.0, 1.0),
        on(UnitParam::Rate, 0.05, 5.0),
        on(UnitParam::StereoSpread, 0.0, 2.0),
        Control {
            addr: UnitAddr::new(UnitParam::GainDb, 0),
            lo: -120.0,
            hi: 24.0,
        },
    ];
    if freezable {
        all.push(on(UnitParam::Freeze, 0.0, 1.0));
    }
    all
}

/// A node of `layout` through `BlockRig` on a noise-like alternating input: 32 warm-up
/// blocks, then `blocks` under `assert_no_alloc`, `between(i)` run before each (the cells
/// move). The output of every block is finite and under [`BOUND`].
fn gate(layout: FdnLayout, blocks: usize, mut between: impl FnMut(&ParamSet, usize)) {
    let node = FdnNode::live(layout, FdnControls::DEFAULT);
    let (mut rig, set) = BlockRig::new(node, SampleRate::SR_48K, 64);
    for c in rig.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 3 == 0 { 0.5 } else { -0.25 };
        }
    }
    for i in 0..32 {
        between(&set, i);
        rig.block();
    }
    let mut peak = 0.0f32;
    let mut finite = true;
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..blocks {
            between(&set, i);
            rig.block();
            for c in 0..2 {
                for &s in rig.output().plane(c).samples() {
                    finite &= s.is_finite();
                    peak = peak.max(s.abs());
                }
            }
        }
    });
    assert!(finite, "the output is finite");
    assert!(peak < BOUND, "the output is bounded: {peak}");
}

/// The default room, its controls still: the plain path through the lines, the design, and
/// the tail tracking.
#[test]
fn the_fdn_node_process_is_allocation_free() {
    gate(FdnLayout::default(), 1_000, |_, _| {});
}

/// Every layout, every control at both ends and moving each block (a write every block from
/// one end to the other, so a glide is in flight on all of them), then all of them at once
/// to the same end and alternately: the corner sweep of the sizes a layout allocates and the
/// designs a control reaches.
#[test]
fn the_fdn_corners_are_allocation_free_with_every_control_moving() {
    let layouts = [
        FdnLayout::new(2, MaxDecay::of(Seconds(0.05)), false).unwrap(),
        FdnLayout::new(8, MaxDecay::DEFAULT, true).unwrap(),
        FdnLayout::new(16, MaxDecay::of(Seconds(60.0)), true).unwrap(),
    ];
    for layout in layouts {
        let all = controls(layout.freezable());
        // One control moving between its ends, the rest at the default.
        for c in &all {
            gate(layout, 200, |set, i| {
                assert!(set.set(c.addr, if i % 2 == 0 { c.lo } else { c.hi }));
            });
        }
        // Every control at once: to the low corner, to the high corner, and alternating.
        for phase in 0..3usize {
            gate(layout, 200, |set, i| {
                for c in &all {
                    let high = match phase {
                        0 => false,
                        1 => true,
                        _ => i % 2 == 1,
                    };
                    assert!(set.set(c.addr, if high { c.hi } else { c.lo }));
                }
            });
        }
    }
}

/// A node of `layout` at `controls` through `Direct`, which feeds its ports and reads its
/// status, on an alternating input of `amplitude`.
fn direct(layout: FdnLayout, controls: FdnControls, amplitude: f32) -> Direct<FdnNode> {
    let mut rig = Direct::new(FdnNode::live(layout, controls), SampleRate::SR_48K, 64);
    set_input(&mut rig, amplitude);
    rig
}

fn set_input(rig: &mut Direct<FdnNode>, amplitude: f32) {
    for c in rig.inputs_mut().iter_mut() {
        let c = c.into_samples_mut();
        for (i, x) in c.iter_mut().enumerate() {
            *x = if i % 2 == 0 { amplitude } else { -amplitude };
        }
    }
}

/// The peak and finiteness of the two output channels of the block `rig` last rendered.
fn level(rig: &Direct<FdnNode>) -> (f32, bool) {
    let mut peak = 0.0f32;
    let mut finite = true;
    for c in 0..2 {
        for &s in rig.output().plane(c).samples() {
            finite &= s.is_finite();
            peak = peak.max(s.abs());
        }
    }
    (peak, finite)
}

/// A node that parks and wakes allocates nothing: past its tail the input goes silent, the
/// block that crosses the tail is cut and zero filled (when the tail ends inside a block),
/// the kernel is cleared and the node is idle block after block, and then the input returns
/// and it renders again. A short room (0.1 s decays) parks within the gate's 800 silent
/// blocks, a long layout's too.
#[test]
fn the_fdn_parks_and_wakes_without_allocating() {
    let short = FdnControls {
        decay: [Seconds(0.1); 3],
        ..FdnControls::DEFAULT
    };
    for layout in [
        FdnLayout::new(2, MaxDecay::of(Seconds(0.1)), false).unwrap(),
        FdnLayout::new(8, MaxDecay::DEFAULT, true).unwrap(),
        FdnLayout::new(16, MaxDecay::DEFAULT, false).unwrap(),
    ] {
        let mut rig = direct(layout, short, 0.5);
        for _ in 0..32 {
            rig.block();
        }
        let (peak, _) = level(&rig);
        assert!(peak > 0.0, "the room rings before the gate");
        set_input(&mut rig, 0.0);
        let (mut idle, mut finite) = (0, true);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..800 {
                let status = rig.block();
                idle += usize::from(status == Status::Idle);
                let (p, f) = level(&rig);
                finite &= f && p < BOUND;
            }
        });
        assert!(finite);
        assert!(idle > 600, "it parks and stays parked: {idle} idle blocks");
        set_input(&mut rig, 0.5);
        let mut woke = 0.0f32;
        let mut finite = true;
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..100 {
                rig.block();
                let (p, f) = level(&rig);
                finite &= f;
                woke = woke.max(p);
            }
        });
        assert!(finite && woke > 0.0 && woke < BOUND, "it wakes: {woke}");
    }
}

/// The slow controls driven by lanes (the decay, the size and the pre-delay, each swept across
/// its whole range inside the block and then back), so the block's longest tail is designed
/// from the extremes of the lanes every block, and the grid reads and the corners of each
/// lane retarget.
#[test]
fn the_fdn_with_its_lanes_fed_is_allocation_free() {
    let sweep = |lo: f32, hi: f32, up: bool| -> Vec<f32> {
        (0..64)
            .map(|i| {
                let t = i as f32 / 63.0;
                lo + (hi - lo) * if up { t } else { 1.0 - t }
            })
            .collect()
    };
    for layout in [
        FdnLayout::new(2, MaxDecay::of(Seconds(0.05)), false).unwrap(),
        FdnLayout::new(8, MaxDecay::DEFAULT, true).unwrap(),
    ] {
        for up in [true, false] {
            let mut rig = direct(layout, FdnControls::DEFAULT, 0.5);
            rig.feed(0, Some(&sweep(0.05, 8.0, up)));
            rig.feed(3, Some(&sweep(0.0, 1.0, !up)));
            rig.feed(5, Some(&sweep(0.0, 0.5, up)));
            for _ in 0..32 {
                rig.block();
            }
            let (mut peak, mut finite) = (0.0f32, true);
            assert_no_alloc::assert_no_alloc(|| {
                for _ in 0..400 {
                    rig.block();
                    let (p, f) = level(&rig);
                    peak = peak.max(p);
                    finite &= f;
                }
            });
            assert!(finite, "the output is finite");
            assert!(peak < BOUND, "the output is bounded: {peak}");
        }
    }
}

/// A block of non-finite input (a NaN and both infinities) is rendered, and the blocks after
/// it, without allocating. The loop it reaches is poisoned (a recirculating line keeps what it
/// is given), so nothing is asserted of the output: the test can fail only on an allocation
/// or a panic, which is the property it pins, and it is the last thing the node is asked to do.
#[test]
fn the_fdn_renders_non_finite_input_without_allocating() {
    for layout in [
        FdnLayout::new(8, MaxDecay::DEFAULT, false).unwrap(),
        FdnLayout::new(8, MaxDecay::DEFAULT, true).unwrap(),
    ] {
        let mut rig = direct(layout, FdnControls::DEFAULT, 0.5);
        for _ in 0..32 {
            rig.block();
        }
        for c in rig.inputs_mut().iter_mut() {
            let c = c.into_samples_mut();
            c[0] = f32::NAN;
            c[1] = f32::INFINITY;
            c[2] = f32::NEG_INFINITY;
        }
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..200 {
                rig.block();
            }
        });
    }
}
