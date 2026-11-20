use super::error::{Result, VbapError};
use tutti_buffers::{Bed, BusTopology, ChannelLayout, ChannelTopology, Float, Fold, Mono, MonoMut};
use tutti_dsp::{COEFF_INTERVAL, Lerp, Ramp, SpeakerLayout, VbapGains, VbapKernel};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_rt::Param;
use tutti_units::UnitParam;
use tutti_units::{Azimuth, Elevation, SampleRate, Seconds, Spread, StereoWidth, Tail};

use crate::buses::main_bus;
use crate::ramp::LastGood;

/// The grid the gains are solved on, in frames: the controls are read, and the
/// gains solved, every `GRID` frames counted from the last reset, and a frame
/// reads the gains ramped between the two grid points around it.
///
/// It is [`COEFF_INTERVAL`], the grid the filters and the modulated delays solve
/// on (0.33 ms at 48 kHz): short enough that a gain vector turning through a
/// whole bed in a second moves by under a degree between two points, so the
/// chord between them sits within a hair of unit power.
const GRID: usize = COEFF_INTERVAL;

/// The time a bearing or height written to a cell takes to ramp to its new
/// value.
const RAMP: Seconds = Seconds::new(0.05);

// The params a [`VbapPannerNode`] lets the graph modulate, in port order.
tutti_graph::declare_params!([F: Float] VbapPannerNode<F>, n {
    Azimuth => Azimuth = &n.azimuth,
    Elevation => Elevation = &n.elevation,
    Spread => Spread = &n.spread,
    StereoWidth => StereoWidth = &n.width,
});

/// The last finite value each control cell held.
#[derive(Clone, Copy, Debug)]
struct Held {
    azimuth: LastGood<f64>,
    elevation: LastGood<f64>,
    spread: LastGood<f64>,
    width: LastGood<f64>,
}

/// The grid points a ramp of [`RAMP`] takes at `rate`: its length in frames,
/// rounded to whole grid periods (at least one).
fn ramp_steps(rate: SampleRate) -> u32 {
    let frames = RAMP.to_samples(rate);
    let points = ((frames.get() + GRID / 2) / GRID).max(1);
    u32::try_from(points).unwrap_or(u32::MAX)
}

