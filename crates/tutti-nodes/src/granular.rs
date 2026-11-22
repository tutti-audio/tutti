//! The granular source: [`GranularNode`], a cloud of grains read from a shared source,
//! made from a [`GranularConfig`].
//!
//! The node holds the params and the rate; the algorithm is `tutti_dsp::Granular`.

use std::sync::Arc;

use crate::last_good::LastGood;
use tutti_buffers::{ChannelLayout, Float, Wave};
use tutti_dsp::{GrainSettings, Granular, MaxGrains};
use tutti_graph::{
    Cx, IntoNode, Io, Node, NodeParts, ParamInput, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_rt::Param;
use tutti_units::{Cents, Hz, Normalized, Samples, Seconds, Tail, Unit, UnitParam};

/// A granular source: no inputs and a layout's channels as output, grains of the shared source
/// started at a density. A mono source feeds every output channel; a wider source has as many
/// channels as the layout, each feeding its own.
///
/// Its params are read once per block, a driven control signal by its last frame, and a
/// non-finite value holds the last finite one:
/// - the density in grains a second (`UnitParam::Rate`, [`Hz`]);
/// - the playback rate (`UnitParam::Pitch`, [`Cents`]);
/// - the grain length (`UnitParam::GrainLength`, [`Seconds`]);
/// - the jitter (`UnitParam::Jitter`, [`Normalized`]): index 0 the position's, a fraction of
///   the source, and index 1 the timing's, a fraction of the interval between grains.
///
/// - the position the grains start from (`UnitParam::Position`, [`Normalized`]), `0` the
///   start of the source and `1` its end.
///
/// Every param is read when a grain starts.
///
/// The cloud is built when the graph prepares the node at the device rate; before that it
/// renders silence. It has no inputs and no ring-out ([`Tail::None`]): what it emits is a
/// function of its seed and its clock, so a render's end ends it. A reset or a fork starts
/// the cloud again from the seed. Grains that find every slot busy are not started
/// ([`Granular::dropped`]).
///
/// # Examples
///
/// ```
/// # use tutti_buffers::Planes;
/// use std::sync::Arc;
/// use tutti_buffers::{ChannelLayout, Interleaved, MaxBlock, Wave};
/// use tutti_graph::{GraphBuilder, Params, Prepare};
/// use tutti_nodes::{GranularConfig, GranularNode};
/// use tutti_units::{Hz, SampleRate, Samples, Seconds};
///
/// let mut source = Wave::<f32>::new(1, SampleRate::SR_48K);
/// source.append(Interleaved::new(&[0.5; 4800], ChannelLayout::MONO)?)?;
/// let cloud = GranularNode::<f32>::try_from(
///     GranularConfig::new(ChannelLayout::MONO, Arc::new(source))
///         .density(Hz::new(100.0))
///         .length(Seconds::new(0.01)),
/// )?;
/// let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
/// let node = g.add_with_controls(Params(cloud));
/// g.pipe_output(node.0);
/// let mut r = g
///     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(480))))
///     .expect("builds");
/// let out = r.render_input(Planes::new(&[&[0.0; 480]]).unwrap());
/// assert!(out[0].iter().any(|&x| x > 0.4));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct GranularNode<F: Float> {
    layout: ChannelLayout,
    source: Arc<Wave<F>>,
    max_grains: MaxGrains,
    seed: u64,
    density: Param<Hz>,
    rate: Param<Cents>,
    length: Param<Seconds>,
    /// The position's jitter at 0 and the timing's at 1.
    jitter: [Param<Normalized>; 2],
    position: Param<Normalized>,
    /// The last finite value of each cell or port, in the order of [`Self::settings`]'s reads.
    good: [LastGood<f64>; 6],
    /// `None` until [`Node::prepare`]: no cloud is built at a guessed rate.
    cloud: Option<Granular<F>>,
}

