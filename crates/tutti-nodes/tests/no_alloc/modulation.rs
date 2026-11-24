//! The modulation rows: the LFO, the delay and the modulated delays held, with every control
//! moved and with their param ports fed per frame.

use crate::harness::{Gate, bus, feeds};
use tutti_buffers::ChannelTopology;
use tutti_nodes::{
    DelayConfig, DelayControls, DelayLayout, DelayNode, LfoConfig, LfoNode, LfoShape, LfoSync,
    MaxDelay, ModDelayConfig, ModDelayNode, PhaserConfig, PhaserNode, Stages, Stagger,
};
use tutti_units::{BeatsPerCycle, Feedback, Hz, PhaseIncrement, Seconds, UnitParam};

/// The rows of this file.
pub const ROWS: &[fn()] = &[lfo, held, controls_moving, ports_fed];

/// A delay at `time` seconds and `feedback`, `cross` cross-fed, `width` wide, holding up to
/// two seconds.
fn delay(width: u16, time: f64, feedback: f64, cross: f64) -> DelayNode<f32> {
    DelayNode::<f32>::from(
        DelayConfig::new(DelayLayout::new(
            bus(width),
            MaxDelay::of(Seconds::new(2.0)),
        ))
        .controls(DelayControls {
            sync: tutti_nodes::DelaySync::Free,
            time: Seconds::new(time),
            feedback: Feedback::new(feedback),
            cross_feedback: Feedback::new(cross),
        }),
    )
}

/// A phaser `width` wide of `stages`, staggered by `step`.
fn phaser(width: u16, stages: usize, step: f64) -> PhaserNode<f32> {
    PhaserNode::<f32>::try_from(
        PhaserConfig::new(bus(width), Stages::of(stages))
            .stagger(Stagger::of_step(PhaseIncrement::new(step))),
    )
    .expect("a step fits any width")
}

/// The free-running sine, the random-smooth shape (hashed on its own path, fast enough to
/// step inside the warm-up) and the beat-synced path, which walks the block's `Env` frame by
/// frame; its quarter-cycle offset puts the stopped transport's beat 0 on the sine's peak.
/// The synced node's sync word is stored every 20 blocks (under the 2400-frame ramp's 37.5),
/// so the measured blocks run both clocks, the ramp, and a switch inside the ramp.
fn lfo() {
    let gate = Gate::new(feeds::silence).warm(32).blocks(5_000);
    let free = LfoConfig::new(LfoShape::Sine).rate(Hz::new(2.0_f64));
    gate.run("lfo sine", LfoNode::from(free), |_, _| {});
    let random = LfoConfig::new(LfoShape::RandomSmooth).rate(Hz::new(200.0_f64));
    gate.run("lfo random", LfoNode::from(random), |_, _| {});
    let beat = LfoConfig::new(LfoShape::Sine)
        .beats_per_cycle(BeatsPerCycle::of(4.0))
        .sync(LfoSync::Synced)
        .phase_offset(PhaseIncrement::new(0.25));
    let node = LfoNode::from(beat);
    let sync = tutti_graph::ParamNode::<f32>::settings(&node)
        .into_iter()
        .map(|setting| std::sync::Arc::clone(setting.cell()))
        .next()
        .expect("an LFO has a sync setting");
    gate.run("lfo beat-synced, switching", node, |_, i| {
        sync.store(if (i / 20) % 2 == 0 {
            LfoSync::Synced
        } else {
            LfoSync::Free
        });
    });
}

