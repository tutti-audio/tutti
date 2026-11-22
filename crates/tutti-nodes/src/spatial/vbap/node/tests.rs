use super::*;
use tutti_buffers::{MaxBlock, Planes};
use tutti_graph::ParamNode;
use tutti_graph::contract::{drive, prepared};
use tutti_units::{Gain, UnitParam};

use tutti_dsp::SpeakerLayout;

use crate::testing::aim;

const RATE: SampleRate = SampleRate::SR_48K;

/// `node`, prepared at 48 kHz for blocks of up to 64 frames.
fn prep(node: VbapPannerNode<f32>) -> VbapPannerNode<f32> {
    prepared(node, RATE, 64)
}

/// One block of one frame, written to `out`.
fn tick(node: &mut VbapPannerNode<f32>, input: &[f32; 2], out: &mut [f32]) {
    tick_at(node, RATE, input, out);
}

fn tick_at(node: &mut VbapPannerNode<f32>, rate: SampleRate, input: &[f32; 2], out: &mut [f32]) {
    let rendered = drive(
        node,
        rate,
        Planes::new(&[&input[..1], &input[1..]]).unwrap(),
        &[],
    );
    for (o, c) in out.iter_mut().zip(rendered) {
        *o = c[0];
    }
}

#[test]
fn vbap_panner_tick() {
    let mut panner = prep(vbap::<f32>(tutti_buffers::Bed::Stereo));
    aim(&panner.param_set(), Azimuth::new(0.0), Elevation::new(0.0));

    let input = [1.0f32, 1.0f32];
    let mut output = [0.0f32; 2];

    tick(&mut panner, &input, &mut output);

    // Dead ahead at the default width: two virtual sources at +-15 degrees
    // over the +-30 pair, whose pan law at 15 degrees is (0.939071, 0.343724),
    // so each output is 0.939071 + 0.343724 of a unit pair.
    let want = 0.939_071 + 0.343_724;
    for (c, &o) in output.iter().enumerate() {
        assert!((o - want).abs() < 1e-4, "channel {c}: {o}, want {want}");
    }
}

/// `Node::reset` resets time, not settings.
///
/// A fork (an offline export's copy of the live graph) is reset before it
/// renders, to drop inherited state; a host resets between clips for the
/// same reason. A reset that re-aimed would move every spatialised
/// source to front-centre in both cases, with nothing to compare and no
/// error raised.
#[test]
fn reset_keeps_the_authored_placement() {
    let mut panner = prep(vbap::<f32>(tutti_buffers::Bed::Surround51));
    aim(
        &panner.param_set(),
        Azimuth::new(45.0),
        Elevation::new(15.0),
    );
    panner.cells.spread.store(Spread::new(0.3));
    panner.cells.width.store(StereoWidth::new(1.5));

    Node::reset(&mut panner);

    assert!(
        (panner.cells.azimuth.load().get() - 45.0).abs() < 0.001,
        "reset moved the bearing to {}",
        panner.cells.azimuth.load().get()
    );
    assert!(
        (panner.cells.elevation.load().get() - 15.0).abs() < 0.001,
        "reset moved the height to {}",
        panner.cells.elevation.load().get()
    );
    assert!(
        (panner.cells.spread.load().get() - 0.3).abs() < 0.001,
        "reset changed the spread to {}",
        panner.cells.spread.load().get()
    );
    assert!(
        (panner.cells.width.load().get() - 1.5).abs() < 0.001,
        "reset changed the width to {}",
        panner.cells.width.load().get()
    );
}

