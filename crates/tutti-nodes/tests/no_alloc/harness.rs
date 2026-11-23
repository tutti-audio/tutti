//! The driver every table row runs: one node alone through `BlockRig`, warmed up, then
//! stepped under `assert_no_alloc` with its controls moved before each block.

use std::cell::Cell;
use tutti_buffers::{BusTopology, ChannelLayout, Planes, PlanesMut};
use tutti_graph::contract::{BlockRig, Direct};
use tutti_graph::{IntoNode, Node};
use tutti_units::SampleRate;

thread_local! {
    /// The largest output magnitude the measured blocks of this thread's runs produced.
    static PEAK: Cell<f32> = const { Cell::new(0.0) };
}

/// The largest output magnitude the runs since the last call produced; resets it.
pub fn take_peak() -> f32 {
    PEAK.with(|p| p.replace(0.0))
}

/// A sample of an input signal: `(channel, frame)`.
pub type Feed = Box<dyn Fn(usize, usize) -> f32>;

/// The input signals the rows feed.
pub mod feeds {
    /// The input left silent: a source node.
    pub fn silence(_: usize, _: usize) -> f32 {
        0.0
    }

    /// An eight-frame-half-period square of amplitude `0.5 * (channel + 1) * 0.5` on a
    /// `0.25` offset: asymmetric, with a DC part, and different on each channel.
    pub fn offset_square(channel: usize, frame: usize) -> f32 {
        0.25 + square(channel, frame)
    }

    /// The eight-frame-half-period square of amplitude `0.25 * (channel + 1)`.
    pub fn square(channel: usize, frame: usize) -> f32 {
        let side = if (frame / 8).is_multiple_of(2) {
            0.5
        } else {
            -0.5
        };
        side * (channel as f32 + 1.0) * 0.5
    }

    /// A four-frame-half-period square of amplitude `0.5 * (1 + 0.1 * channel)`: broadband
    /// enough to drive a band at several centres.
    pub fn bright_square(channel: usize, frame: usize) -> f32 {
        let side = if (frame / 4).is_multiple_of(2) {
            0.5
        } else {
            -0.5
        };
        side * (1.0 + 0.1 * channel as f32)
    }

    /// The square at a tenth of the level, for a node that gates on level.
    pub fn quiet_square(channel: usize, frame: usize) -> f32 {
        let side = if (frame / 8).is_multiple_of(2) {
            0.05
        } else {
            -0.05
        };
        side * (channel as f32 + 1.0)
    }

    /// A sine of `0.07` rad a frame times `channel + 1`.
    pub fn sine(channel: usize, frame: usize) -> f32 {
        ((frame as f32) * 0.07 * (channel as f32 + 1.0)).sin() * 0.8
    }

    /// The `0.07` rad sine, the same on every channel.
    pub fn shared_sine(_: usize, frame: usize) -> f32 {
        sine(0, frame)
    }

    /// An alternating `+amplitude`, `-amplitude` on every channel: a signal that keeps an
    /// envelope follower active.
    pub fn alternating(amplitude: f32) -> impl Fn(usize, usize) -> f32 {
        move |_, frame| {
            if frame.is_multiple_of(2) {
                amplitude
            } else {
                -amplitude
            }
        }
    }

    /// A square of half-period 16 frames and `amplitude`: slow enough that a low-pass passes
    /// some of it, and broadband enough to drive every band of a splitter.
    pub fn slow_square(amplitude: f32) -> impl Fn(usize, usize) -> f32 {
        move |_, frame| {
            if (frame / 16).is_multiple_of(2) {
                amplitude
            } else {
                -amplitude
            }
        }
    }

    /// A unit impulse every 32 frames.
    pub fn pulse(_: usize, frame: usize) -> f32 {
        if frame.is_multiple_of(32) { 1.0 } else { 0.0 }
    }

    /// A 13-frame sawtooth of amplitude `0.9`, shifted `7` frames a channel: hot enough to
    /// drive a saturator.
    pub fn ramp(channel: usize, frame: usize) -> f32 {
        0.9 * (((frame + 7 * channel) % 13) as f32 / 6.0 - 1.0)
    }
}

/// How one row steps its node.
pub struct Gate {
    feed: Feed,
    frames: usize,
    warm: usize,
    floor: f32,
    blocks: usize,
    every_channel: bool,
}

