//! [`MultibandCompressorNode`]: a compressor per band of a Linkwitz-Riley
//! crossover, summed.
//!
//! The algorithm is `tutti-dsp`'s ([`MultibandBank`]: the crossover, a
//! [`CompressorBank`](tutti_dsp::CompressorBank) per band and the band sum); this file holds
//! the cells and how a block reads them. It is **hand-written** and not a
//! [`KernelNode`](crate::KernelNode): its band count varies per instance, its bypass and
//! solo cells have no port, and a `KernelNode`'s [`ControlSet`](crate::ControlSet) takes
//! `&'static` addresses.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use atomic_float::AtomicF64;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_buffers::{Float, InputOutput, Mono, Planes, PlanesMut};
use tutti_dsp::{
    BandSettings, BankShape, COEFF_INTERVAL, CompressorControls, CrossoverControls, Lerp,
    MultibandBands, MultibandBank, MultibandSettings, MultibandSignals, SimdLanes,
};
use tutti_graph::{
    Cost, Cx, HostValues, Initials, IntoNode, Io, MAX_PARAM_PORTS, Node, NodeConfig, NodeParts,
    ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_rt::Param;
use tutti_units::{CompressionRatio, Db, Hz, SampleRate, Samples};
use tutti_units::{Seconds, Tail, UnitAddr, UnitParam};

use crate::filter::Splits;
use crate::kernel::{
    Grid, first_non_finite, log_non_finite, silence_block, slice_in, slice_out, walk_grid,
};
use crate::last_good::LastGood;

/// The crossover frequencies a band count starts with.
fn default_crossovers(bands: MultibandBands) -> &'static [Hz] {
    match bands.get() {
        2 => &const { [Hz::new(1_000.0)] },
        3 => &const { [Hz::new(200.0), Hz::new(3_000.0)] },
        _ => &const { [Hz::new(120.0), Hz::new(1_000.0), Hz::new(6_000.0)] },
    }
}

/// The threshold a band starts at.
const DEFAULT_THRESHOLD: Db = Db::new(-18.0);
/// The ratio a band starts at.
const DEFAULT_RATIO: CompressionRatio = CompressionRatio::new(2.0);
/// The attack a band starts at.
const DEFAULT_ATTACK: Seconds = Seconds::new(0.010);
/// The release a band starts at.
const DEFAULT_RELEASE: Seconds = Seconds::new(0.100);

/// The most crossovers: one fewer than the bands.
const MAX_CROSSOVERS: usize = BankShape::MAX_BANDS - 1;

/// The params of one band, in port order: the band is the address's index.
/// The make-up gain is [`UnitParam::GainDb`], as [`CompressorNode`]'s is
/// (one cell, one address).
///
/// [`CompressorNode`]: crate::CompressorNode
pub const BAND_PARAMS: [UnitParam; 6] = [
    UnitParam::Threshold,
    UnitParam::Knee,
    UnitParam::Ratio,
    UnitParam::Attack,
    UnitParam::Release,
    UnitParam::GainDb,
];

/// Param ports per band.
const PER_BAND: usize = BAND_PARAMS.len();
/// Param ports at the most bands: the band params, then the crossovers.
const MAX_PORTS: usize = BankShape::MAX_BANDS * PER_BAND + MAX_CROSSOVERS;
const _: () = assert!(MAX_PORTS <= MAX_PARAM_PORTS);

/// Where each of a band's params is in [`BAND_PARAMS`].
const THRESHOLD: usize = 0;
const KNEE: usize = 1;
const RATIO: usize = 2;
const ATTACK: usize = 3;
const RELEASE: usize = 4;
const MAKEUP: usize = 5;

/// A toggle cell's value: `>= 0.5` is on (so a NaN is off), as
/// [`UnitParam::Bypass`] encodes it.
#[inline]
fn is_on(cell: &AtomicF64) -> bool {
    cell.load(core::sync::atomic::Ordering::Acquire) >= 0.5
}