/// The other half of the contract: the ramp *is* dropped, so the frame
/// after a reset renders at the commanded position rather than sweeping in
/// from wherever the ramp had got to.
///
/// Asserted against a *settled* reference panner rather than against a
/// channel inequality: a ramp that starts at front-centre has both channels
/// near-equal a few frames into a hard-left move, and either one may lead by a
/// hair. "Equals the settled answer" is the property a seated node actually
/// has, and it is the one that fails when the ramp is left in flight.
///
/// The `-90°` half is the guard: at `+90°` a bug that seeds front-centre still
/// leaves the left channel leading, so a one-sided assertion could pass for the
/// wrong reason.
#[test]
fn reset_seats_the_ramp_on_the_commanded_position() {
    let input = [1.0f32, 1.0f32];

    for (bearing, lead, silent) in [(90.0f64, 0usize, 1usize), (-90.0, 1, 0)] {
        // Where the panner ends up once the 50 ms ramp has run out: a hard
        // pan, so the opposite channel is silent.
        let mut settled = prep(vbap::<f32>(tutti_buffers::Bed::Stereo));
        aim(
            &settled.param_set(),
            Azimuth::new(bearing),
            Elevation::LEVEL,
        );
        let mut reference = [0.0f32; 2];
        for _ in 0..48_000 {
            tick(&mut settled, &input, &mut reference);
        }
        assert!(
            reference[lead] > 0.9 && reference[silent] < 0.1,
            "the settled reference at {bearing} should be a hard pan, got {reference:?}"
        );

        // The node has rendered at front-centre, then is aimed and reset: the
        // reset drops the ramp that would carry it there.
        let mut panner = prep(vbap::<f32>(tutti_buffers::Bed::Stereo));
        let mut scratch = [0.0f32; 2];
        for _ in 0..64 {
            tick(&mut panner, &input, &mut scratch);
        }
        aim(&panner.param_set(), Azimuth::new(bearing), Elevation::LEVEL);
        Node::reset(&mut panner);

        let mut after = [0.0f32; 2];
        tick(&mut panner, &input, &mut after);
        assert!(
            (after[0] - reference[0]).abs() < 0.01 && (after[1] - reference[1]).abs() < 0.01,
            "at {bearing}deg, the first frame after a reset should already render \
             at the commanded position: got {after:?}, settled is {reference:?}"
        );
    }
}

/// The ramp is genuinely in flight before a reset — otherwise
/// [`reset_seats_the_ramp_on_the_commanded_position`] would hold trivially and
/// prove nothing about the reset. A node aimed before its first block lands on
/// the aim whole; one aimed after is 50 ms from it.
#[test]
fn a_bearing_written_after_the_first_block_ramps() {
    let input = [1.0f32, 1.0f32];

    let mut settled = prep(vbap::<f32>(tutti_buffers::Bed::Stereo));
    aim(&settled.param_set(), Azimuth::new(90.0), Elevation::LEVEL);
    let mut reference = [0.0f32; 2];
    for _ in 0..48_000 {
        tick(&mut settled, &input, &mut reference);
    }

    let mut panner = prep(vbap::<f32>(tutti_buffers::Bed::Stereo));
    let mut scratch = [0.0f32; 2];
    tick(&mut panner, &input, &mut scratch);
    aim(&panner.param_set(), Azimuth::new(90.0), Elevation::LEVEL);
    let mut mid_ramp = [0.0f32; 2];
    for _ in 0..64 {
        tick(&mut panner, &input, &mut mid_ramp);
    }
    assert!(
        (mid_ramp[1] - reference[1]).abs() > 0.1,
        "64 frames in, the ramp should still be far from settled, \
         got {mid_ramp:?} vs {reference:?}"
    );
}

/// `Clone` shares the position atomics, so a reset that wrote them would
/// reach back through every handle — including the live node an offline
/// render was cloned from.
#[test]
fn reset_on_a_clone_does_not_move_the_original() {
    let panner = vbap::<f32>(tutti_buffers::Bed::Stereo);
    aim(
        &panner.param_set(),
        Azimuth::new(-60.0),
        Elevation::new(20.0),
    );

    let mut cloned = panner.clone();
    Node::reset(&mut cloned);

    assert!(
        (panner.cells.azimuth.load().get() - (-60.0)).abs() < 0.001,
        "resetting the clone moved the original to {}",
        panner.cells.azimuth.load().get()
    );
    assert!(
        (panner.cells.elevation.load().get() - 20.0).abs() < 0.001,
        "resetting the clone moved the original to {}",
        panner.cells.elevation.load().get()
    );
}