/// The delay is shorter than the warm-up so its wet-only output is audible at the check; the
/// per-channel lines of the 6-wide path are built at construction.
fn held() {
    let gate = |amplitude| {
        Gate::new(feeds::alternating(amplitude))
            .warm(32)
            .blocks(2_000)
    };
    for width in [1, 2, 6] {
        gate(0.5).run(
            &format!("delay {width}ch"),
            delay(width, 0.02, 0.4, 0.0),
            |_, _| {},
        );
    }
    let chorus = ModDelayConfig::chorus(ChannelTopology::STEREO);
    gate(0.4).run("chorus", ModDelayNode::<f32>::from(chorus), |_, _| {});
    let flanger = ModDelayConfig::flanger(ChannelTopology::STEREO);
    gate(0.4).run("flanger", ModDelayNode::<f32>::from(flanger), |_, _| {});
    let general_midi = ModDelayConfig::general_midi_chorus(ChannelTopology::STEREO);
    gate(0.4).run(
        "general midi chorus",
        ModDelayNode::<f32>::from(general_midi),
        |_, _| {},
    );
    let four_stage = PhaserConfig::new(ChannelTopology::MONO, Stages::of(4));
    gate(0.4).run(
        "phaser",
        PhaserNode::<f32>::try_from(four_stage).unwrap(),
        |_, _| {},
    );
}

/// Every control moves each block: the delay time ramps at its slew, the chorus depth sweeps,
/// the phaser's control points are re-solved, the cross-fed delay reads every tap before
/// writing any line, all on preallocated scratch.
fn controls_moving() {
    let gate = Gate::new(feeds::alternating(0.5))
        .warm(32)
        .blocks(2_000)
        .warm_moves();
    let t = |i: usize| (i % 50) as f32 / 50.0;
    // A cross-fed stereo delay whose time and both feedbacks move, the ramps in flight on
    // every block.
    gate.run("delay moving", delay(2, 0.01, 0.4, 0.3), |c, i| {
        assert!(c.set(UnitParam::DelayTime, f64::from(0.005 + 0.03 * t(i))));
        assert!(c.set(UnitParam::Feedback, f64::from(0.2 + 0.5 * t(i))));
        assert!(c.set(UnitParam::CrossFeedback, f64::from(0.4 * (1.0 - t(i)))));
    });
    // A 6-wide chorus: depth and feedback ramps in flight, the rate landing whole at the grid.
    let chorus = ModDelayNode::<f32>::from(ModDelayConfig::chorus(bus(6)));
    gate.run("chorus moving", chorus, |c, i| {
        assert!(c.set(UnitParam::Depth, f64::from(0.002 + 0.01 * t(i))));
        assert!(c.set(UnitParam::Rate, f64::from(0.5 + 4.0 * t(i))));
        assert!(c.set(UnitParam::Feedback, f64::from(0.6 * t(i))));
    });
    gate.run("phaser moving", phaser(6, 8, 0.2), |c, i| {
        assert!(c.set(UnitParam::Rate, f64::from(0.5 + 4.0 * t(i))));
        assert!(c.set(UnitParam::Depth, f64::from(t(i))));
        assert!(c.set(UnitParam::Feedback, f64::from(0.9 * t(i))));
    });
}

/// The delay's time and feedback, the chorus's and the phaser's rate, depth and feedback
/// control signals fed per frame, at widths 2 and 6: each is read at the grid's solve frame,
/// the ramps move on every solve, and the staggered chain renders every channel's sweep.
fn ports_fed() {
    let tau = std::f32::consts::TAU / 64.0;
    let signal = |base: f32, swing: f32, k: f32| -> Vec<f32> {
        (0..64)
            .map(|i| base + swing * (tau * k * i as f32).sin())
            .collect()
    };
    let gate = Gate::new(feeds::alternating(0.5)).warm(32).blocks(2_000);
    for width in [2u16, 6] {
        gate.fed(
            &format!("delay fed {width}ch"),
            delay(width, 0.01, 0.4, 0.3),
            &[signal(0.01, 0.004, 1.0), signal(0.4, 0.3, 3.0)],
        );
        gate.fed(
            &format!("chorus fed {width}ch"),
            ModDelayNode::<f32>::from(ModDelayConfig::chorus(bus(width))),
            &[
                signal(2.0, 1.5, 1.0),
                signal(0.006, 0.004, 2.0),
                signal(0.4, 0.3, 3.0),
            ],
        );
        gate.fed(
            &format!("phaser fed {width}ch"),
            phaser(width, 6, 0.25),
            &[
                signal(2.0, 1.5, 1.0),
                signal(0.5, 0.5, 2.0),
                signal(0.5, 0.4, 3.0),
            ],
        );
    }
}