/// A VBAP loudspeaker panner: stereo in, one output per channel of a [`Bed`].
///
/// Build it from a [`VbapPannerConfig`] over a bed, whose speakers sit where
/// `tutti_buffers::Bed::direction` puts them. It is a
/// graph node (`tutti_graph::Node` and [`ParamNode`]); inserting it hands back its
/// [`ParamSet`] ([`UnitParam::Azimuth`], [`Elevation`](tutti_units::UnitParam::Elevation),
/// [`Spread`](tutti_units::UnitParam::Spread) and [`StereoWidth`](tutti_units::UnitParam::StereoWidth)),
/// which move the source lock-free while it renders, and a graph's control
/// lanes may drive all four. A mono source presents the same sample on both
/// inputs. Rendering allocates nothing and reports no latency.
///
/// # Examples
///
/// ```
/// # use tutti_buffers::Planes;
/// use tutti_buffers::{Bed, MaxBlock};
/// use tutti_units::{SampleRate, Samples, UnitParam};
/// use tutti_graph::{ParamNode, Prepare, Standalone};
/// use tutti_nodes::{VbapPannerConfig, VbapPannerNode};
///
/// let panner = VbapPannerNode::<f32>::try_from(VbapPannerConfig::new(Bed::Quad)).unwrap();
/// panner.param_set().set(UnitParam::Azimuth, 45.0); // front-left
///
/// let mut standalone = Standalone::new(panner, Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64))));
/// let out = standalone.render_input(Planes::new(&[&[1.0; 64], &[1.0; 64]]).unwrap());
/// assert_eq!(out.len(), 4); // one channel per speaker
/// ```
///
/// # The beds
///
/// The speaker positions are the bed's: [`Bed::direction`] answers each from the
/// one BS.2051 table in `tutti_buffers`, and a channel's speaker is its place
/// among the bed's placed speakers. VBAP pans across the two or three speakers
/// surrounding a direction, so it needs an arrangement and not merely a channel
/// count; a topology that is not a bed ([`Bed::from_topology`]) is not a panner
/// input. [`Bed::Mono`] has one speaker, so no direction to pan between, and is
/// refused.
///
/// # The ramp
///
/// The controls are read, and the gains solved, on a grid of 16 frames counted
/// from the last reset. A frame reads the gains of the two grid points around it
/// blended linearly (`tutti_dsp::Ramp`, in closed form), so the render does not
/// depend on where the blocks fall: a control that moves between two grid points
/// is read at the next one, and sounds from the frame after it.
///
/// A bearing or height written to a cell is a step: the node ramps to it over
/// 50 ms (a whole number of grid points, [`tutti_dsp::Ramp`] per coordinate),
/// and the gains are solved for the direction it has reached at each grid point.
/// The ramp of the bearing takes the short arc: from 170 to -170 degrees it
/// passes through 180, never through the front.
///
/// A param that a control signal drives is followed, not smoothed: a control signal already has a
/// value for every frame, so the direction at each grid point is the control signal's value at that
/// frame (a bearing wrapped), and the gains chord along the short arc to the next
/// point. A control signal is read every 16 frames, so a source it moves lags it by at most
/// one grid period. A spread or width reaches the gains at the next grid point
/// and is blended in with them. The first grid point after construction, a
/// [`reset`](Node::reset) or a [`prepare`](Node::prepare) lands on the controls
/// whole.
///
/// # The energy law
///
/// The speaker gains sum to unit energy for every bearing, height, spread and
/// supported layout, including the directions no speaker tuple surrounds;
/// [`SpeakerLayout::gains_into`](tutti_dsp::SpeakerLayout::gains_into) says how.
///
/// That law holds at the grid points, not strictly between them: the gains are
/// blended linearly, a chord between two unit-energy vectors and not the arc
/// between them, so a source that moves sits slightly below unit energy between
/// grid points. The energy of a chord midpoint is `cos²(Δ/2)` for a gain vector
/// turning through `Δ` in one grid step, a dip of at most `sin²(Δ/2)`. A cell step
/// of half a turn ramps at 1.2 degrees of direction per grid step at 48 kHz; a
/// control signal moves the direction as fast as it likes, and the dip follows its speed. A held source is exact everywhere.
///
/// Note that LFE is not one of the gains: the panner never feeds it
/// (a low-passed send, wired by the caller, feeds it), so it reads zero and is outside the law above.
///
/// # Clone
///
/// A clone shares the node's control cells (so a clone taken as a fork's
/// template reads the placement as it is when the fork is taken) and copies
/// its ramps. A fork detaches the cells and resets the copy.
#[derive(Clone, Debug)]
pub struct VbapPannerNode<F: Float> {
    kernel: VbapKernel,
    layout: ChannelLayout,
    /// Bearing, wrapped onto the circle when read.
    azimuth: Param<Azimuth>,
    /// Height, clamped to the poles when read.
    elevation: Param<Elevation>,
    /// VBAP diffusion, `0..1`: how many speakers a point source is smeared
    /// across. See [`Spread`]: it is not a `Mix`, because it blends nothing.
    spread: Param<Spread>,
    /// Mid/side stereo width, `0..`: 1.0 is unchanged, above 1.0 is wider
    /// than the source. NOT an `Amplitude` despite the matching range: it
    /// scales the SIDE component against the mid. See [`StereoWidth`].
    width: Param<StereoWidth>,
    /// Output-channel → speaker gather map, derived from the bed's topology
    /// (see `speaker_of_channel`). `None` marks a channel no speaker feeds:
    /// the LFE, which the panner leaves silent. Precomputed per layout so the
    /// RT path just indexes it, so the render can run channel-outer over each
    /// output's planar slice.
    speaker_of_channel: Vec<Option<usize>>,
    /// The stereo input folded to mono, for the mono-fold branch. Built at
    /// construction.
    fold: Fold<F>,
    /// The last finite azimuth, elevation, spread and width the cells held, so
    /// a non-finite write reads as unchanged instead of reaching the solve.
    good: Held,
    /// The bearing the gains are solved for, ramping to a cell's value over
    /// [`ramp_steps`] grid points (one step per grid point) and parked on a
    /// control signal's.
    aim_azimuth: Ramp<Azimuth>,
    /// The height, as `aim_azimuth`.
    aim_elevation: Ramp<Elevation>,
    /// The gains between the grid point behind and the one ahead: its steps
    /// are the frames of a grid segment.
    gains: Ramp<VbapGains>,
    /// The grid points a bearing or height ramp takes (see [`ramp_steps`]).
    steps: u32,
    /// Frames into the grid segment, in `0..GRID`: zero is a grid point.
    pos: usize,
    /// Whether a grid point has landed on the controls since construction, a
    /// reset or a prepare; the first does, whole.
    seated: bool,
}