/// Every separately shared cell survives a clone as a shared handle,
/// not as a snapshot.
///
/// `Clone` hands on `azimuth`, `elevation`, `spread` and `width` by handle
/// (a `Param` clone shares its cell), so all four are asserted -- one per
/// shared field. Covering only the position leaves a hole the compiler
/// cannot see: a hand-written `Clone` building `spread` with
/// `Param::new(spread.load())` is a snapshotting clone that still passes a
/// position-only test.
///
/// Why sharing is the required behaviour rather than an implementation
/// detail: a clone is the fork source's template (`tutti_graph::param_parts`),
/// taken at insert, so a snapshotting clone would freeze every later export at
/// whatever spread or width happened to be set at insert and silently
/// ignore every later move. (The fork itself detaches; see
/// `tests/spatial/spatial_param_fork.rs`.) It is also the premise of
/// `reset_on_a_clone_does_not_move_the_original` -- `reset` must not write
/// these cells precisely because the write would reach back through every
/// handle.
///
/// Asserted in both directions per field: a handle is shared, not merely
/// copied forward, so a write through either side must be visible from the
/// other.
#[test]
fn vbap_clone_shares_atomics() {
    let panner = vbap::<f32>(tutti_buffers::Bed::Stereo);
    let cloned = panner.clone();

    // -- target: azimuth + elevation, set on the original.
    aim(
        &panner.param_set(),
        Azimuth::new(90.0),
        Elevation::new(45.0),
    );
    assert!(
        (cloned.cells.azimuth.load().get() - 90.0).abs() < 0.001,
        "the clone did not see the original's bearing: {}",
        cloned.cells.azimuth.load().get()
    );
    assert!(
        (cloned.cells.elevation.load().get() - 45.0).abs() < 0.001,
        "the clone did not see the original's height: {}",
        cloned.cells.elevation.load().get()
    );

    // And vice versa.
    aim(
        &cloned.param_set(),
        Azimuth::new(-60.0),
        Elevation::new(10.0),
    );
    assert!(
        (panner.cells.azimuth.load().get() - (-60.0)).abs() < 0.001,
        "the original did not see the clone's bearing: {}",
        panner.cells.azimuth.load().get()
    );
    assert!(
        (panner.cells.elevation.load().get() - 10.0).abs() < 0.001,
        "the original did not see the clone's height: {}",
        panner.cells.elevation.load().get()
    );

    // -- spread: its own cell.
    panner.cells.spread.store(Spread::new(0.3));
    assert!(
        (cloned.cells.spread.load().get() - 0.3).abs() < 0.001,
        "the clone did not see the original's spread: {} -- a snapshotting \
         clone would freeze an offline render at the clone-time value",
        cloned.cells.spread.load().get()
    );
    cloned.cells.spread.store(Spread::new(0.8));
    assert!(
        (panner.cells.spread.load().get() - 0.8).abs() < 0.001,
        "the original did not see the clone's spread: {}",
        panner.cells.spread.load().get()
    );

    // -- width: its own cell too.
    panner.cells.width.store(StereoWidth::new(1.5));
    assert!(
        (cloned.cells.width.load().get() - 1.5).abs() < 0.001,
        "the clone did not see the original's width: {}",
        cloned.cells.width.load().get()
    );
    cloned.cells.width.store(StereoWidth::new(0.4));
    assert!(
        (panner.cells.width.load().get() - 0.4).abs() < 0.001,
        "the original did not see the clone's width: {}",
        panner.cells.width.load().get()
    );
}

/// A clone keeps the original's sample rate, so its ramp runs at the same
/// speed: the original and its clone at 96 kHz sweep to a new bearing
/// identically, and the sweep is 50 ms long there (4 800 frames), not the 2 400
/// of 48 kHz.
#[test]
fn a_clone_ramps_at_the_originals_sample_rate() {
    let rate = SampleRate::SR_96K;
    let mut original = prepared(vbap::<f32>(tutti_buffers::Bed::Stereo), rate, 64);
    let mut clone = original.clone();
    let (mut a, mut b) = ([0.0f32; 2], [0.0f32; 2]);
    tick_at(&mut original, rate, &[1.0, 1.0], &mut a);
    tick_at(&mut clone, rate, &[1.0, 1.0], &mut b);
    aim(&original.param_set(), Azimuth::new(60.0), Elevation::LEVEL);
    let start = a;
    let mut at_48k_length = a;
    for i in 0..4_800 {
        tick_at(&mut original, rate, &[1.0, 1.0], &mut a);
        tick_at(&mut clone, rate, &[1.0, 1.0], &mut b);
        assert_eq!(a, b, "frame {i}: the clone's ramp diverged");
        if i == 2_399 {
            at_48k_length = a;
        }
    }
    assert_ne!(at_48k_length, a, "the ramp is not over at 2 400 frames");
    assert_ne!(start, a);
}