/// The handles on a [`MultibandCompressorNode`] once inserted: its
/// [`ParamSet`] and the band meters. Clones share the cells.
///
/// The settings are the set's; the meters are readings, not controls, so
/// they are not in it.
#[derive(Clone, Debug)]
pub struct MultibandControls {
    params: ParamSet,
    reductions: Vec<Param<Db>>,
}

impl MultibandControls {
    /// The node's params by address (see [`MultibandCompressorNode`]).
    #[must_use]
    pub fn params(&self) -> &ParamSet {
        &self.params
    }

    /// How many bands there are.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.reductions.len()
    }

    /// Band `band`'s largest gain reduction in the last block, in [`Db`]: a
    /// **measurement**, for a reduction meter. Larger is more reduction;
    /// [`Db::UNITY`] is none (also past the last band, and while the band is
    /// bypassed for every frame of each 64-frame chunk of the block).
    ///
    /// The node writes it once per block, so reading it from any thread is
    /// wait-free.
    #[must_use]
    pub fn gain_reduction_db(&self, band: usize) -> Db {
        self.reductions.get(band).map_or(Db::UNITY, Param::load)
    }
}

/// One band's cells.
#[derive(Clone, Debug)]
struct Band {
    threshold: Param<Db>,
    knee: Param<Db>,
    ratio: Param<CompressionRatio>,
    attack: Param<Seconds>,
    release: Param<Seconds>,
    makeup: Param<Db>,
    /// `>= 0.5` bypasses the band ([`UnitParam::Bypass`]).
    bypass: Arc<AtomicF64>,
    /// `>= 0.5` solos the band ([`UnitParam::Solo`]).
    solo: Arc<AtomicF64>,
    reduction: Param<Db>,
    /// Last finite threshold, knee, ratio, makeup, attack and release. Every one of
    /// them reaches the envelope follower, a recursive state that a single NaN or ±∞
    /// leaves NaN for good, so a non-finite write reads as unchanged (see [`LastGood`]).
    good: [LastGood<f64>; PER_BAND],
}

impl Band {
    fn new() -> Self {
        Self {
            threshold: Param::new(DEFAULT_THRESHOLD),
            knee: Param::new(Db::new(0.0)),
            ratio: Param::new(DEFAULT_RATIO),
            attack: Param::new(DEFAULT_ATTACK),
            release: Param::new(DEFAULT_RELEASE),
            makeup: Param::new(Db::new(0.0)),
            bypass: Arc::new(AtomicF64::new(0.0)),
            solo: Arc::new(AtomicF64::new(0.0)),
            reduction: Param::new(Db::UNITY),
            good: [
                LastGood::new(DEFAULT_THRESHOLD.get(), 0.0),
                LastGood::fixed(0.0),
                LastGood::new(DEFAULT_RATIO.get(), CompressionRatio::UNITY.get()),
                LastGood::new(DEFAULT_ATTACK.get(), 0.0),
                LastGood::new(DEFAULT_RELEASE.get(), 0.0),
                LastGood::fixed(0.0),
            ],
        }
    }

    /// Stops sharing every cell, keeping the values (and the meter, which starts again).
    fn detach(&mut self) {
        self.threshold.detach();
        self.knee.detach();
        self.ratio.detach();
        self.attack.detach();
        self.release.detach();
        self.makeup.detach();
        self.bypass = Arc::new(AtomicF64::new(
            self.bypass.load(core::sync::atomic::Ordering::Acquire),
        ));
        self.solo = Arc::new(AtomicF64::new(
            self.solo.load(core::sync::atomic::Ordering::Acquire),
        ));
        self.reduction = Param::new(Db::UNITY);
    }

