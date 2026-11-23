//! The noise source: [`NoiseNode`], white, pink or brown noise at a level, made from a
//! [`NoiseConfig`].
//!
//! The node holds the level cell and the generator; the algorithm is `tutti_dsp::Noise`.

use crate::last_good::LastGood;
use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, MonoMut};
use tutti_dsp::{Noise, NoiseColour};
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_rt::Param;
use tutti_units::{Amplitude, Tail, UnitParam};

/// A noise source: no inputs and one (mono) output of white, pink or brown noise
/// ([`NoiseColour`]) scaled by a level.
///
/// The stream is `tutti_dsp::Noise` of the seed the config names: the same seed gives the
/// same samples from the start of the node, and a [reset](Node::reset) or a fork starts it
/// again. Its one param is the level ([`UnitParam::Volume`], in [`Amplitude`], a linear gain
/// on the generator's own scale), read per frame: a driven control signal's every frame is heard,
/// and a non-finite value holds the last finite one. Made from a [`NoiseConfig`]
/// (`NoiseNode::from`); inserted, its controls are a [`ParamSet`](tutti_graph::ParamSet).
///
/// A source with no ring-out ([`Tail::None`]): what it emits is a function of its seed, so a
/// render's end ends it.
///
/// # Examples
///
/// ```
/// # use tutti_buffers::Planes;
/// use tutti_graph::{GraphBuilder, Prepare};
/// use tutti_nodes::{NoiseColour, NoiseConfig, NoiseNode};
/// use tutti_units::{Amplitude, SampleRate, Samples};
/// use tutti_buffers::{ChannelLayout, MaxBlock};
///
/// let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
/// let hiss = g.add_with_controls(NoiseNode::<f32>::from(
///     NoiseConfig::new(NoiseColour::White).level(Amplitude::new(0.5)).seed(7),
/// ));
/// g.pipe_output(hiss.0);
/// let mut r = g
///     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))))
///     .expect("builds");
/// let out = r.render_input(Planes::new(&[&[0.0; 64]]).unwrap());
/// assert!(out[0].iter().all(|s| s.abs() < 0.5));
/// ```
#[derive(Clone, Debug)]
pub struct NoiseNode<F: Float> {
    noise: Noise<F>,
    cells: NoiseCells,
    /// The last finite level read from the cell or the port.
    good_level: LastGood<f64>,
}

// The params a [`NoiseNode`] lets the graph modulate: its level; and the
// [`NoiseConfig`] it is made from: its colour, its seed and its level.
tutti_graph::declare_params!([F: Float] NoiseNode<F>, #[derive(Copy, PartialEq)] NoiseConfig, NoiseCells {
    required { colour: NoiseColour }
    params { Volume => level: Amplitude = Amplitude::UNITY }
    options { seed: u64 = 0 }
});

impl<F: Float> From<NoiseConfig> for NoiseNode<F> {
    fn from(config: NoiseConfig) -> Self {
        Self {
            noise: Noise::new(config.colour, config.seed),
            cells: NoiseCells::from_config(&config),
            good_level: LastGood::new(config.level.get(), Amplitude::UNITY.get()),
        }
    }
}

impl<F: Float> NoiseNode<F> {
    /// A handle on the level cell, for a UI or automation to share.
    #[must_use]
    pub fn level(&self) -> Param<Amplitude> {
        self.cells.level.handle()
    }
}

impl<F: Float> Node<F> for NoiseNode<F> {
    /// No inputs, one output, no latency, no ring-out; arithmetic only, `Light`.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .cost(tutti_graph::Cost::Light)
            .tail(Tail::None)
            .param_addrs(self.declared_ports().as_slice())
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let size = io.frames();
        let level = self
            .declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
            .frames();
        let cell = self.cells.level.load().get();
        let out = &mut io.output(0)[..size];
        self.noise.process(MonoMut::new(out));
        match level {
            // A level that holds crosses into `F` once per block.
            None => {
                let gain = self.good_level.level(cell);
                for o in out {
                    *o *= gain;
                }
            }
            Some(signal) => {
                for (o, &l) in out.iter_mut().zip(signal) {
                    *o *= self.good_level.driven_level(l);
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.noise.reset();
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

#[cfg(test)]
mod tests;