/// A non-finite position, spread or width never reaches the solve: the node
/// keeps the last finite value, renders finite audio, and follows the next
/// finite write.
#[test]
fn a_non_finite_control_reads_as_unchanged() {
    let bad = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
    for value in bad {
        for which in 0..4 {
            let mut node = prep(vbap::<f32>(tutti_buffers::Bed::Surround51));
            aim(&node.param_set(), Azimuth::new(30.0), Elevation::LEVEL);
            let (mut out, input) = ([0.0f32; 6], [0.5f32, -0.25]);
            let mut all_finite = true;
            let mut run = |node: &mut VbapPannerNode<f32>, frames: usize| {
                for _ in 0..frames {
                    tick(node, &input, &mut out);
                    all_finite &= out.iter().all(|s| s.is_finite());
                }
            };
            run(&mut node, 64);
            match which {
                0 => node.cells.azimuth.store(Azimuth::new(value)),
                1 => node.cells.elevation.store(Elevation::new(value)),
                2 => node.cells.spread.store(Spread::new(value)),
                _ => node.cells.width.store(StereoWidth::new(value)),
            }
            // Everything else moves in the same block.
            if which != 0 {
                node.cells.azimuth.store(Azimuth::new(-70.0));
            }
            if which != 2 {
                node.cells.spread.store(Spread::new(0.4));
            }
            run(&mut node, 4_800);
            aim(&node.param_set(), Azimuth::new(10.0), Elevation::new(5.0));
            node.cells.spread.store(Spread::new(0.1));
            node.cells.width.store(StereoWidth::new(1.0));
            run(&mut node, 4_800);
            assert!(all_finite, "control {which} = {value} reached the output");
        }
    }
}

/// The `ParamSet` an insert hands back addresses the node's own cells: a move
/// through it reaches the running node, which the graph owns.
#[test]
fn the_inserted_params_move_the_running_node() {
    use tutti_graph::Standalone;
    use tutti_units::Samples;

    let mut solo = Standalone::new(
        vbap::<f32>(tutti_buffers::Bed::Stereo),
        Prepare::new(RATE, MaxBlock::of(Samples::new(64))),
    );
    aim(solo.controls(), Azimuth::new(90.0), Elevation::LEVEL);
    // 100 ms: past the 50 ms ramp.
    let ones = vec![1.0f32; 4_800];
    let out = solo.render_input(Planes::new(&[&ones, &ones]).unwrap());
    let (l, r) = (out[0][4_799], out[1][4_799]);
    assert!(
        l > 0.9 && r < 0.1,
        "a hard-left move through the params should have landed: [{l}, {r}]"
    );
}

/// A channel's speaker is its place among the placed speakers of the bed: the
/// LFE (index 3 of every bed that has one) feeds from no speaker and the
/// channels after it take the next one, so the surrounds are not one slot early.
/// The literals are the beds' orders worked by hand.
#[test]
fn a_channels_speaker_skips_the_lfe() {
    let n = |count: usize| (0..count).map(Some).collect::<Vec<_>>();
    // The bed's channels with the LFE at index 3: speakers 0..3, none, then on.
    let with_lfe = |count: usize| {
        let mut v = n(3);
        v.push(None);
        v.extend((3..count - 1).map(Some));
        v
    };
    let rows: [(Bed, Vec<Option<usize>>); 11] = [
        (Bed::Mono, n(1)),
        (Bed::Stereo, n(2)),
        (Bed::Quad, n(4)),
        (Bed::Surround50, n(5)),
        (Bed::Surround51, with_lfe(6)),
        (Bed::Surround70, n(7)),
        (Bed::Surround71, with_lfe(8)),
        (Bed::Surround512, with_lfe(8)),
        (Bed::Surround514, with_lfe(10)),
        (Bed::Surround712, with_lfe(10)),
        (Bed::Surround714, with_lfe(12)),
    ];
    for (bed, want) in rows {
        assert_eq!(speaker_of_channel(bed), want, "{bed:?}");
    }
}

/// The ports a control signal can drive are the params the node declares, each with
/// its cell as the base the graph's modulation rides on.
#[test]
fn the_panner_declares_its_params() {
    let node = vbap::<f32>(tutti_buffers::Bed::Stereo);
    node.cells.azimuth.store(Azimuth::new(10.0));
    node.cells.elevation.store(Elevation::new(20.0));
    node.cells.spread.store(Spread::new(0.3));
    node.cells.width.store(StereoWidth::new(1.5));
    assert_eq!(
        node.shape().params.as_slice(),
        &[
            UnitParam::Azimuth,
            UnitParam::Elevation,
            UnitParam::Spread,
            UnitParam::StereoWidth,
        ][..]
    );
    let bases: Vec<_> = (0..5).map(|k| node.param_base(k)).collect();
    assert_eq!(
        bases,
        [Some(10.0), Some(20.0), Some(0.3), Some(1.5), None],
        "each port's base is its cell"
    );
}