    /// The band's settings at frame `at` of the block: the cells, or the values the
    /// graph drives on the band's ports (the threshold is read per frame by the
    /// bank, so its cell is the base it falls back to). A non-finite value reads as
    /// the last finite one.
    fn read<F: Float>(&mut self, signals: &[Option<&[F]>; PER_BAND], at: usize) -> BandSettings {
        let driven = |k: usize| signals[k].map(|signal| signal[at].into());
        let mut take = |k: usize, cell: f64| self.good[k].read(driven(k).unwrap_or(cell));
        let knee = take(KNEE, self.knee.load().get());
        let ratio = take(RATIO, self.ratio.load().get());
        let attack = take(ATTACK, self.attack.load().get());
        let release = take(RELEASE, self.release.load().get());
        let makeup = take(MAKEUP, self.makeup.load().get());
        // The threshold's control signal is the bank's own; only the base is read here.
        let threshold = self.good[THRESHOLD].read(self.threshold.load().get());
        BandSettings::new()
            .compressor(CompressorControls {
                threshold: Db::new(threshold),
                knee: Db::new(knee),
                ratio: CompressionRatio::new(ratio),
                attack: Seconds::new(attack),
                release: Seconds::new(release),
                makeup: Db::new(makeup),
                // The multiband compressor has no key filter: its bands read the split signal.
                ..CompressorControls::DEFAULT
            })
            .bypass(is_on(&self.bypass))
            .solo(is_on(&self.solo))
    }
}

/// What the graph drives on the node's param ports for a block, by band and param, and
/// by crossover: `None` for a port nothing drives.
struct PortSignals<'a, F: Float> {
    band: [[Option<&'a [F]>; PER_BAND]; BankShape::MAX_BANDS],
    cutoff: [Option<&'a [F]>; MAX_CROSSOVERS],
}

/// A multiband compressor: a Linkwitz-Riley crossover splits the input into 2 to 4 bands,
/// each band is compressed on its own, and the bands are summed.
///
/// `channels` in, `channels` out; the algorithm, its crossover and phase, its grid and its
/// ramps are [`MultibandBank`]'s, which this node runs. Zero latency and
/// [`Tail::Unknown`] (the crossover's sections ring after the input stops), so it never
/// parks.
///
/// # Params
///
/// Every setting is a param on the node's [`ParamSet`], addressed by a kind and an index
/// (the band, or the crossover):
///
/// | Address | Unit | Meaning |
/// |---|---|---|
/// | `Threshold`, band | [`Db`] | level above which the band is reduced |
/// | `Knee`, band | [`Db`] | knee width, 0 (hard) to 400 |
/// | `Ratio`, band | [`CompressionRatio`] | clamped to at least 1 |
/// | `Attack`, band | [`Seconds`] | floored at 0 |
/// | `Release`, band | [`Seconds`] | floored at 0 |
/// | `GainDb`, band | [`Db`] | make-up after reduction |
/// | `Cutoff`, crossover | [`Hz`] | between band `i` and band `i + 1` |
/// | `Bypass`, band | toggle | `>= 0.5` passes the band unreduced |
/// | `Solo`, band | toggle | `>= 0.5` solos the band |
///
/// That is `bands * 6 + (bands - 1)` param ports the graph can modulate (27 at 4 bands, under
/// `MAX_PARAM_PORTS`), in the order of [`BAND_PARAMS`] band by band and then the
/// crossovers. The threshold is read per frame when driven; every other control is read on
/// the stream grid, 16 frames from the last reset ([`COEFF_INTERVAL`]), so a cell write or a
/// driven port reaches the render at the next grid point whatever the blocks. A non-finite
/// write reads as unchanged (the last finite value stands). A crossover set that is not
/// strictly increasing, after each frequency is fitted under the rate, is refused as a
/// whole and the last good set stands.
///
/// The toggles are cells with no port, as [`BusStripNode`](crate::BusStripNode)'s mute is:
/// they are a block-rate on/off a control signal would only flap, and with them the ports would pass
/// the graph's 32.
///
/// A [`reset`](Node::reset) and a clone (a fork) start from silence, and the next retarget lands
/// whole, as a fresh node's first does. A block whose output holds a NaN or an infinity is
/// passed on as rendered and resets the node after it, its meters back at unity, so nothing
/// non-finite is latched.
///
/// The gain reduction of each band is a measurement:
/// [`gain_reduction_db`](Self::gain_reduction_db), or
/// [`MultibandControls::gain_reduction_db`] once inserted: the band's deepest reduction in
/// the last block. A band is left out of it only for a whole 64-frame chunk of the block on
/// which it is bypassed (see [`BandReductions`](tutti_dsp::BandReductions)).
///
/// `F` is the float of the graph it runs in, and of the crossover, the detectors and the
/// gains.
pub struct MultibandCompressorNode<F: Float + SimdLanes + Lerp> {
    rig: Rig,
    /// What [`prepare`](Node::prepare) builds from the rate it is given: `None` until the
    /// node is prepared.
    tuned: Option<Tuned<F>>,
}

/// What the node is without a rate: its cells.
struct Rig {
    shape: BankShape,
    bands: Vec<Band>,
    splits: Splits,
    /// The last set of crossover frequencies that was valid.
    crossover: CrossoverControls,
}

/// The node prepared for one rate and block size.
struct Tuned<F: Float + SimdLanes + Lerp> {
    bank: MultibandBank<F>,
    /// The settings the bank was last set to.
    applied: Option<MultibandSettings>,
    rate: SampleRate,
    /// Frames rendered since the last reset: where the stream grid's points fall.
    position: u64,
}

impl<F: Float + SimdLanes + Lerp> fmt::Debug for MultibandCompressorNode<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MultibandCompressorNode")
            .field("layout", &self.rig.shape.width())
            .field("bands", &self.rig.bands.len())
            .finish_non_exhaustive()
    }
}