// The params a [`GranularNode`] lets the graph modulate, in port order.
tutti_graph::declare_params!([F: Float] GranularNode<F>, n {
    Rate => Hz = &n.density,
    Pitch => Cents = &n.rate,
    GrainLength => Seconds = &n.length,
    [i in 0..2u8] { Jitter => Normalized = &n.jitter[usize::from(i)] },
    Position => Normalized = &n.position,
});

/// What a [`GranularNode`] is made from: the shared source and the starting values of its
/// params.
#[derive(Clone, Debug)]
pub struct GranularConfig<F: Float> {
    layout: ChannelLayout,
    source: Arc<Wave<F>>,
    max_grains: MaxGrains,
    seed: u64,
    density: Hz,
    length: Seconds,
    rate: Cents,
    position: Normalized,
    position_jitter: Normalized,
    timing_jitter: Normalized,
}

impl<F: Float> GranularConfig<F> {
    /// Grains of `source` out of `layout`'s channels: 10 a second, 100 ms long, from its start at
    /// its own pitch, with no jitter, at most 32 sounding, seed 0. The node refuses a source
    /// that is neither mono nor as wide as the layout ([`GranularError`]).
    #[must_use]
    pub fn new(layout: ChannelLayout, source: Arc<Wave<F>>) -> Self {
        Self {
            layout,
            source,
            max_grains: MaxGrains::of(32),
            seed: 0,
            density: Hz::new(10.0),
            length: Seconds::new(0.1),
            rate: Cents::new(0.0),
            position: Normalized::of(0.0),
            position_jitter: Normalized::of(0.0),
            timing_jitter: Normalized::of(0.0),
        }
    }

    /// The most grains sounding at once.
    #[must_use]
    pub fn max_grains(mut self, max: MaxGrains) -> Self {
        self.max_grains = max;
        self
    }

    /// The seed of the jitter's stream: the same seed is the same grains.
    #[must_use]
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// The grains started a second, at the start.
    #[must_use]
    pub fn density(mut self, density: Hz) -> Self {
        self.density = density;
        self
    }

    /// The length of a grain, at the start.
    #[must_use]
    pub fn length(mut self, length: Seconds) -> Self {
        self.length = length;
        self
    }

    /// The pitch grains are read at, against the source's own, at the start.
    #[must_use]
    pub fn rate(mut self, rate: Cents) -> Self {
        self.rate = rate;
        self
    }

    /// Where in the source grains start, `0` its start and `1` its end, at the start.
    #[must_use]
    pub fn position(mut self, position: Normalized) -> Self {
        self.position = position;
        self
    }

    /// How far a grain's start moves from the position, as a fraction of the source, at the
    /// start.
    #[must_use]
    pub fn position_jitter(mut self, jitter: Normalized) -> Self {
        self.position_jitter = jitter;
        self
    }

    /// How far an interval between grains moves, as a fraction of the interval, at the start.
    #[must_use]
    pub fn timing_jitter(mut self, jitter: Normalized) -> Self {
        self.timing_jitter = jitter;
        self
    }
}

/// What a [`GranularConfig`] cannot make a node of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GranularError {
    /// The source is wider than one channel and not as wide as the output: no channel of it
    /// is the one an output channel reads.
    #[error(
        "a source of {source_channels} channels cannot feed {output_channels} output channels: it is mono or as wide as the output"
    )]
    Width {
        /// The source's channels.
        source_channels: usize,
        /// The layout's channels.
        output_channels: u16,
    },
}

impl<F: Float> TryFrom<GranularConfig<F>> for GranularNode<F> {
    type Error = GranularError;