impl Gate {
    /// A gate feeding `feed`: blocks of 64 frames, 16 warm-up blocks (`warm`), then 240 measured, the
    /// first channel's output past `1e-6` after the warm-up.
    pub fn new(feed: impl Fn(usize, usize) -> f32 + 'static) -> Self {
        Self {
            feed: Box::new(feed),
            frames: 64,
            warm: 16,
            floor: 1e-6,
            blocks: 240,
            every_channel: false,
        }
    }

    /// Blocks of `frames` frames.
    pub fn frames(mut self, frames: usize) -> Self {
        self.frames = frames;
        self
    }

    /// The output must pass `floor` in magnitude after the warm-up.
    pub fn floor(mut self, floor: f32) -> Self {
        self.floor = floor;
        self
    }

    /// `warm` warm-up blocks.
    pub fn warm(mut self, warm: usize) -> Self {
        self.warm = warm;
        self
    }

    /// `blocks` measured blocks.
    pub fn blocks(mut self, blocks: usize) -> Self {
        self.blocks = blocks;
        self
    }

    /// Every output channel, not the first alone, must sound.
    pub fn every_channel(mut self) -> Self {
        self.every_channel = true;
        self
    }

    /// `node` alone, labelled `label` in a failure: the warm-up blocks run with `moves`
    /// applied before each, the output must sound, then the measured blocks run under
    /// `assert_no_alloc` with `moves` applied before each.
    pub fn run<N: IntoNode>(
        &self,
        label: &str,
        node: N,
        mut moves: impl FnMut(&N::Controls, usize),
    ) {
        let (mut rig, controls) = BlockRig::new(node, SampleRate::SR_48K, self.frames);
        self.fill(rig.inputs_mut());
        for i in 0..self.warm {
            moves(&controls, i);
            rig.block();
        }
        self.sounds(label, rig.output());
        let mut peak = 0.0f32;
        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..self.blocks {
                moves(&controls, i);
                rig.block();
                peak = peak_of(rig.output(), peak);
            }
        });
        record(peak);
    }

    /// `node` through `Direct`, which feeds its param ports: port `k` plays `signals[k]` (one
    /// value per frame, copied at the feed, so the same frames play each block). Warmed up and
    /// stepped as [`run`](Self::run) does, with no control moved.
    pub fn fed<N: Node>(&self, label: &str, node: N, signals: &[Vec<f32>]) {
        let mut rig = Direct::new(node, SampleRate::SR_48K, self.frames);
        self.fill(rig.inputs_mut());
        for (k, signal) in signals.iter().enumerate() {
            rig.feed(k, Some(signal));
        }
        for _ in 0..self.warm {
            rig.block();
        }
        self.sounds(label, rig.output());
        let mut peak = 0.0f32;
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..self.blocks {
                rig.block();
                peak = peak_of(rig.output(), peak);
            }
        });
        record(peak);
    }

    /// Writes the feed into every input channel.
    fn fill(&self, mut inputs: PlanesMut<'_, '_, f32>) {
        for (c, ch) in inputs.iter_mut().enumerate() {
            for (i, x) in ch.into_samples_mut().iter_mut().enumerate() {
                *x = (self.feed)(c, i);
            }
        }
    }

    /// The checked output channels must pass the floor.
    fn sounds(&self, label: &str, output: Planes<'_, f32>) {
        let checked = if self.every_channel { usize::MAX } else { 1 };
        for (c, plane) in output.iter().enumerate().take(checked) {
            assert!(
                plane.samples().iter().any(|s| s.abs() > self.floor),
                "{label}: channel {c} rendered silence; the gate would walk no DSP"
            );
        }
    }
}

/// `a` on even `i`, `b` on odd.
pub fn alternate(i: usize, a: f64, b: f64) -> f64 {
    if i.is_multiple_of(2) { a } else { b }
}

/// `a` on every third `i`, `b` otherwise.
pub fn every_third(i: usize, a: f64, b: f64) -> f64 {
    if i.is_multiple_of(3) { a } else { b }
}

/// The largest of `peak` and the output's magnitudes.
fn peak_of(output: Planes<'_, f32>, peak: f32) -> f32 {
    output.iter().fold(peak, |m, plane| {
        plane.samples().iter().fold(m, |m, s| m.max(s.abs()))
    })
}

/// Adds a run's `peak` to the thread's [`take_peak`].
fn record(peak: f32) {
    PEAK.with(|p| p.set(p.get().max(peak)));
}

/// A bus of `width` channels on the engine's order.
pub fn bus(width: u16) -> BusTopology {
    BusTopology::default_for(ChannelLayout::from(width)).expect("a test width is one bus")
}