/// What a [`MultibandCompressorNode`] is made from: its bus and band count.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultibandCompressorConfig {
    input: ChannelTopology,
    bands: MultibandBands,
}

impl MultibandCompressorConfig {
    /// A compressor of `bands` bands over the bus `input`, as wide as it. The node's ports
    /// name no speaker.
    ///
    /// The crossovers start at 1 kHz (2 bands), 200 Hz and 3 kHz (3), or
    /// 120 Hz, 1 kHz and 6 kHz (4). Each band starts at a -18 dB threshold,
    /// 2:1, 10 ms attack, 100 ms release, hard knee, 0 dB makeup, neither
    /// bypassed nor soloed. The node has no rate until the graph's `prepare` gives it
    /// one, and renders silence before.
    #[must_use]
    pub fn new(input: ChannelTopology, bands: MultibandBands) -> Self {
        Self { input, bands }
    }
}

impl<F: Float + SimdLanes + Lerp> From<MultibandCompressorConfig> for MultibandCompressorNode<F> {
    fn from(config: MultibandCompressorConfig) -> Self {
        let MultibandCompressorConfig { input, bands } = config;
        let defaults = default_crossovers(bands);
        let crossover = CrossoverControls::try_from(defaults)
            .expect("the default splits are positive and strictly increasing");
        Self {
            rig: Rig {
                shape: BankShape::new(input, bands),
                bands: (0..bands.get()).map(|_| Band::new()).collect(),
                splits: Splits::new(defaults),
                crossover,
            },
            tuned: None,
        }
    }
}

/// A multiband compressor's config holds its bus and band count, its structure; every band's
/// controls and the crossovers start at the defaults [`new`](MultibandCompressorConfig::new) lists.
impl<F: Float + SimdLanes + Lerp> NodeConfig<F> for MultibandCompressorConfig {
    type Node = MultibandCompressorNode<F>;

    fn build(&self, _: &HostValues) -> MultibandCompressorNode<F> {
        MultibandCompressorNode::from(*self)
    }

    fn same_structure(&self, other: &Self) -> bool {
        self == other
    }

