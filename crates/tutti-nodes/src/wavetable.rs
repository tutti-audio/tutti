//! The wavetable source: [`WavetableNode`], a mipmapped [`Wavetable`] played at a frequency,
//! a level and a frame position, made from a [`WavetableConfig`].
//!
//! The node holds the params, the sample rate and the shared table; the algorithm is
//! `tutti_dsp::WavetableOscillator`.

use crate::ramp::LastGood;
use core::num::NonZeroU16;
use std::sync::Arc;
use tutti_buffers::{ChannelLayout, Float};
use tutti_dsp::{Wavetable, WavetableOscillator, WavetableStep};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_rt::Param;
use tutti_units::{Amplitude, Hz, Normalized, SampleRate, Tail, UnitParam};

/// A wavetable oscillator: a source with no inputs and `width` output channels that each
/// carry the one signal.
///
/// The table is shared (`Arc`) by the node and every fork; the level of its mipmap is
/// chosen from the frequency so that no harmonic of the table lies above Nyquist, and a
/// sweep through a level boundary is a crossfade (see `tutti_dsp::Wavetable` for the
/// interpolation error bound). A frequency outside `0..=Nyquist` is clamped for the step and
/// one that is not finite holds the phase.
///
/// Its params are the frequency ([`UnitParam::Frequency`], in [`Hz`]), the level
/// ([`UnitParam::Volume`], in [`Amplitude`], a linear gain on the table) and the frame
/// position ([`UnitParam::Position`], a [`Normalized`] position along the table's frames),
/// all read per frame: a driven control signal's every frame is heard, and a non-finite
/// driven value holds the last finite one. Made from a [`WavetableConfig`]
/// (`WavetableNode::from`); inserted, its controls are a [`ParamSet`]; a fork starts from the
/// values last set through it, at phase 0.
///
/// The node renders silence until the graph prepares it at the device rate
/// ([`Node::prepare`]). It is a source with no ring-out ([`Tail::None`]).
///
/// # Examples
///
/// ```
/// # use tutti_buffers::{Mono, Planes};
/// use tutti_dsp::Wavetable;
/// use tutti_graph::{GraphBuilder, Params, Prepare};
/// use tutti_nodes::{WavetableConfig, WavetableNode};
/// use tutti_units::{Hz, SampleRate, Samples};
/// use tutti_buffers::{ChannelLayout, MaxBlock};
///
/// let cycle: Vec<f32> = (0..64)
///     .map(|i| (core::f32::consts::TAU * i as f32 / 64.0).sin())
///     .collect();
/// let table = Wavetable::new(&[Mono::new(&cycle)]).expect("one frame");
/// let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
/// let tone = g.add_with_controls(Params(WavetableNode::from(
///     WavetableConfig::new(NonZeroU16::MIN, table).frequency(Hz::new(440.0)),
/// )));
/// g.pipe_output(tone.0);
/// let mut r = g
///     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))))
///     .expect("builds");
/// let out = r.render_input(Planes::new(&[&[0.0; 64]]).unwrap());
/// assert!(out[0][0].abs() < 1e-6);
/// assert!(out[0][1] > 0.0);
/// ```
#[derive(Debug)]
pub struct WavetableNode<F: Float> {
    width: ChannelLayout,
    wave: WavetableOscillator<F>,
    frequency: Param<Hz>,
    level: Param<Amplitude>,
    position: Param<Normalized>,
    /// `None` until [`Node::prepare`]: no value is computed from a guessed rate.
    rate: Option<SampleRate>,
    /// The last finite frequency read from the cell or the port.
    good_frequency: LastGood<f64>,
    /// The last finite level read from the cell or the port.
    good_level: LastGood<f64>,
    /// The last finite position read from the cell or the port.
    good_position: LastGood<f64>,
}

// The params a [`WavetableNode`] lets the graph modulate, in port order.
tutti_graph::declare_params!([F: Float] WavetableNode<F>, n {
    Frequency => Hz = &n.frequency,
    Volume => Amplitude = &n.level,
    Position => Normalized = &n.position,
});

/// What a [`WavetableNode`] is made from: its width, its table and its starting params.
#[derive(Clone, Debug)]
pub struct WavetableConfig<F: Float> {
    width: NonZeroU16,
    table: Arc<Wavetable<F>>,
    frequency: Hz,
    level: Amplitude,
    position: Normalized,
}

impl<F: Float> WavetableConfig<F> {
    /// `table` on `width` channels, at 440 Hz, unity level and the first frame.
    #[must_use]
    pub fn new(width: NonZeroU16, table: Arc<Wavetable<F>>) -> Self {
        Self {
            width,
            table,
            frequency: Hz::new(440.0),
            level: Amplitude::UNITY,
            position: Normalized::of(0.0),
        }
    }

    /// The frequency, at the start.
    #[must_use]
    pub const fn frequency(mut self, frequency: Hz) -> Self {
        self.frequency = frequency;
        self
    }