/// What a [`VbapPannerNode`] is made from: the bus it pans over.
///
/// The bus names a [`Bed`] ([`Bed::from_topology`]), whose speakers sit where
/// [`Bed::direction`] puts them; [`TryFrom`] makes the node.
///
/// ```
/// use tutti_buffers::Bed;
/// use tutti_nodes::{VbapPannerConfig, VbapPannerNode};
///
/// let panner = VbapPannerNode::<f32>::try_from(VbapPannerConfig::new(Bed::Quad)).unwrap();
/// assert_eq!(panner.channels().count(), 4);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VbapPannerConfig {
    output: BusTopology,
}

impl VbapPannerConfig {
    /// A panner over the speakers of `output`: stereo in, the bus's channels out.
    #[must_use]
    pub fn new(output: impl Into<BusTopology>) -> Self {
        Self {
            output: output.into(),
        }
    }
}

impl<F: Float> TryFrom<VbapPannerConfig> for VbapPannerNode<F> {
    type Error = VbapError;

    /// A panner over the config's bus: stereo in, the bus's channels out, in the bus's
    /// order, each placed speaker at its [`Bed::direction`] and the LFE, if the bus has
    /// one, silent.
    ///
    /// The refusal is of the one bus: a placement VBAP can pan over is a [`Bed`] with at
    /// least two speakers, which `BusTopology` cannot say.
    ///
    /// # Errors
    ///
    /// [`VbapError::NotABed`] for a bus that places no bed, and
    /// [`VbapError::Geometry`] for a bed whose speakers cannot be panned over: [`Bed::Mono`].
    fn try_from(config: VbapPannerConfig) -> Result<Self> {
        let bed =
            Bed::from_topology(&ChannelTopology::from(config.output)).ok_or(VbapError::NotABed)?;
        let layout = SpeakerLayout::try_from(bed)?;
        let steps = ramp_steps(SampleRate::SR_48K);
        let mut kernel = VbapKernel::new(layout);
        let gains = kernel.gains(
            Azimuth::FRONT,
            Elevation::LEVEL,
            StereoWidth::NATURAL,
            Spread::POINT,
        );
        Ok(Self {
            kernel,
            layout: bed.width(),
            azimuth: Param::new(Azimuth::FRONT),
            elevation: Param::new(Elevation::LEVEL),
            spread: Param::new(Spread::POINT),
            width: Param::new(StereoWidth::NATURAL),
            speaker_of_channel: speaker_of_channel(bed),
            fold: stereo_to_mono(),
            good: Held {
                azimuth: LastGood::fixed(Azimuth::FRONT.get()),
                elevation: LastGood::fixed(Elevation::LEVEL.get()),
                spread: LastGood::fixed(Spread::POINT.get()),
                width: LastGood::fixed(StereoWidth::NATURAL.get()),
            },
            aim_azimuth: Ramp::new(Azimuth::FRONT).steps(steps),
            aim_elevation: Ramp::new(Elevation::LEVEL).steps(steps),
            gains: Ramp::new(gains).steps(GRID as u32),
            steps,
            pos: 0,
            seated: false,
        })
    }
}