    fn initials(&self) -> Initials {
        let band = |kind, value: f64| (kind, value);
        let per_band = [
            band(UnitParam::Threshold, DEFAULT_THRESHOLD.get()),
            band(UnitParam::Knee, 0.0),
            band(UnitParam::Ratio, DEFAULT_RATIO.get()),
            band(UnitParam::Attack, DEFAULT_ATTACK.get()),
            band(UnitParam::Release, DEFAULT_RELEASE.get()),
            band(UnitParam::GainDb, 0.0),
            band(UnitParam::Bypass, 0.0),
            band(UnitParam::Solo, 0.0),
        ];
        let mut initials = Initials::new();
        for b in 0..self.bands.get() {
            for (kind, value) in per_band {
                initials = initials.param(UnitAddr::new(kind, b), value);
            }
        }
        for (i, split) in (0u8..).zip(default_crossovers(self.bands)) {
            initials = initials.param(UnitAddr::new(UnitParam::Cutoff, i), split.get());
        }
        initials
    }
}

impl<F: Float + SimdLanes + Lerp> MultibandCompressorNode<F> {
    /// How many bands there are.
    #[must_use]
    pub fn bands(&self) -> usize {
        self.rig.bands.len()
    }

    /// The channel layout, the width of both input and output.
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.rig.shape.width()
    }

    /// Band `band`'s largest gain reduction in the last block, in [`Db`] (see
    /// [`MultibandControls::gain_reduction_db`]); [`Db::UNITY`] past the last
    /// band.
    #[must_use]
    pub fn gain_reduction_db(&self, band: usize) -> Db {
        self.rig
            .bands
            .get(band)
            .map_or(Db::UNITY, |b| b.reduction.load())
    }
}

impl Rig {
    /// The meters at unity.
    fn clear_meters(&mut self) {
        for b in &mut self.bands {
            b.reduction.store(Db::UNITY);
        }
    }

    /// The settings at frame `at` of the block, at `rate`.
    fn read<F: Float>(
        &mut self,
        rate: SampleRate,
        signals: &PortSignals<'_, F>,
        at: usize,
    ) -> MultibandSettings {
        let nb = self.bands.len();
        // `read` fits every entry to the rate, so only a set that is out of order or not
        // above 0 is refused, and the last good one stands.
        let targets = self
            .splits
            .read(rate, |i| signals.cutoff[i].map(|l| l[at].into()));
        if let Ok(set) = CrossoverControls::try_from((&targets[..nb - 1], rate)) {
            self.crossover = set;
        }
        let mut bands = [BandSettings::new(); BankShape::MAX_BANDS];
        for (b, band) in self.bands.iter_mut().enumerate() {
            bands[b] = band.read(&signals.band[b], at);
        }
        MultibandSettings::new()
            .crossover(self.crossover)
            .bands(bands)
    }
}

impl<F: Float + SimdLanes + Lerp> Tuned<F> {
    /// Prepares a bank for `rate`.
    fn new(shape: BankShape, rate: SampleRate) -> Self {
        Self {
            bank: MultibandBank::<F>::new(shape, rate),
            applied: None,
            rate,
            position: 0,
        }
    }

    /// Back to silence, the meters at unity, the next design landing whole.
    fn clear(&mut self, rig: &mut Rig) {
        self.bank.reset();
        self.applied = None;
        self.position = 0;
        rig.clear_meters();
    }

    /// Aims the bank at `settings`.
    fn apply(&mut self, settings: MultibandSettings) {
        self.bank.retarget(&settings);
        if self.applied.is_none() {
            self.bank.jump_to_target();
        }
        self.applied = Some(settings);
    }

    /// Renders `frames` of the block through the bank, each band's threshold control signal cut to
    /// them.
    fn render(
        &mut self,
        input: &[&[F]],
        output: &mut [&mut [F]],
        signals: &PortSignals<'_, F>,
        frames: Range<usize>,
    ) {
        if frames.is_empty() {
            return;
        }
        let (audio, heard) = slice_in(input.iter().copied(), &frames);
        let (mut out, written) = slice_out(output.iter_mut().map(|c| &mut **c), &frames);
        let (Ok(input), Ok(output)) = (
            Planes::new(&audio[..heard]),
            PlanesMut::new(&mut out[..written]),
        ) else {
            return;
        };
        let Ok(mut block) = InputOutput::new(Samples::new(frames.len()), input, output) else {
            return;
        };
        let mut thresholds = MultibandSignals::default();
        for (slot, signal) in thresholds.thresholds.iter_mut().zip(&signals.band) {
            *slot = signal[THRESHOLD].map(|l| Mono::new(&l[frames.clone()]));
        }
        // A block that is not the bank's shape is refused whole: silence, never a part.
        if self.bank.process(thresholds, &mut block).is_err() {
            silence_block(&mut block);
        }
    }