    /// The level the output is scaled by, at the start.
    #[must_use]
    pub const fn level(mut self, level: Amplitude) -> Self {
        self.level = level;
        self
    }

    /// The frame position, at the start.
    #[must_use]
    pub const fn position(mut self, position: Normalized) -> Self {
        self.position = position;
        self
    }
}

impl<F: Float> From<WavetableConfig<F>> for WavetableNode<F> {
    fn from(config: WavetableConfig<F>) -> Self {
        Self {
            width: ChannelLayout::from_count(config.width.get()),
            wave: WavetableOscillator::new(config.table),
            frequency: Param::new(config.frequency),
            level: Param::new(config.level),
            position: Param::new(config.position),
            rate: None,
            good_frequency: LastGood::new(config.frequency.get(), 0.0),
            good_level: LastGood::new(Amplitude::UNITY.get(), 1.0),
            good_position: LastGood::new(config.position.get(), 0.0),
        }
    }
}

impl<F: Float> WavetableNode<F> {
    /// A handle on the frequency cell, for a UI or automation to share.
    #[must_use]
    pub fn frequency(&self) -> Param<Hz> {
        self.frequency.handle()
    }

    /// A handle on the level cell, for a UI or automation to share.
    #[must_use]
    pub fn level(&self) -> Param<Amplitude> {
        self.level.handle()
    }

    /// A handle on the frame position cell, for a UI or automation to share.
    #[must_use]
    pub fn position(&self) -> Param<Normalized> {
        self.position.handle()
    }
}

/// Shares the cells (the template [`tutti_graph::param_parts`] forks from) and the table.
impl<F: Float> Clone for WavetableNode<F> {
    fn clone(&self) -> Self {
        Self {
            width: self.width,
            wave: self.wave.clone(),
            frequency: self.frequency.handle(),
            level: self.level.handle(),
            position: self.position.handle(),
            rate: self.rate,
            good_frequency: self.good_frequency,
            good_level: self.good_level,
            good_position: self.good_position,
        }
    }
}

impl<F: Float> Node<F> for WavetableNode<F> {
    /// No inputs, `width` outputs, no latency, no ring-out; four table reads and a blend a
    /// frame at most, `Light`.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.width)
            .with_cost(tutti_graph::Cost::Light)
            .with_tail(Tail::None)
            .with_param_addrs(self.declared_ports().as_slice())
    }

    fn prepare(&mut self, prepare: &Prepare) {
        self.rate = Some(prepare.sample_rate());
    }

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let Some(rate) = self.rate else {
            return Status::Silent;
        };
        let size = io.frames();
        let frequency = self
            .declared_param::<{ UnitParam::Frequency as u16 }, _>(&io)
            .frames();
        let level = self
            .declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
            .frames();
        let position = self
            .declared_param::<{ UnitParam::Position as u16 }, _>(&io)
            .frames();
        let mut hz = self.good_frequency.read(self.frequency.load().get());
        let mut at = self.good_position.read(self.position.load().get());
        let table = Arc::clone(self.wave.table());
        let design = |hz: f64, at: f64| {
            WavetableStep::new(&table, Hz::new(hz), rate, Normalized::clamped(at))
        };
        // A frequency, a position and a level that hold are read, and cross into `F`, once
        // per block; a control signal's value replaces them at each frame, and the step is
        // designed again only where the frequency or the position moved.
        let mut step = design(hz, at);
        let mut gain = if level.is_none() {
            F::from_unit(Amplitude::new(
                self.good_level.read(self.level.load().get()),
            ))
        } else {
            F::ZERO
        };
        let mut outputs = io.split().1;
        let mut channels = outputs.iter_mut();
        let Some(first) = channels.next() else {
            return Status::Silent;
        };
        for (i, o) in first[..size].iter_mut().enumerate() {
            let mut moved = false;
            if let Some(signal) = frequency {
                let f = self.good_frequency.read(signal[i].into());
                moved |= f != hz;
                hz = f;
            }
            if let Some(signal) = position {
                let p = self.good_position.read(signal[i].into());
                moved |= p != at;
                at = p;
            }
            if moved {
                step = design(hz, at);
            }
            if let Some(signal) = level {
                // A finite control signal value is the level in `F` as it is.
                let good = self.good_level.read(signal[i].into());
                gain = if signal[i].is_finite() {
                    signal[i]
                } else {
                    F::from_unit(Amplitude::new(good))
                };
            }
            *o = self.wave.step(step) * gain;
        }
        for other in channels {
            other[..size].copy_from_slice(&first[..size]);
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.wave.reset();
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl<F: Float> ParamNode<F> for WavetableNode<F> {
    /// The frequency, the level and the frame position.
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A clone with its cells detached (at their values now), at phase 0.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.frequency.detach();
        fork.level.detach();
        fork.position.detach();
        Node::<F>::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values last set through
/// it ([`tutti_graph::param_parts`]).
impl<F: Float> IntoNode<F> for WavetableNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests;
