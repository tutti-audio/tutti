//! The granular source: [`GranularNode`], a cloud of grains read from a shared source,
//! made from a [`GranularConfig`].
//!
//! The node holds the params and the rate; the algorithm is `tutti_dsp::Granular`.

use std::sync::Arc;

use crate::last_good::LastGood;
use tutti_buffers::{ChannelLayout, Float, Wave};
use tutti_dsp::{GrainSettings, Granular, MaxGrains};
use tutti_graph::{Cx, Io, Node, ParamInput, Prepare, Shape, Shared, Status};
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
/// use tutti_graph::{GraphBuilder, Prepare};
/// use tutti_nodes::{GranularConfig, GranularNode, GranularSource};
/// use tutti_units::{Hz, SampleRate, Samples, Seconds};
///
/// let mut source = Wave::<f32>::new(1, SampleRate::SR_48K);
/// source.append(Interleaved::new(&[0.5; 4800], ChannelLayout::MONO)?)?;
/// let cloud = GranularNode::<f32>::from(
///     GranularConfig::new(GranularSource::new(ChannelLayout::MONO, Arc::new(source))?)
///         .density(Hz::new(100.0))
///         .length(Seconds::new(0.01)),
/// );
/// let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
/// let node = g.add_with_controls(cloud);
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
    cells: GranularCells<F>,
    /// The last finite value of each cell or port, in the order of [`Self::settings`]'s reads.
    good: [LastGood<f64>; 6],
    /// `None` until [`Node::prepare`]: no cloud is built at a guessed rate.
    cloud: Option<Granular<F>>,
}

// The params a [`GranularNode`] lets the graph modulate, in port order, and the
// [`GranularConfig`] it is made from: the source and the starting values of its
// params. The jitter is two cells: the position's at 0 and the timing's at 1.
tutti_graph::declare_params!([F: Float] GranularNode<F>, GranularConfig<F: Float>,
    GranularCells<F: Float> {
    required { source: GranularSource<F> }
    params {
        Rate => density: Hz = Hz::new(10.0),
        Pitch => rate: Cents = Cents::new(0.0),
        GrainLength => length: Seconds = Seconds::new(0.1),
        Jitter => jitter[2]: Normalized = Normalized::of(0.0),
        Position => position: Normalized = Normalized::of(0.0),
    }
    options { max_grains: MaxGrains = MaxGrains::of(32), seed: u64 = 0 }
});

/// What a [`GranularSource`] cannot be made of.
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

/// The wave a [`GranularNode`] reads and the layout it feeds: a mono wave for any layout,
/// otherwise a wave with a channel for each of the layout's.
///
/// The wave is a [`Shared`]: two sources are equal when they share one wave and one layout.
#[derive(Clone, Debug, PartialEq)]
pub struct GranularSource<F: Float> {
    layout: ChannelLayout,
    wave: Shared<Wave<F>>,
}

impl<F: Float> GranularSource<F> {
    /// `wave` read into the channels of `layout`.
    ///
    /// # Errors
    ///
    /// [`GranularError::Width`] when `wave` is neither mono nor as wide as `layout`: the
    /// validity is the wave's width and the layout's together.
    pub fn new(
        layout: impl Into<ChannelLayout>,
        wave: impl Into<Shared<Wave<F>>>,
    ) -> Result<Self, GranularError> {
        let (layout, wave) = (layout.into(), wave.into());
        let source_channels = wave.channels();
        if source_channels != 1 && source_channels != usize::from(layout.count()) {
            return Err(GranularError::Width {
                source_channels,
                output_channels: layout.count(),
            });
        }
        Ok(Self { layout, wave })
    }
}

impl<F: Float> From<GranularConfig<F>> for GranularNode<F> {
    fn from(config: GranularConfig<F>) -> Self {
        let good = |initial: f64, fallback: f64| LastGood::new(initial, fallback);
        Self {
            layout: config.source.layout,
            source: Arc::clone(config.source.wave.arc()),
            max_grains: config.max_grains,
            seed: config.seed,
            good: [
                good(config.density.get(), 10.0),
                good(config.rate.get(), 0.0),
                good(config.length.get(), 0.1),
                good(config.jitter[0].get(), 0.0),
                good(config.jitter[1].get(), 0.0),
                good(config.position.get(), 0.0),
            ],
            cells: GranularCells::from_config(&config),
            cloud: None,
        }
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
            &self.cells.density,
            self.declared_param::<{ UnitParam::Rate as u16 }, _>(io),
            &mut self.good[0],
        );
        let rate = value(
            &self.cells.rate,
            self.declared_param::<{ UnitParam::Pitch as u16 }, _>(io),
            &mut self.good[1],
        );
        let length = value(
            &self.cells.length,
            self.declared_param::<{ UnitParam::GrainLength as u16 }, _>(io),
            &mut self.good[2],
        );
        let position_jitter = value(
            &self.cells.jitter[0],
            self.declared_param_at::<{ UnitParam::Jitter as u16 }, _>(io, 0),
            &mut self.good[3],
        );
        let timing_jitter = value(
            &self.cells.jitter[1],
            self.declared_param_at::<{ UnitParam::Jitter as u16 }, _>(io, 1),
            &mut self.good[4],
        );
        let position = value(
            &self.cells.position,
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
            .param_ports(self.declared_ports())
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

#[cfg(test)]
mod tests;