    /// Renders the block cut at the grid points of the stream, the block beginning at
    /// stream frame `position`: each grid point reads the controls at its frame, and one
    /// that moved retargets the bank before the frames after it render. The stretches that did
    /// not move render together, in one call.
    fn render_steps(
        &mut self,
        rig: &mut Rig,
        input: &[&[F]],
        output: &mut [&mut [F]],
        signals: &PortSignals<'_, F>,
        position: u64,
    ) {
        let rate = self.rate;
        walk_grid(
            &mut (self, rig, output),
            Grid {
                position,
                size: input[0].len(),
                period: COEFF_INTERVAL,
            },
            |_, _| None,
            |(tuned, rig, _), at, _| {
                let target = rig.read(rate, signals, at);
                (tuned.applied.as_ref() != Some(&target)).then_some(target)
            },
            |(tuned, ..), target| tuned.apply(target),
            |(tuned, _, output), frames| tuned.render(input, output, signals, frames),
        );
    }
}

// The params, band by band in the order of `BAND_PARAMS`, then a toggle pair
// per band that is no port, then one crossover per split. The render reads
// them by kind and band (`declared_param_at`), not by port. A band's make-up gain is addressed as
// `GainDb`.
tutti_graph::declare_params!([F: Float + SimdLanes + Lerp] MultibandCompressorNode<F>, n {
    [b in 0..n.rig.bands.len()] {
        Threshold => Db { whole } = &n.rig.bands[b].threshold,
        Knee => Db { whole } = &n.rig.bands[b].knee,
        Ratio => CompressionRatio { whole } = &n.rig.bands[b].ratio,
        Attack => Seconds { whole } = &n.rig.bands[b].attack,
        Release => Seconds { whole } = &n.rig.bands[b].release,
        GainDb => Db { whole } = &n.rig.bands[b].makeup;
        Bypass => bool = &n.rig.bands[b].bypass,
        Solo => bool = &n.rig.bands[b].solo,
    },
    [i in 0..n.rig.bands.len() - 1] {
        Cutoff => Hz = n.rig.splits.param(i),
    },
});

impl<F: Float + SimdLanes + Lerp> Node<F> for MultibandCompressorNode<F> {
    /// `channels` in, `channels` out; zero latency; a param port for each
    /// band parameter and each crossover (see the type's docs).
    ///
    /// Heavy (the default cost): a split into up to 4 bands and a compressor
    /// for each, per channel. The tail is [`Tail::Unknown`], as the other IIR
    /// filters': the crossover rings after the input stops.
    fn shape(&self) -> Shape {
        Shape::through_unassigned(self.rig.shape.width())
            .cost(Cost::Heavy)
            .tail(Tail::Unknown)
            .param_ports(self.declared_ports())
    }