impl<F: Float> VbapPannerNode<F> {
    /// The output channels: the bed's width.
    pub fn channels(&self) -> ChannelLayout {
        self.layout
    }

    /// One grid point, at frame `at` of the block: reads the controls (a
    /// control signal's value at that frame where a control signal drives the param, else the
    /// block's `cells`), steps the bearing and height one grid step and aims the
    /// gains ramp at the gains solved for where they have reached.
    ///
    /// A cell's value is a step, which the bearing and height ramp to over
    /// [`ramp_steps`] grid points. A driven control signal needs no smoothing, so a driven
    /// coordinate is the control signal's value at the grid point, wrapped for a bearing,
    /// and the gains chord to the next grid point from there.
    fn grid_point(
        &mut self,
        at: usize,
        driven: &[Option<&[F]>; 4],
        cells: (Azimuth, Elevation, Spread, StereoWidth),
    ) {
        // A control signal shorter than the block reads as the cell. A control signal is the graph's `F`; the
        // angles are `f64`, so a driven coordinate widens here, once per grid point, and
        // `Spread` and `StereoWidth` hold an `f32`.
        let signal = |k: usize| driven[k].and_then(|signal| signal.get(at)).copied();
        let azimuth = self
            .good
            .azimuth
            .read(signal(0).map_or(cells.0.get(), Into::into));
        let elevation = self
            .good
            .elevation
            .read(signal(1).map_or(cells.1.get(), Into::into));
        let spread = Spread::new_clamped(
            self.good
                .spread
                .read(signal(2).map_or(cells.2.get(), Into::into)),
        );
        let width = StereoWidth::new(
            self.good
                .width
                .read(signal(3).map_or(cells.3.get(), Into::into)),
        );
        let whole = !self.seated;
        steer(
            &mut self.aim_azimuth,
            Azimuth::new(azimuth).wrap(),
            whole || signal(0).is_some(),
            self.steps,
        );
        steer(
            &mut self.aim_elevation,
            Elevation::new_clamped(elevation),
            whole || signal(1).is_some(),
            self.steps,
        );
        let (azimuth, elevation) = (self.aim_azimuth.now(), self.aim_elevation.now());
        let gains = self.kernel.gains(azimuth, elevation, width, spread);
        if whole {
            self.gains = Ramp::new(gains).steps(GRID as u32);
            self.seated = true;
        }
        self.gains.jump_to_target();
        self.gains.retarget(gains);
    }
}

/// One grid step of a coordinate's ramp toward `target`: parked on it when
/// `whole`, else aimed at it when it moved, then stepped.
fn steer<T: Lerp>(ramp: &mut Ramp<T>, target: T, whole: bool, steps: u32) {
    if whole {
        *ramp = Ramp::new(target).steps(steps);
    } else if target != ramp.target() {
        ramp.retarget(target);
    }
    ramp.advance(1);
}

/// The fold of the stereo input to the mono the single-source branch pans.
fn stereo_to_mono<F: Float>() -> Fold<F> {
    Fold::between(
        &ChannelTopology::assumed(ChannelLayout::STEREO),
        &ChannelTopology::assumed(ChannelLayout::MONO),
    )
    .expect("stereo folds to mono")
}

/// Which speaker feeds each channel of `bed`: its place among the bed's placed
/// speakers, in the bed's order, and `None` for a channel with no direction (the
/// LFE). Allocates, so it runs at construction only.
fn speaker_of_channel(bed: Bed) -> Vec<Option<usize>> {
    let mut next = 0;
    // A bed's topology is speakers.
    bed.topology()
        .positions()
        .unwrap_or_default()
        .iter()
        .map(|&speaker| {
            bed.direction(speaker).map(|_| {
                next += 1;
                next - 1
            })
        })
        .collect()
}