/// A control signal that drives a param is heard as the cell would be: a node whose
/// control signal holds the value, over a cell at another, settles on the render of a
/// node whose cell holds it, for each of the four ports.
///
/// The cell and the control signal are told apart by the default: a render at the
/// value differs from one at the default (else the row would pass on a node
/// that ignored both).
#[test]
fn a_signal_drives_each_param_as_its_cell_would() {
    let (left, right) = ([0.5f32; 64], [-0.25f32; 64]);
    let settle = |node: &mut VbapPannerNode<f32>, signal: Option<(usize, f32)>| {
        let held = signal.map(|(_, v)| [v; 64]);
        let mut params: [Option<&[f32]>; 4] = [None; 4];
        if let Some((k, _)) = signal {
            params[k] = held.as_ref().map(|v| &v[..]);
        }
        // Ten ramp lengths of blocks.
        let mut last = Vec::new();
        for _ in 0..(RATE.get() as usize / 2).div_ceil(64) {
            last = drive(node, RATE, Planes::new(&[&left, &right]).unwrap(), &params);
        }
        last.iter().map(|c| c[63]).collect::<Vec<f32>>()
    };
    for (k, param, value) in [
        (0, UnitParam::Azimuth, 90.0f32),
        (1, UnitParam::Elevation, 45.0),
        (2, UnitParam::Spread, 0.8),
        (3, UnitParam::StereoWidth, 0.2),
    ] {
        let fresh = || prep(vbap::<f32>(tutti_buffers::Bed::Surround714));
        let mut default = fresh();
        let at_default = settle(&mut default, None);

        let mut cell = fresh();
        assert!(cell.param_set().set(param, f64::from(value)));
        Node::reset(&mut cell);
        let at_cell = settle(&mut cell, None);
        assert!(
            at_cell
                .iter()
                .zip(&at_default)
                .any(|(a, b)| (a - b).abs() > 0.01),
            "{param:?}: the value does not change the render, so the row proves nothing"
        );

        let mut signal = fresh();
        let at_signal = settle(&mut signal, Some((k, value)));
        for (ch, (a, b)) in at_signal.iter().zip(&at_cell).enumerate() {
            assert!(
                (a - b).abs() < 1e-3,
                "{param:?}, channel {ch}: the signal settled at {a}, the cell at {b}"
            );
        }
    }
}

/// A control signal-driven bearing that crosses the seam takes the short arc: 170 degrees
/// driven to 190 (which is -170) is followed to -170, and the gains chord
/// between the two rear-pair vectors, so the speakers silent at both ends (the
/// front pair of a quad) stay silent along the way. The long way round,
/// 340 degrees through the front, lights them.
///
/// The speakers that are silent at both ends are found by rendering both
/// ends settled, not named, so the row follows the bed.
#[test]
fn a_signal_driven_bearing_crosses_the_seam_the_short_way() {
    let ones = [1.0f32; 64];
    let at = |bearing: f64| {
        let mut node = prep(vbap::<f32>(tutti_buffers::Bed::Quad));
        node.cells.azimuth.store(Azimuth::new(bearing));
        Node::reset(&mut node);
        drive(
            &mut node,
            RATE,
            Planes::new(&[&ones[..1], &ones[..1]]).unwrap(),
            &[],
        )
        .iter()
        .map(|c| c[0])
        .collect::<Vec<f32>>()
    };
    let (start, end) = (at(170.0), at(-170.0));
    let front: Vec<usize> = (0..4)
        .filter(|&c| start[c] < 0.05 && end[c] < 0.05)
        .collect();
    assert_eq!(
        front.len(),
        2,
        "the rear pair carries both ends: {start:?} {end:?}"
    );

    let mut node = prep(vbap::<f32>(tutti_buffers::Bed::Quad));
    node.cells.azimuth.store(Azimuth::new(170.0));
    Node::reset(&mut node);
    let signal = [190.0f32; 64];
    let mut loudest_front = 0.0f32;
    let mut last = Vec::new();
    // Ten ramp lengths: the ramp is over.
    for _ in 0..(RATE.get() as usize / 2).div_ceil(64) {
        last = drive(
            &mut node,
            RATE,
            Planes::new(&[&ones, &ones]).unwrap(),
            &[Some(&signal)],
        );
        for &c in &front {
            loudest_front = last[c].iter().fold(loudest_front, |m, s| m.max(s.abs()));
        }
    }
    assert!(
        loudest_front < 0.05,
        "the ramp from 170 to 190 lit the front pair ({loudest_front}): the long way round"
    );
    for c in 0..4 {
        assert!(
            (last[c][63] - end[c]).abs() < 1e-3,
            "channel {c}: settled at {}, the bearing -170 renders {}",
            last[c][63],
            end[c]
        );
    }
}