    fn try_from(config: GranularConfig<F>) -> Result<Self, GranularError> {
        let source_channels = config.source.channels();
        if source_channels != 1 && source_channels != usize::from(config.layout.count()) {
            return Err(GranularError::Width {
                source_channels,
                output_channels: config.layout.count(),
            });
        }
        let good = |initial: f64, fallback: f64| LastGood::new(initial, fallback);
        Ok(Self {
            layout: config.layout,
            source: config.source,
            max_grains: config.max_grains,
            seed: config.seed,
            good: [
                good(config.density.get(), 10.0),
                good(config.rate.get(), 0.0),
                good(config.length.get(), 0.1),
                good(config.position_jitter.get(), 0.0),
                good(config.timing_jitter.get(), 0.0),
                good(config.position.get(), 0.0),
            ],
            density: Param::new(config.density),
            rate: Param::new(config.rate),
            length: Param::new(config.length),
            jitter: [
                Param::new(config.position_jitter),
                Param::new(config.timing_jitter),
            ],
            position: Param::new(config.position),
            cloud: None,
        })
    }
}

impl<F: Float> GranularNode<F> {
    /// The settings of this block: each param's driven last frame, else its cell, held at its
    /// last finite value.
    fn settings(&mut self, io: &Io<'_, F>) -> GrainSettings {
        fn value<U: Unit<Raw = f64>, F: Float>(
            cell: &Param<U>,
            driven: ParamInput<'_, F>,
            good: &mut LastGood<f64>,
        ) -> U {
            U::from_raw(good.block(driven.last_frame(), cell.load().to_raw()))
        }
        let density = value(
            &self.density,
            self.declared_param::<{ UnitParam::Rate as u16 }, _>(io),
            &mut self.good[0],
        );
        let rate = value(
            &self.rate,
            self.declared_param::<{ UnitParam::Pitch as u16 }, _>(io),
            &mut self.good[1],
        );
        let length = value(
            &self.length,
            self.declared_param::<{ UnitParam::GrainLength as u16 }, _>(io),
            &mut self.good[2],
        );
        let position_jitter = value(
            &self.jitter[0],
            self.declared_param_at::<{ UnitParam::Jitter as u16 }, _>(io, 0),
            &mut self.good[3],
        );
        let timing_jitter = value(
            &self.jitter[1],
            self.declared_param_at::<{ UnitParam::Jitter as u16 }, _>(io, 1),
            &mut self.good[4],
        );
        let position = value(
            &self.position,
            self.declared_param::<{ UnitParam::Position as u16 }, _>(io),
            &mut self.good[5],
        );
        GrainSettings::new(density, length)
            .position(position)
            .rate(rate)
            .position_jitter(position_jitter)
            .timing_jitter(timing_jitter)
    }
}

impl<F: Float> Node<F> for GranularNode<F> {
    /// No inputs, the layout's channels out, no latency, no ring-out.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.layout)
            .tail(Tail::None)
            .param_addrs(self.declared_ports().as_slice())
    }

    fn prepare(&mut self, prepare: &Prepare) {
        self.cloud = Some(Granular::new(
            Arc::clone(&self.source),
            prepare.sample_rate(),
            self.max_grains,
            self.seed,
        ));
    }

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        if self.cloud.is_none() {
            return Status::Silent;
        }
        let frames = io.frames();
        let settings = self.settings(&io);
        let Some(cloud) = self.cloud.as_mut() else {
            return Status::Silent;
        };
        let (_, mut outputs) = io.split();
        let Ok(mut planes) = outputs.planes_mut() else {
            return Status::Silent;
        };
        cloud.process(
            &settings,
            planes.slice_mut(Samples::ZERO..Samples::new(frames)),
        );
        Status::Modified
    }

    fn reset(&mut self) {
        if let Some(cloud) = &mut self.cloud {
            cloud.reset();
        }
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl<F: Float> ParamNode<F> for GranularNode<F> {
    /// The density, the pitch, the grain length and the two jitters.
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A clone with its cells detached (at their values now), its cloud at the start.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.density.detach();
        fork.rate.detach();
        fork.length.detach();
        fork.jitter.iter_mut().for_each(Param::detach);
        fork.position.detach();
        Node::<F>::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values last set through
/// it ([`tutti_graph::param_parts`]).
impl<F: Float> IntoNode<F> for GranularNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests;