    fn prepare(&mut self, p: &Prepare) {
        self.tuned = Some(Tuned::new(self.rig.shape, p.sample_rate()));
        self.rig.clear_meters();
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let n = io.frames();
        if n == 0 {
            return Status::Modified;
        }
        if self.tuned.is_none() {
            return Status::Silent;
        }

        // What the graph drives on each port, taken before the split of `io`.
        let nb = self.rig.bands.len();
        let mut signals = PortSignals {
            band: [[None; PER_BAND]; BankShape::MAX_BANDS],
            cutoff: [None; MAX_CROSSOVERS],
        };
        for (i, c) in signals.cutoff.iter_mut().enumerate().take(nb - 1) {
            *c = self
                .declared_param_at::<{ UnitParam::Cutoff as u16 }, _>(&io, i as u8)
                .narrowed();
        }
        for (b, signal) in signals.band[..nb].iter_mut().enumerate() {
            let band = b as u8;
            // In the order of `BAND_PARAMS`: `declared_param_at` takes its kind as a const
            // generic, which a loop over the array cannot supply.
            *signal = [
                self.declared_param_at::<{ UnitParam::Threshold as u16 }, _>(&io, band)
                    .narrowed(),
                self.declared_param_at::<{ UnitParam::Knee as u16 }, _>(&io, band)
                    .narrowed(),
                self.declared_param_at::<{ UnitParam::Ratio as u16 }, _>(&io, band)
                    .narrowed(),
                self.declared_param_at::<{ UnitParam::Attack as u16 }, _>(&io, band)
                    .narrowed(),
                self.declared_param_at::<{ UnitParam::Release as u16 }, _>(&io, band)
                    .narrowed(),
                self.declared_param_at::<{ UnitParam::GainDb as u16 }, _>(&io, band)
                    .narrowed(),
            ];
        }

        let (inputs, mut outputs) = io.split();
        let width = inputs.len();
        let (input, _) = slice_in((0..width).map(|c| inputs.get(c)), &(0..n));
        let (mut output, written) = slice_out(outputs.iter_mut(), &(0..n));
        let (input, output) = (&input[..width], &mut output[..written]);

        let Self { rig, tuned } = self;
        let Some(t) = tuned else {
            return Status::Silent;
        };
        let position = t.position;
        t.position += n as u64;
        t.render_steps(rig, input, output, &signals, position);

        // Each band's deepest reduction in the block.
        let reading = t.bank.take_reading();
        for (band, deepest) in rig.bands.iter().zip(reading.iter()) {
            band.reduction.store(deepest);
        }
        // A block that wrote a NaN or an infinity is passed on as rendered, and the bank
        // starts the next one from silence: nothing non-finite is latched.
        if let Some((frame, channel)) = first_non_finite(output.iter()) {
            log_non_finite(&io, frame, channel);
            t.clear(rig);
        }
        Status::Modified
    }

    fn reset(&mut self) {
        if let Some(t) = &mut self.tuned {
            t.clear(&mut self.rig);
        }
    }

    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

/// Shares the control cells (the template a fork is taken from); the realization
/// is a copy with its state empty.
impl<F: Float + SimdLanes + Lerp> Clone for MultibandCompressorNode<F> {
    fn clone(&self) -> Self {
        Self {
            rig: Rig {
                shape: self.rig.shape,
                bands: self.rig.bands.clone(),
                splits: self.rig.splits.clone(),
                crossover: self.rig.crossover,
            },
            tuned: self.tuned.as_ref().map(|t| {
                let mut bank = t.bank.clone();
                bank.reset();
                Tuned {
                    bank,
                    applied: None,
                    rate: t.rate,
                    position: 0,
                }
            }),
        }
    }
}

impl<F: Float + SimdLanes + Lerp> ParamNode<F> for MultibandCompressorNode<F> {
    /// Every param declared, the bands' then the crossovers'.
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A clone with every cell detached at its value (the meters too) and
    /// its state cleared.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.rig.splits.detach();
        for b in &mut fork.rig.bands {
            b.detach();
        }
        fork.rig.clear_meters();
        fork
    }
}

/// Inserted with its [`MultibandControls`] as its controls; a fork starts
/// from the values last set through its params.
impl<F: Float + SimdLanes + Lerp> IntoNode<F> for MultibandCompressorNode<F> {
    type Controls = MultibandControls;

    fn into_parts(self) -> NodeParts<MultibandControls, F> {
        let reductions = self
            .rig
            .bands
            .iter()
            .map(|b| b.reduction.handle())
            .collect();
        let NodeParts {
            node,
            controls,
            fork,
            params,
            reclaimer,
            published,
            settings,
        } = tutti_graph::param_parts(self);
        NodeParts {
            node,
            controls: MultibandControls {
                params: controls,
                reductions,
            },
            fork,
            params,
            reclaimer,
            published,
            settings,
        }
    }
}

#[cfg(test)]
mod tests;