/// The gains of a source at `azimuth` on the horizon: one per placed speaker,
/// by the layout's own solve.
fn solve(bed: Bed, azimuth: Azimuth) -> Vec<Gain> {
    let layout = SpeakerLayout::try_from(bed).unwrap();
    let mut gains = vec![Gain::SILENT; speaker_of_channel(bed).iter().flatten().count()];
    layout
        .gains_into(azimuth, Elevation::LEVEL, Spread::POINT, &mut gains)
        .unwrap();
    gains
}

/// A cell's ramp is about 50 ms at every rate: its grid points, of 16 frames,
/// are within one grid period of 50 ms.
#[test]
fn the_cell_ramp_is_fifty_milliseconds_to_a_grid_period_at_every_rate() {
    for rate in [22_050.0, 44_100.0, 48_000.0, 96_000.0, 192_000.0] {
        let frames = f64::from(ramp_steps(SampleRate::new(rate).unwrap())) * GRID as f64;
        let want = 0.05 * rate;
        assert!(
            (frames - want).abs() <= GRID as f64,
            "{rate} Hz: {frames} frames, want {want}"
        );
    }
}

/// A bearing written as a step from 170 to -170 degrees ramps along the short
/// arc: at every grid point the last frame of its segment plays the gains of
/// the closed-form bearing `170 + 20 j / N` (through 180, wrapped), on a quad
/// whose front pair is silent anywhere on that arc. The chord between the gains
/// at the two ends is a different vector at the midpoint, so a ramp of the
/// gains, or the long way round through the front, fails it.
#[test]
fn a_bearing_step_across_the_seam_ramps_along_the_short_arc() {
    for rate in [SampleRate::SR_48K, SampleRate::SR_44K1, SampleRate::SR_96K] {
        let mut node = prepared(vbap::<f32>(tutti_buffers::Bed::Quad), rate, GRID);
        let params = node.param_set();
        params.set(UnitParam::StereoWidth, 0.0);
        params.set(UnitParam::Azimuth, 170.0);
        let ones = [1.0f32; GRID];
        // Seated on 170 by the first segment, then the step.
        drive(&mut node, rate, Planes::new(&[&ones, &ones]).unwrap(), &[]);
        params.set(UnitParam::Azimuth, -170.0);

        let n = ramp_steps(rate);
        let (a, b) = (
            solve(Bed::Quad, Azimuth::new(170.0)),
            solve(Bed::Quad, Azimuth::new(-170.0)),
        );
        let mut worst_chord_gap = 0.0f64;
        for j in 1..=n + 2 {
            let out = drive(&mut node, rate, Planes::new(&[&ones, &ones]).unwrap(), &[]);
            for front in [0, 1] {
                assert!(out[front].iter().all(|&s| s == 0.0), "{rate:?} point {j}");
            }
            let bearing = Azimuth::new(170.0 + 20.0 * j.min(n) as f64 / n as f64).wrap();
            let want = solve(Bed::Quad, bearing);
            for c in 0..4 {
                let want = want[c].get() as f32;
                assert!(
                    (out[c][GRID - 1] - want).abs() < 1e-4,
                    "{rate:?} point {j}, channel {c}: {} against {want}",
                    out[c][GRID - 1],
                );
            }
            worst_chord_gap =
                worst_chord_gap.max((0.5 * (a[2].get() + b[2].get()) - want[2].get()).abs());
        }
        assert!(worst_chord_gap > 0.01, "the chord of the ends is the arc");
    }
}