/// A graph node: stereo in, the bed's width out, no latency.
///
/// # Reset clears time, not placement
///
/// [`reset`](Node::reset) restarts the grid and drops the ramps in flight.
/// Position, spread and width are caller-set configuration and survive: a host
/// resets between clips to clear a tail, and a reset that re-aimed would
/// silently move every spatialised source to front-centre and, since clones
/// share these cells, the *live* node's source too.
///
/// The first grid point after a reset lands on the controls as they are then, so
/// the first block after a reset already renders there rather than ramping in
/// from front-centre, and a reset node renders what a fresh one does.
///
/// # Tail
///
/// [`Tail::Unknown`], deliberately not [`Tail::None`]: the executor skips a
/// `Tail::None` node on a silent block, and a skipped block would not step the
/// grid, so a source moved during silence would resume from a stalled ramp
/// instead of the settled bearing.
impl<F: Float> Node<F> for VbapPannerNode<F> {
    fn shape(&self) -> Shape {
        // A stereo pair in and the bed's speakers out, so the graph's mix and
        // bypass fold the dry pair onto the bed by speaker.
        Shape::buses(main_bus(ChannelLayout::STEREO), main_bus(self.layout))
            .with_ports(ChannelLayout::STEREO, self.layout)
            .with_tail(Tail::Unknown)
            .with_param_addrs(self.declared_ports().as_slice())
            // A blended gain pair per output channel and frame, and two
            // solves per grid segment while the source moves.
            .with_cost(tutti_graph::Cost::Moderate)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.steps = ramp_steps(p.sample_rate());
        self.aim_azimuth = self.aim_azimuth.steps(self.steps);
        self.aim_elevation = self.aim_elevation.steps(self.steps);
        // The next grid point lands whole at the new rate.
        self.seated = false;
        self.pos = 0;
    }

    /// Renders the block along the grid.
    ///
    /// At each grid point (every 16 frames from the last reset, wherever
    /// the blocks fall) the controls are read and the gains solved; the frames
    /// until the next point read the ramp between them. A control signal that drives a
    /// param is read at the grid point's frame, a cell once for the block.
    /// Each output channel is rendered over its own planar slice, reading the
    /// speaker that feeds it (LFE: none, so silence). The panner never feeds
    /// LFE; the caller feeds it a separate low-passed send.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let size = io.frames();
        if size == 0 {
            return Status::Modified;
        }
        let driven = [
            self.declared_param::<{ UnitParam::Azimuth as u16 }, _>(&io)
                .frames(),
            self.declared_param::<{ UnitParam::Elevation as u16 }, _>(&io)
                .frames(),
            self.declared_param::<{ UnitParam::Spread as u16 }, _>(&io)
                .frames(),
            self.declared_param::<{ UnitParam::StereoWidth as u16 }, _>(&io)
                .frames(),
        ];
        let cells = (
            self.azimuth.load(),
            self.elevation.load(),
            self.spread.load(),
            self.width.load(),
        );
        let (inputs, mut outputs) = io.split();
        let (left, right) = (&inputs.get(0)[..size], &inputs.get(1)[..size]);
        let mut at = 0;
        while at < size {
            if self.pos == 0 {
                self.grid_point(at, &driven, cells);
            }
            let end = at + (GRID - self.pos).min(size - at);
            // The first frame after a grid point reads step 1.
            let first = self.pos as u32 + 1;
            for (ch, out) in outputs.iter_mut().enumerate() {
                VbapGains::process_channel(
                    &self.gains,
                    first,
                    self.speaker_of_channel[ch],
                    &self.fold,
                    Mono::new(&left[at..end]),
                    Mono::new(&right[at..end]),
                    MonoMut::new(&mut out[at..end]),
                );
            }
            self.pos = (self.pos + end - at) % GRID;
            at = end;
        }
        Status::Modified
    }

    fn reset(&mut self) {
        // The next grid point lands on the cells as they are then.
        self.seated = false;
        self.pos = 0;
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl<F: Float> ParamNode<F> for VbapPannerNode<F> {
    /// The bearing, height, spread and width.
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A clone with its four cells detached (at their values now) and its
    /// grid restarted.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.azimuth.detach();
        fork.elevation.detach();
        fork.spread.detach();
        fork.width.detach();
        Node::<F>::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl<F: Float> IntoNode<F> for VbapPannerNode<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests;