/// A control written between two grid points is read at the next one: the
/// frames up to it hold the old gains bit for bit, and the frame after it is
/// step 1 of 16 of the way from them to the gains solved for the bearing one
/// grid step into the ramp (`90 / N` degrees).
#[test]
fn a_move_between_grid_points_starts_at_the_next_one() {
    let ones = [1.0f32; 24];
    let mut node = prep(vbap::<f32>(tutti_buffers::Bed::Surround51));
    node.param_set().set(UnitParam::StereoWidth, 0.0);
    // Frames 0..8 of the first segment, then the write, then the rest.
    let head = drive(
        &mut node,
        RATE,
        Planes::new(&[&ones[..8], &ones[..8]]).unwrap(),
        &[],
    );
    aim(&node.param_set(), Azimuth::new(90.0), Elevation::LEVEL);
    let tail = drive(&mut node, RATE, Planes::new(&[&ones, &ones]).unwrap(), &[]);
    let old = solve(Bed::Surround51, Azimuth::new(0.0));
    let one_step = solve(
        Bed::Surround51,
        Azimuth::new(90.0 / ramp_steps(RATE) as f64),
    );
    let speakers = speaker_of_channel(Bed::Surround51);
    for (c, speaker) in speakers.iter().enumerate() {
        let held = head[c][0].to_bits();
        assert!(head[c].iter().all(|s| s.to_bits() == held), "channel {c}");
        assert!(
            tail[c][..8].iter().all(|s| s.to_bits() == held),
            "channel {c} moved before the next grid point"
        );
        let Some(s) = *speaker else { continue };
        let want = (old[s].get() + (one_step[s].get() - old[s].get()) / GRID as f64) as f32;
        assert!(
            (tail[c][8] - want).abs() < 1e-5,
            "channel {c}: first frame {} against {want}",
            tail[c][8]
        );
    }
    assert!(
        one_step[0] > Gain::SILENT,
        "the step must move the left speaker"
    );
}

/// A control signal is followed, not smoothed: at each grid point the direction solved is
/// the control signal's value at that frame, so the last frame of the segment plays those
/// gains however fast the control signal moves, with no lag beyond the grid period. The
/// control signal runs at 0.3 degrees a frame, through the rear and across the seam, on a
/// quad whose gains change along the way; a 50 ms smoothing would trail it by
/// tens of degrees.
#[test]
fn a_signal_driven_bearing_is_followed_to_the_grid_period() {
    let mut node = prep(vbap::<f32>(tutti_buffers::Bed::Quad));
    node.param_set().set(UnitParam::StereoWidth, 0.0);
    let ones = [1.0f32; GRID];
    // The control signal is the graph's `f32`: the bearing narrows into it.
    let signal_at = |frame: usize| Azimuth::new(100.0 + 0.3 * frame as f64).wrap().get() as f32;
    let mut crossed = false;
    for j in 0..60 {
        let signal: Vec<f32> = (0..GRID).map(|i| signal_at(j * GRID + i)).collect();
        crossed |= signal[0] > signal[GRID - 1];
        let out = drive(
            &mut node,
            RATE,
            Planes::new(&[&ones, &ones]).unwrap(),
            &[Some(&signal), None, None, None],
        );
        let want = solve(Bed::Quad, Azimuth::new(f64::from(signal[0])));
        for c in 0..4 {
            let want = want[c].get() as f32;
            assert!(
                (out[c][GRID - 1] - want).abs() < 1e-4,
                "point {j}, channel {c}: {} against {want}",
                out[c][GRID - 1],
            );
        }
    }
    assert!(crossed, "the signal never crossed the seam");
}

/// In an `f64` graph the panner is linear in its input to the rounding of `f64`: the gains are
/// constants of the grid, so the render of a mixture is the mixture of the renders.
#[test]
fn in_an_f64_graph_the_panner_is_linear_on_wide_signals() {
    crate::test_support::assert_linear(
        || vbap::<f64>(tutti_buffers::Bed::Stereo),
        SampleRate::SR_48K,
        2,
        1_024,
    );
}

/// A panner over `bed`.
fn vbap<F: tutti_buffers::Float>(bed: tutti_buffers::Bed) -> crate::VbapPannerNode<F> {
    crate::VbapPannerNode::try_from(crate::VbapPannerConfig::new(bed.into()))
        .expect("a bed with speakers to pan between")
}
