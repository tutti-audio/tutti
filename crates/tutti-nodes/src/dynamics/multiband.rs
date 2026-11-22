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
use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_buffers::{Float, InputOutput, Mono, Planes, PlanesMut};
use tutti_dsp::{
    BandSettings, BankShape, COEFF_INTERVAL, CompressorControls, CrossoverControls, Lerp,
    MultibandBands, MultibandBank, MultibandSettings, MultibandSignals, SimdLanes,
};
use tutti_graph::{
    Cost, Cx, IntoNode, Io, MAX_PARAM_PORTS, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape,
    Status,
};
use tutti_rt::Param;
use tutti_units::{CompressionRatio, Db, Hz, SampleRate, Samples};
use tutti_units::{Seconds, Tail, UnitParam};

use crate::filter::Splits;
use crate::kernel::{
    Grid, first_non_finite, log_non_finite, silence_block, slice_in, slice_out, walk_grid,
};
use crate::ramp::LastGood;

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
    input: BusTopology,
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
    pub fn new(input: BusTopology, bands: MultibandBands) -> Self {
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
        Threshold => Db = &n.rig.bands[b].threshold,
        Knee => Db = &n.rig.bands[b].knee,
        Ratio => CompressionRatio = &n.rig.bands[b].ratio,
        Attack => Seconds = &n.rig.bands[b].attack,
        Release => Seconds = &n.rig.bands[b].release,
        GainDb => Db = &n.rig.bands[b].makeup;
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
            .with_cost(Cost::Heavy)
            .with_tail(Tail::Unknown)
            .with_param_addrs(self.declared_ports().as_slice())
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
                .frames();
        }
        for (b, signal) in signals.band[..nb].iter_mut().enumerate() {
            let band = b as u8;
            // In the order of `BAND_PARAMS`: `declared_param_at` takes its kind as a const
            // generic, which a loop over the array cannot supply.
            *signal = [
                self.declared_param_at::<{ UnitParam::Threshold as u16 }, _>(&io, band)
                    .frames(),
                self.declared_param_at::<{ UnitParam::Knee as u16 }, _>(&io, band)
                    .frames(),
                self.declared_param_at::<{ UnitParam::Ratio as u16 }, _>(&io, band)
                    .frames(),
                self.declared_param_at::<{ UnitParam::Attack as u16 }, _>(&io, band)
                    .frames(),
                self.declared_param_at::<{ UnitParam::Release as u16 }, _>(&io, band)
                    .frames(),
                self.declared_param_at::<{ UnitParam::GainDb as u16 }, _>(&io, band)
                    .frames(),
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
        } = tutti_graph::param_parts(self);
        NodeParts {
            node,
            controls: MultibandControls {
                params: controls,
                reductions,
            },
            fork,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        RATE, amplitude_at, mean, prepared, render_chunked, render_f64, sine, wide_sine,
    };
    use core::f64::consts::PI;
    use tutti_graph::contract::{Direct, drive};
    use tutti_units::UnitAddr;

    const FS: f64 = 48_000.0;
    /// Whole periods of every 5 Hz-grid tone, and of 3 kHz (16 samples).
    const WINDOW: usize = 9_600;
    const SETTLE: usize = 24_000;
    // The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
    // format, so their tolerances are `f32`'s.

    /// Frames a 5 ms ramp takes at 48 kHz.
    const RAMP: usize = 240;
    /// The crossover's flatness tolerance: the `f32` rounding of the input, each band and the
    /// sum, which the lock-in only averages down.
    const TOL: f64 = 1e-6;

    /// A node's params by band and crossover, over its [`ParamSet`].
    struct Ctl {
        set: ParamSet,
        bands: usize,
    }

    /// One band's params, under the names the settings have.
    struct Bd<'a>(&'a Ctl, u8);

    impl Ctl {
        fn new<F: Float + SimdLanes + Lerp>(n: &MultibandCompressorNode<F>) -> Self {
            Self {
                set: n.param_set(),
                bands: n.bands(),
            }
        }

        fn band(&self, b: usize) -> Bd<'_> {
            Bd(self, b as u8)
        }

        /// Writes `v` to `kind` at `index`; the node must have it.
        fn put(&self, kind: UnitParam, index: u8, v: f32) {
            assert!(
                self.set.set(UnitAddr::new(kind, index), f64::from(v)),
                "{kind:?} {index}"
            );
        }

        fn set_crossover(&self, i: usize, hz: Hz) {
            // A param word is an `f32`.
            self.put(UnitParam::Cutoff, i as u8, hz.get() as f32);
        }
    }

    impl Bd<'_> {
        fn put(&self, kind: UnitParam, v: f32) {
            self.0.put(kind, self.1, v);
        }
        fn set_threshold(&self, db: f32) {
            self.put(UnitParam::Threshold, db);
        }
        fn set_knee(&self, db: f32) {
            self.put(UnitParam::Knee, db);
        }
        fn set_ratio(&self, r: f32) {
            self.put(UnitParam::Ratio, r);
        }
        fn set_attack(&self, s: f32) {
            self.put(UnitParam::Attack, s);
        }
        fn set_release(&self, s: f32) {
            self.put(UnitParam::Release, s);
        }
        fn set_makeup(&self, db: f32) {
            self.put(UnitParam::GainDb, db);
        }
        fn set_bypass(&self, on: bool) {
            self.put(UnitParam::Bypass, f32::from(on));
        }
        fn set_solo(&self, on: bool) {
            self.put(UnitParam::Solo, f32::from(on));
        }
    }

    fn node(bands: u8) -> (MultibandCompressorNode<f32>, Ctl) {
        let n = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
            BusTopology::MONO,
            MultibandBands::new(bands).unwrap(),
        ));
        let c = Ctl::new(&n);
        (n, c)
    }

    /// Every band at unity ratio and no makeup: nothing is reduced.
    fn unity(c: &Ctl) {
        for b in 0..c.bands {
            c.band(b).set_ratio(1.0);
        }
    }

    /// The warped frequency ratio of the bilinear transform.
    fn warp(f: f64, fc: f64) -> f64 {
        libm::tan(PI * f / FS) / libm::tan(PI * fc / FS)
    }

    /// How far a constant `0.5` can sit from its fixed point in an `f32` section of the split
    /// `fc` (`k = sqrt 2`, `g = tan(pi fc / fs)`, `a1 = 1 / (1 + g (g + k))`, `d = 1 - a1`, `a2 =
    /// g a1`, `a3 = g a2`): an offset `e` of `ic2` from the input holds `ic1` at `a2 e / d`, and
    /// the section's restoring step `a2 ic1 + a3 v3 = e (a2^2 / d + a3)` rounds away once it is
    /// under half an ulp of the state, `2^-25`, so the offset stalls at `2^-25 / (a2^2 / d +
    /// a3)`. The node is `f32`, so its state is `f32` by design and the stall is its floor. It is an
    /// upper bound: `2^-25` is half an ulp of a state at or above 0.5, the largest half-ulp the
    /// constant's state can have.
    fn dead_band(fc: f64) -> f64 {
        let g = libm::tan(PI * fc / FS);
        let h = g * (g + core::f64::consts::SQRT_2);
        let a1 = 1.0 / (1.0 + h);
        let (d, a2) = (h * a1, g * a1);
        2f64.powi(-25) / (a2 * a2 / d + g * a2)
    }

    /// A unit sine through `node`, steady-state amplitude at `hz`.
    fn gain_at(node: &mut MultibandCompressorNode<f32>, hz: f64) -> f64 {
        let x = sine(hz, FS, SETTLE + WINDOW);
        let out = render_chunked(node, &[x], 512);
        amplitude_at(&out[0][SETTLE..], hz, FS)
    }

    /// An `f32` node holds its crossover in `f32`. A 4-band tree has 3 splits of 3 sections each
    /// (the shared first stage and the two second stages) and 3 all-passes (one per band split
    /// off before a later split): 12 sections. Each rounds its state and its tap at most four
    /// times a frame at a half-ulp of `2^-24` of a value of at most 1, and a section is a
    /// recursive integrator whose state carries a rounding with a gain of up to `1 / g`, `g` the
    /// prewarped integrator gain `tan(pi fc / fs)` of the lowest split (120 Hz): the
    /// unit-magnitude sum is within `12 * 4 * 2^-24 / g` of 1.
    #[test]
    fn an_f32_node_sums_its_bands_within_the_rounding_of_its_sections() {
        let n = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
            BusTopology::MONO,
            MultibandBands::of(4),
        ));
        let c = Ctl::new(&n);
        unity(&c);
        let mut n = prepared(n);
        let a = gain_at(&mut n, 185.0);
        let g = (std::f64::consts::PI * 120.0 / FS).tan();
        let bound = 12.0 * 4.0 * 2f64.powi(-24) / g;
        assert!((a - 1.0).abs() < bound, "{a}");
    }

    /// Unity-ratio, zero-makeup bands reproduce the crossover's sum, which is
    /// allpass: unit magnitude at every frequency, for 2, 3 and 4 bands, in an `f64` graph to
    /// the rounding of the sections: a 4-band tree is at most 12 sections, each rounding its
    /// state and its tap at most four times a frame at a half-ulp of `2^-53` of a value of at
    /// most 1, with the state's gain of up to `1 / g`, `g` the prewarped integrator gain of
    /// the lowest split (120 Hz).
    #[test]
    fn unity_ratio_reproduces_the_allpass_sum() {
        let g = (PI * 120.0 / FS).tan();
        let bound = 12.0 * 4.0 * 2f64.powi(-53) / g;
        for bands in 2..=4 {
            for i in 0..16 {
                let hz =
                    (20.0 * (16_000.0f64 / 20.0).powf(f64::from(i) / 15.0) / 5.0).round() * 5.0;
                let n = MultibandCompressorNode::<f64>::from(MultibandCompressorConfig::new(
                    BusTopology::MONO,
                    MultibandBands::new(bands).unwrap(),
                ));
                unity(&Ctl::new(&n));
                let x = wide_sine(hz, FS, SETTLE + WINDOW);
                let out = render_f64(n, RATE, &[&x], x.len());
                let a = amplitude_at(&out[0][SETTLE..], hz, FS);
                assert!((a - 1.0).abs() < bound, "{bands} bands at {hz} Hz: {a}");
            }
        }
    }

    /// The signal of the static-curve tests: a loud DC level (the band-0
    /// content with an exact detector reading) and a quiet 3 kHz tone (band 2).
    fn dc_and_tone(dc: f32, tone: f32) -> Vec<f32> {
        sine(3_000.0, FS, SETTLE + WINDOW)
            .into_iter()
            .map(|s| dc + tone * s)
            .collect()
    }

    /// Loud low content compresses its own band by the compressor's static
    /// curve and leaves a quiet tone in band 2 alone.
    ///
    /// The loud content is a constant 0.5 (-6.02 dB): it is all in band 0 once
    /// the crossover settles, and a constant is the one input whose detector
    /// reading is exact, so the settled reduction is the curve's value and not
    /// a ripple. With a -20 dB threshold at 4:1 the reduction is
    /// `(-6.02 + 20) * 0.75` dB, and a 2 dB makeup rides on it.
    ///
    /// The settled follower is the curve's value to far under the `f32` output's rounding of
    /// the level, so the tolerance (5e-6 of the level) is generous.
    ///
    /// The tone is 0.01 (-40 dB), under every band's -18 dB threshold. Band
    /// 0 carries `1 / (1 + 25^4) = 2.6e-6` of it, so it is unchanged to far
    /// less than the `f32` floor: the input, band 0's output and the sum
    /// each round the 0.5 background to a grid of half-ulp `3e-8`, and
    /// that rounding repeats at the tone's 16-sample period, so the tone's
    /// bin is good to six half-ulps, `2e-7`.
    #[test]
    fn a_loud_low_band_follows_the_static_curve_and_a_quiet_band_is_left_alone() {
        let (n, c) = node(4);
        let b0 = c.band(0);
        b0.set_threshold(-20.0);
        b0.set_ratio(4.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        b0.set_makeup(2.0);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[dc_and_tone(0.5, 0.01)], 512);
        let y = &out[0][SETTLE..];

        let level_db = 20.0 * libm::log10(0.5);
        let reduction = (level_db + 20.0) * (1.0 - 1.0 / 4.0);
        let expected = 0.5 * libm::pow(10.0, (2.0 - reduction) / 20.0);
        let dc = mean(y);
        assert!(
            (dc - expected).abs() < 5e-6 * expected,
            "band 0 settled at {dc}, the curve says {expected}"
        );
        let tone = amplitude_at(y, 3_000.0, FS);
        assert!((tone - 0.01).abs() < 2e-7, "the quiet tone is {tone}");

        // The meter reads the reduction the curve gives, and only on band 0.
        let meter = n.gain_reduction_db(0).get();
        assert!(
            (meter - reduction).abs() < 1e-3,
            "meter {meter} dB vs {reduction}"
        );
        // The DC's onset is a step through the crossover, which rings the
        // upper bands over their threshold for a moment; their 100 ms release
        // has taken all but a hair of it off by the end.
        for b in 1..4 {
            let held = n.gain_reduction_db(b);
            assert!(held < Db::new(1e-3), "band {b} still reduces {held:?}");
        }
        assert_eq!(n.gain_reduction_db(9), Db::UNITY);
    }

    /// Makeup is applied per band, after reduction, on every band.
    ///
    /// Two bands split at 1 kHz: DC is all band 0, and 10 kHz is
    /// `x^4 / (1 + x^4)` band 1 with `x` the warped ratio, the rest
    /// (`1e-4`) band 0. Band 0 -3 dB and band 1 +6 dB.
    #[test]
    fn makeup_applies_per_band() {
        let (n, c) = node(2);
        unity(&c);
        c.band(0).set_makeup(-3.0);
        c.band(1).set_makeup(6.0);
        let mut n = prepared(n);
        let tone: Vec<f32> = sine(10_000.0, FS, SETTLE + WINDOW)
            .into_iter()
            .map(|s| 0.5 + 0.1 * s)
            .collect();
        let out = render_chunked(&mut n, &[tone], 512);
        let y = &out[0][SETTLE..];
        let dc = mean(y);
        let want_dc = 0.5 * libm::pow(10.0, -3.0 / 20.0);
        assert!((dc - want_dc).abs() < 1e-6, "{dc} vs {want_dc}");
        let g1 = libm::pow(10.0, 6.0 / 20.0);
        let leak = 1.0 / (1.0 + warp(10_000.0, 1_000.0).powi(4));
        // Band 0's `leak` of the tone runs at -3 dB instead of +6 dB.
        let tol = 0.1 * leak * (g1 - 0.7) + 1e-6;
        let amp = amplitude_at(y, 10_000.0, FS);
        assert!((amp - 0.1 * g1).abs() < tol, "{amp} vs {}", 0.1 * g1);
    }

    /// A bypassed band passes unreduced and without makeup; releasing the
    /// bypass compresses again. In an `f64` graph the bypassed sum is the input to the
    /// rounding of the sections, and the compressed level is the curve's value to the rounding
    /// of the gain.
    #[test]
    fn bypass_passes_the_band_unprocessed() {
        let n = MultibandCompressorNode::<f64>::from(MultibandCompressorConfig::new(
            BusTopology::MONO,
            MultibandBands::of(4),
        ));
        let c = Ctl::new(&n);
        let b0 = c.band(0);
        b0.set_threshold(-30.0);
        b0.set_ratio(8.0);
        b0.set_makeup(6.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        b0.set_bypass(true);
        let mut d = Direct::<_, f64>::with_format(n, RATE, SETTLE + WINDOW);
        d.inputs_mut().plane_mut(0).samples_mut().fill(0.5);
        d.block();
        let g = (PI * 120.0 / FS).tan();
        let rounding = 12.0 * 4.0 * 2f64.powi(-53) / g;
        assert!((mean(&d.output().plane(0).samples()[SETTLE..]) - 0.5).abs() < rounding);
        assert_eq!(d.node.gain_reduction_db(0), Db::UNITY);
        b0.set_bypass(false);
        d.block();
        let out = [d.output().plane(0).samples().to_vec()];
        // Over -30 dB at 8:1 the 0.5 DC (-6.02 dB) is reduced by
        // `(30 - 6.02) * 7/8`, and the 6 dB makeup rides on it.
        let reduction = (30.0 + 20.0 * libm::log10(0.5)) * 7.0 / 8.0;
        let want = 0.5 * libm::pow(10.0, (6.0 - reduction) / 20.0);
        let dc = mean(&out[0][SETTLE..]);
        assert!((dc - want).abs() < rounding * want, "{dc} vs {want}");
        let meter = d.node.gain_reduction_db(0).get();
        assert!((meter - reduction).abs() < 1e-3, "{meter} vs {reduction}");
    }

    /// Toggling the bypass ramps the band to unity over 5 ms (240 frames at 48 kHz), and the
    /// compressor keeps running meanwhile.
    ///
    /// DC 0.5 through band 0 at -30 dB, 2:1 is reduced by `(30 - 6.02) / 2` dB: 0.1253
    /// before the bypass. Bypassed, the output climbs to 0.5 in 240 equal steps, so the
    /// first frame is within 1.5 steps of 0.1253; releasing it ramps back down to the
    /// *running* detector's gain (0.1253 again, to 1e-3: a detector reset by
    /// the bypass would end near 0.5).
    #[test]
    fn toggling_bypass_ramps_and_keeps_the_compressor_warm() {
        let (n, c) = node(2);
        let b0 = c.band(0);
        b0.set_threshold(-30.0);
        b0.set_ratio(2.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        let mut n = prepared(n);
        render_chunked(&mut n, &[vec![0.5f32; SETTLE]], 512);
        let reduced = 0.5 * libm::pow(10.0, -(30.0 + 20.0 * libm::log10(0.5)) / 2.0 / 20.0);
        let step = (0.5 - reduced) / RAMP as f64;

        b0.set_bypass(true);
        let up = render_chunked(&mut n, &[vec![0.5f32; RAMP]], 64).remove(0);
        assert!(
            (up[0] as f64 - reduced).abs() <= 1.5 * step,
            "first {}",
            up[0]
        );
        assert!((up[RAMP - 1] - 0.5).abs() < 1e-6, "last {}", up[RAMP - 1]);
        assert!(up.windows(2).all(|w| w[1] >= w[0]), "monotone rise");
        // Held bypassed for a while, then released.
        render_chunked(&mut n, &[vec![0.5f32; 4_800]], 512);
        assert_eq!(n.gain_reduction_db(0), Db::UNITY);
        b0.set_bypass(false);
        let down = render_chunked(&mut n, &[vec![0.5f32; RAMP]], 64).remove(0);
        assert!(
            (down[0] - 0.5).abs() as f64 <= 1.5 * step,
            "first {}",
            down[0]
        );
        assert!(
            (down[RAMP - 1] as f64 - reduced).abs() < 1e-3,
            "last {}",
            down[RAMP - 1]
        );
        assert!(down.windows(2).all(|w| w[1] <= w[0]), "monotone fall");
    }

    /// The meter holds the largest reduction of the block, not what was reached since the
    /// start.
    ///
    /// DC 0.5 reduced 10.48 dB (-20 dB, 4:1), then a block of silence with a
    /// 1 ms release: band 0's level (the low-pass's slow tail) starts the
    /// block near 0.5 and the follower falls across it, so the first block's
    /// largest reduction is nearly the full one, and the blocks after it, with the
    /// follower released, read nearly none.
    #[test]
    fn the_meter_holds_the_blocks_largest_reduction() {
        let (n, c) = node(2);
        let b0 = c.band(0);
        b0.set_threshold(-20.0);
        b0.set_ratio(4.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        let mut n = prepared(n);
        render_chunked(&mut n, &[vec![0.5f32; SETTLE]], 512);
        render_chunked(&mut n, &[vec![0.0f32; 64]], 64);
        let first = n.gain_reduction_db(0);
        assert!(first > Db::new(10.0), "{first:?}");
        render_chunked(&mut n, &[vec![0.0f32; 4_096]], 64);
        let later = n.gain_reduction_db(0);
        assert!(later < Db::new(1.0), "the meter still holds {later:?}");
    }

    /// Two soloed bands are both heard, and the bands being phase-aligned, their
    /// amplitudes add.
    ///
    /// Bands 1 and 2 of four at 3 kHz: band 1 is `HP(120) * LP(1 kHz)` (and a
    /// unit all-pass), band 2 `HP(120) * HP(1 kHz) * LP(6 kHz)`.
    #[test]
    fn several_solos_are_all_heard() {
        let (n, c) = node(4);
        unity(&c);
        c.band(1).set_solo(true);
        c.band(2).set_solo(true);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[dc_and_tone(0.5, 0.2)], 512);
        let y = &out[0][SETTLE..];
        let hp = |fc: f64| {
            let x4 = warp(3_000.0, fc).powi(4);
            x4 / (1.0 + x4)
        };
        let lp = |fc: f64| 1.0 / (1.0 + warp(3_000.0, fc).powi(4));
        let want = 0.2 * hp(120.0) * (lp(1_000.0) + hp(1_000.0) * lp(6_000.0));
        let amp = amplitude_at(y, 3_000.0, FS);
        assert!((amp - want).abs() < 1e-6, "{amp} vs {want}");
        assert!(
            mean(y).abs() < 1e-6,
            "the DC (band 0) is heard: {}",
            mean(y)
        );
    }

    /// Releasing a solo brings the muted bands back over 5 ms, rising.
    #[test]
    fn un_soloing_ramps_the_bands_back() {
        let (n, c) = node(2);
        unity(&c);
        c.band(1).set_solo(true);
        let mut n = prepared(n);
        render_chunked(&mut n, &[vec![0.5f32; SETTLE]], 512);
        c.band(1).set_solo(false);
        let y = render_chunked(&mut n, &[vec![0.5f32; RAMP]], 64).remove(0);
        assert!(y[0].abs() <= 1.5 * 0.5 / RAMP as f32, "first {}", y[0]);
        assert!((y[RAMP - 1] - 0.5).abs() < 1e-6, "last {}", y[RAMP - 1]);
        assert!(y.windows(2).all(|w| w[1] >= w[0]), "monotone rise");
    }

    /// A band both soloed and bypassed is heard unprocessed; bypassing a band
    /// another band's solo mutes leaves it muted.
    #[test]
    fn solo_and_bypass_combine() {
        let (n, c) = node(2);
        let b0 = c.band(0);
        b0.set_threshold(-30.0);
        b0.set_ratio(8.0);
        b0.set_makeup(6.0);
        b0.set_bypass(true);
        b0.set_solo(true);
        let mut n = prepared(n);
        let x = vec![0.5f32; SETTLE + WINDOW];
        let out = render_chunked(&mut n, std::slice::from_ref(&x), 512);
        assert!((mean(&out[0][SETTLE..]) - 0.5).abs() < TOL);
        b0.set_solo(false);
        c.band(1).set_solo(true);
        let out = render_chunked(&mut n, &[x], 512);
        assert!(
            mean(&out[0][SETTLE..]).abs() < TOL,
            "{}",
            mean(&out[0][SETTLE..])
        );
    }

    /// A crossover above the prepare rate's limit does not freeze the others.
    ///
    /// Four bands at 22.05 kHz with the top crossover at 15 kHz (over
    /// Nyquist): it is held under the rate (fitted, not refused), and moving the lowest
    /// crossover still reaches the filter: band 0 soloed at unity is `1 / (1 + x^4)` of a
    /// tone, which at the split itself is a half.
    #[test]
    fn a_crossover_above_the_rate_does_not_freeze_the_others() {
        let (n, c) = node(4);
        unity(&c);
        c.band(0).set_solo(true);
        c.set_crossover(2, Hz::new(15_000.0));
        let mut n = tutti_graph::contract::prepared(n, SampleRate::SR_22K05, 1024);
        c.set_crossover(0, Hz::new(150.0));
        // 150 Hz at 22.05 kHz is 147 samples a period.
        let x = sine(150.0, 22_050.0, 12_000 + 5_880);
        let out = render_chunked(&mut n, &[x], 512);
        let amp = amplitude_at(&out[0][12_000..], 150.0, 22_050.0);
        assert!((amp - 0.5).abs() < TOL, "{amp}");
    }

    /// A soloed band is all that is heard: the output is that band's
    /// analytic share of the input.
    ///
    /// Band 2 of four (1 kHz to 6 kHz) of a 3 kHz tone is
    /// `HP(120) * HP(1 kHz) * LP(6 kHz)`, each `x^4/(1+x^4)` or `1/(1+x^4)`
    /// with the warped ratio; a constant is in band 0 and must vanish.
    #[test]
    fn solo_plays_only_the_soloed_band() {
        let (n, c) = node(4);
        unity(&c);
        c.band(2).set_solo(true);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[dc_and_tone(0.5, 0.2)], 512);
        let y = &out[0][SETTLE..];
        let hp = |fc: f64| {
            let x4 = warp(3_000.0, fc).powi(4);
            x4 / (1.0 + x4)
        };
        let lp = 1.0 / (1.0 + warp(3_000.0, 6_000.0).powi(4));
        let want = 0.2 * hp(120.0) * hp(1_000.0) * lp;
        let amp = amplitude_at(y, 3_000.0, FS);
        assert!((amp - want).abs() < 1e-6, "{amp} vs {want}");
        assert!(
            mean(y).abs() < 1e-6,
            "the unsoloed DC is heard: {}",
            mean(y)
        );
        // Un-soloing restores the whole sum.
        c.band(2).set_solo(false);
        let out = render_chunked(&mut n, &[dc_and_tone(0.5, 0.2)], 512);
        // The constant passes the low band's four sections (the split at 120 Hz twice, the
        // all-passes of the splits above it), each within its own dead band.
        let stall = 2.0 * dead_band(120.0) + dead_band(1_000.0) + dead_band(6_000.0);
        assert!((mean(&out[0][SETTLE..]) - 0.5).abs() < stall);
    }

    /// Soloing mutes the other bands over 5 ms, not at once.
    ///
    /// DC through band 0, then band 1 soloed: the next 240 frames fall
    /// from the old level to 0 in 240 equal steps, so the first frame is
    /// within a step and a half of 0.5 and the last is 0.
    #[test]
    fn a_solo_ramps_the_mute() {
        let (n, c) = node(2);
        unity(&c);
        let mut n = prepared(n);
        let settle = vec![0.5f32; SETTLE];
        render_chunked(&mut n, &[settle], 512);
        c.band(1).set_solo(true);
        let y = render_chunked(&mut n, &[vec![0.5f32; RAMP]], 64).remove(0);
        assert!(
            (y[0] - 0.5).abs() <= 1.5 * 0.5 / RAMP as f32,
            "first {}",
            y[0]
        );
        assert!(y[RAMP - 1].abs() < 1e-6, "last {}", y[RAMP - 1]);
        assert!(y.windows(2).all(|w| w[1] <= w[0]), "monotone fall");
    }

    /// Detection is linked across channels within a band: a quiet channel is
    /// reduced by what the loud one drives.
    ///
    /// L = 0.05 and R = 0.5 as DC through band 0 at -20 dB, 4:1. Unlinked,
    /// L (-26 dB) is under the threshold and untouched; linked, both are
    /// scaled by the one gain the loud channel sets.
    #[test]
    fn detection_is_linked_across_channels() {
        let n = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
            BusTopology::STEREO,
            MultibandBands::of(2),
        ));
        let c = Ctl::new(&n);
        let b0 = c.band(0);
        b0.set_threshold(-20.0);
        b0.set_ratio(4.0);
        b0.set_attack(0.005);
        b0.set_release(0.005);
        let mut n = prepared(n);
        let len = 12_000;
        let out = render_chunked(&mut n, &[vec![0.05; len], vec![0.5; len]], 512);
        let (l, r) = (mean(&out[0][len - 2_000..]), mean(&out[1][len - 2_000..]));
        let gain = r / 0.5;
        let want = libm::pow(10.0, -(20.0 * libm::log10(0.5) + 20.0) * 0.75 / 20.0);
        assert!((gain - want).abs() < 1e-5, "{gain} vs {want}");
        assert!(
            (l / 0.05 - gain).abs() < 1e-5,
            "L gain {} vs R {gain}",
            l / 0.05
        );
    }

    /// A node has the band count it was made with and a width of the bus it was made over,
    /// the empty bus (no ports) included.
    #[test]
    fn a_node_has_the_bands_and_width_it_was_made_with() {
        for bands in 2..=4u8 {
            let n = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
                BusTopology::STEREO,
                MultibandBands::new(bands).unwrap(),
            ));
            let bands = usize::from(bands);
            assert_eq!(
                (n.bands(), n.clone().into_parts().controls.bands()),
                (bands, bands)
            );
            assert_eq!(n.layout(), ChannelLayout::STEREO);
        }
        let empty = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
            BusTopology::EMPTY,
            MultibandBands::of(3),
        ));
        assert_eq!(empty.layout(), ChannelLayout::EMPTY);
        assert_eq!(empty.shape().audio_in().count(), 0);
    }

    /// The defaults for each band count: the crossovers, and each band's
    /// settings.
    #[test]
    fn the_defaults_are_the_documented_ones() {
        let at = |c: &Ctl, kind, i| c.set.get(UnitAddr::new(kind, i));
        let (_, c) = node(4);
        let cross = [0, 1, 2, 3].map(|i| at(&c, UnitParam::Cutoff, i));
        assert_eq!(cross, [Some(120.0), Some(1_000.0), Some(6_000.0), None]);
        assert_eq!(at(&node(3).1, UnitParam::Cutoff, 1), Some(3_000.0));
        assert_eq!(at(&node(2).1, UnitParam::Cutoff, 0), Some(1_000.0));
        for b in 0..4 {
            let got = BAND_PARAMS.map(|k| at(&c, k, b));
            let want = [-18.0, 0.0, 2.0, 0.010, 0.100, 0.0].map(Some);
            assert_eq!(got, want, "band {b}");
            assert_eq!(at(&c, UnitParam::Bypass, b), Some(0.0));
            assert_eq!(at(&c, UnitParam::Solo, b), Some(0.0));
        }
        assert_eq!(at(&c, UnitParam::Threshold, 4), None);
    }

    /// A non-finite threshold, makeup or crossover written mid-stream reads as
    /// unchanged.
    ///
    /// The crossover cells have two guards (`LastGood` and `set_frequencies`'s own check),
    /// so either alone is enough by design.
    #[test]
    fn non_finite_writes_never_reach_the_state() {
        for (bad, bad_hz) in [
            (f32::NAN, f64::NAN),
            (f32::INFINITY, f64::INFINITY),
            (f32::NEG_INFINITY, f64::NEG_INFINITY),
        ] {
            let (n, c) = node(3);
            let mut n = prepared(n);
            let x = sine(440.0, FS, 2_048);
            let finite = |n: &mut MultibandCompressorNode<f32>| {
                render_chunked(n, std::slice::from_ref(&x), 512)[0]
                    .iter()
                    .all(|s| s.is_finite())
            };
            assert!(finite(&mut n));
            c.band(1).set_threshold(bad);
            c.band(1).set_makeup(bad);
            c.band(2).set_makeup(bad);
            c.band(2).set_knee(bad);
            c.band(2).set_ratio(bad);
            c.band(0).set_attack(bad);
            c.band(0).set_release(bad);
            c.set_crossover(0, Hz::new(bad_hz));
            c.set_crossover(1, Hz::new(bad_hz));
            assert!(finite(&mut n), "{bad}");
            c.band(1).set_threshold(-30.0);
            c.band(1).set_makeup(0.0);
            c.band(2).set_makeup(0.0);
            assert!(finite(&mut n), "{bad} then finite");
        }
    }

    /// A fork starts from the values set through every address and shares no
    /// cell with the live node, whatever the band.
    #[test]
    fn a_fork_shares_no_cell() {
        tutti_graph::contract::assert_param_fork(MultibandCompressorNode::<f32>::from(
            MultibandCompressorConfig::new(BusTopology::STEREO, MultibandBands::of(4)),
        ));
    }

    /// A fork has its own meter: the live node's reads unity while the fork
    /// reduces.
    #[test]
    fn a_fork_has_its_own_meter() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-20.0);
        c.band(0).set_ratio(4.0);
        let live = n.clone();
        let mut fork = prepared(n.fork_fresh());
        render_chunked(&mut fork, &[vec![0.5f32; 4_096]], 512);
        assert!(fork.gain_reduction_db(0) > Db::new(5.0));
        assert_eq!(live.gain_reduction_db(0), Db::UNITY);
    }

    /// The fork the graph takes (`into_parts().fork`) renders from the values
    /// set when it was taken, not from later writes, and leaves the live
    /// node's meter alone.
    ///
    /// Band 0 at -20 dB, 4:1 reduces a DC of 0.5 by the analytic 10.48 dB.
    /// After the fork is taken the live node is bypassed and given +20 dB of
    /// makeup: a fork sharing either cell would not render the reduced level.
    #[test]
    fn the_graphs_fork_renders_from_the_values_it_took() {
        use tutti_graph::RenderMode;
        let (n, _) = node(2);
        let parts = n.into_parts();
        let live = Ctl {
            set: parts.controls.params().clone(),
            bands: parts.controls.bands(),
        };
        let b0 = live.band(0);
        b0.set_threshold(-20.0);
        b0.set_ratio(4.0);
        b0.set_attack(0.001);
        b0.set_release(0.001);
        let forked = parts.fork.unwrap().fork(RenderMode::Offline).unwrap();
        b0.set_bypass(true);
        b0.set_makeup(20.0);
        b0.set_solo(false);
        let mut fork = forked.node;
        fork.prepare(&Prepare::new(
            RATE,
            tutti_buffers::MaxBlock::of(tutti_units::Samples::new(1024)),
        ));
        let out = render_chunked(fork.as_mut(), &[vec![0.5f32; SETTLE]], 512);
        let reduction = (20.0 * libm::log10(0.5) + 20.0) * 0.75;
        let want = 0.5 * libm::pow(10.0, -reduction / 20.0);
        let dc = mean(&out[0][SETTLE - WINDOW..]);
        assert!((dc - want).abs() < 5e-6 * want, "{dc} vs {want}");
        assert_eq!(
            parts.controls.gain_reduction_db(0),
            Db::UNITY,
            "the fork wrote the live meter"
        );
    }

    /// The meters are reachable from the controls once inserted, and read
    /// what the node measured: the analytic 10.48 dB on band 0, nothing on
    /// the bands without content, unity past the last.
    #[test]
    fn the_controls_read_the_meters_after_insertion() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-20.0);
        c.band(0).set_ratio(4.0);
        c.band(0).set_attack(0.001);
        c.band(0).set_release(0.001);
        let mut parts = n.into_parts();
        parts.node.prepare(&Prepare::new(
            RATE,
            tutti_buffers::MaxBlock::of(tutti_units::Samples::new(1024)),
        ));
        render_chunked(parts.node.as_mut(), &[vec![0.5f32; SETTLE]], 512);
        let reduction = (20.0 * libm::log10(0.5) + 20.0) * 0.75;
        let m = parts.controls.gain_reduction_db(0).get();
        assert!((m - reduction).abs() < 1e-3, "{m} vs {reduction}");
        assert!(parts.controls.gain_reduction_db(1) < Db::new(1e-3));
        assert_eq!(parts.controls.gain_reduction_db(2), Db::UNITY);
    }

    /// Zero latency, `Tail::Unknown`, heavy, and one param port per band
    /// parameter then per crossover: 27 at four bands, within the graph's cap.
    #[test]
    fn the_shape_declares_the_indexed_ports() {
        let n = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
            BusTopology::STEREO,
            MultibandBands::of(4),
        ));
        let s = n.shape();
        assert_eq!((s.audio_in().count(), s.audio_out().count()), (2, 2));
        assert_eq!(s.latency, tutti_units::Latency::default());
        assert_eq!(s.tail, Tail::Unknown);
        assert_eq!(s.cost, Cost::Heavy);
        assert_eq!(s.params.len(), 4 * 6 + 3);
        assert!(s.params.len() <= MAX_PARAM_PORTS);
        for (b, band) in s.params.as_slice().chunks(6).take(4).enumerate() {
            let want: Vec<_> = BAND_PARAMS
                .into_iter()
                .map(|k| UnitAddr::new(k, b as u8))
                .collect();
            assert_eq!(band, &want[..], "band {b}");
        }
        let tail: Vec<_> = (0..3)
            .map(|i| UnitAddr::new(UnitParam::Cutoff, i))
            .collect();
        assert_eq!(&s.params.as_slice()[24..], &tail[..]);
        let two = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
            BusTopology::MONO,
            MultibandBands::of(2),
        ));
        assert_eq!(two.shape().params.len(), 2 * 6 + 1);
    }

    /// Port `k`'s base is the cell of the address the shape names for it,
    /// for every port of every band count, and the toggles have a cell but no
    /// port.
    #[test]
    fn each_port_reads_the_cell_of_its_address() {
        for bands in 2..=4 {
            let n = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
                BusTopology::MONO,
                MultibandBands::new(bands).unwrap(),
            ));
            let set = n.param_set();
            let ports = n.shape().params;
            for (k, &addr) in ports.as_slice().iter().enumerate() {
                let v = 100.0 + k as f32;
                assert!(set.set(addr, f64::from(v)));
                assert_eq!(
                    n.param_base(k),
                    Some(f64::from(v)),
                    "{bands} bands, port {k}: {addr:?}"
                );
            }
            assert_eq!(n.param_base(ports.len()), None);
            let cells: Vec<_> = set.params().collect();
            assert_eq!(cells.len(), ports.len() + 2 * usize::from(bands));
            for b in 0..bands {
                for kind in [UnitParam::Bypass, UnitParam::Solo] {
                    let a = UnitAddr::new(kind, b);
                    assert!(cells.contains(&a) && !ports.as_slice().contains(&a));
                }
            }
        }
    }

    /// A write to one band's address reaches that band and no other: with
    /// the band-1 ratio at 1 the 10 kHz tone it carries is not reduced, with
    /// the band-0 ratio at 1 it still is.
    ///
    /// The tone (0.5, -6 dB) is over the default -18 dB threshold in band 1;
    /// band 0 sees 1e-4 of it, far under. A 2:1 band-1 reduction is about
    /// 6 dB; at ratio 1 the detector's target is exactly zero.
    #[test]
    fn a_write_to_band_1_reaches_band_1_only() {
        let (n, c) = node(2);
        for b in 0..2 {
            c.band(b).set_attack(0.001);
            c.band(b).set_release(0.001);
        }
        let mut n = prepared(n);
        let tone: Vec<f32> = sine(10_000.0, FS, 8_192)
            .into_iter()
            .map(|s| 0.5 * s)
            .collect();
        render_chunked(&mut n, std::slice::from_ref(&tone), 512);
        assert!(
            n.gain_reduction_db(1) > Db::new(3.0),
            "{:?}",
            n.gain_reduction_db(1)
        );
        c.band(0).set_ratio(1.0);
        render_chunked(&mut n, std::slice::from_ref(&tone), 512);
        assert!(
            n.gain_reduction_db(1) > Db::new(3.0),
            "band 0's write moved band 1"
        );
        c.band(1).set_ratio(1.0);
        render_chunked(&mut n, std::slice::from_ref(&tone), 512);
        render_chunked(&mut n, std::slice::from_ref(&tone), 512);
        assert!(
            n.gain_reduction_db(1) < Db::new(1e-4),
            "{:?}",
            n.gain_reduction_db(1)
        );
    }

    /// The knee follows the static curve: DC 0.5 (-6.02 dB) over a -10 dB
    /// threshold, 4:1, 12 dB wide, is inside the knee and reduced by
    /// `slope * (x - (T - W / 2))^2`, `slope = (1 - 1/R) / (2W)`: 3.11 dB,
    /// where the hard knee gives 2.98 dB.
    #[test]
    fn the_knee_follows_the_static_curve() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-10.0);
        c.band(0).set_ratio(4.0);
        c.band(0).set_knee(12.0);
        c.band(0).set_attack(0.001);
        c.band(0).set_release(0.001);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[vec![0.5f32; SETTLE + WINDOW]], 512);
        let x = 20.0 * libm::log10(0.5) - (-10.0 - 6.0);
        let reduction = 0.75 / 24.0 * x * x;
        let want = 0.5 * libm::pow(10.0, -reduction / 20.0);
        let dc = mean(&out[0][SETTLE..]);
        assert!((dc - want).abs() < 5e-6 * want, "{dc} vs {want}");
    }

    /// A toggle is on at 0.5 and above, off below, and a NaN is off.
    ///
    /// DC 0.5 through band 0 at -30 dB, 8:1 reduces by `(-6.02 + 30) * 7/8`;
    /// bypassed it is not reduced at all.
    #[test]
    fn a_toggle_is_on_from_one_half() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-30.0);
        c.band(0).set_ratio(8.0);
        let mut n = prepared(n);
        let x = vec![0.5f32; 4_096];
        for (v, bypassed) in [(0.49, false), (0.5, true), (f32::NAN, false), (1.0, true)] {
            c.put(UnitParam::Bypass, 0, v);
            for _ in 0..4 {
                render_chunked(&mut n, std::slice::from_ref(&x), 512);
            }
            let reduced = n.gain_reduction_db(0) > Db::new(1.0);
            assert_eq!(!reduced, bypassed, "bypass at {v}");
        }
    }

    /// Raw writes are clamped as the node reads them: a ratio under 1 is 1
    /// (no expansion), negative times are 0.
    ///
    /// A band-0 threshold of -40 dB on a DC of 0.5 with a ratio written 0.25
    /// would expand by 100 dB if the ratio were taken raw; clamped, the DC
    /// passes untouched.
    #[test]
    fn raw_writes_are_clamped_as_the_node_reads_them() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-40.0);
        c.band(0).set_ratio(0.25);
        c.band(0).set_attack(-1.0);
        c.band(0).set_release(-1.0);
        let mut n = prepared(n);
        let out = render_chunked(&mut n, &[vec![0.5f32; 4_096]], 512);
        assert!(out[0].iter().all(|s| s.is_finite()));
        assert_eq!(
            n.gain_reduction_db(0),
            Db::UNITY,
            "no expansion, no reduction"
        );
        assert!((mean(&out[0][3_000..]) - 0.5).abs() < 1e-5);
    }

    /// The crossover param moves the split: with band 0 soloed at unity, a
    /// 2 kHz tone is `1 / (1 + x^4)` of itself, `x` the warped ratio to the
    /// split, and at a 2 kHz split that is 0.5 (each branch -6.02 dB).
    #[test]
    fn a_crossover_param_moves_the_split() {
        let (n, c) = node(2);
        unity(&c);
        c.band(0).set_solo(true);
        let mut n = prepared(n);
        let before = gain_at(&mut n, 2_000.0);
        assert!((before - 1.0 / (1.0 + warp(2_000.0, 1_000.0).powi(4))).abs() < TOL);
        c.set_crossover(0, Hz::new(2_000.0));
        let after = gain_at(&mut n, 2_000.0);
        assert!((after - 0.5).abs() < TOL, "{after}");
    }

    /// Crossover writes that are out of order, not above 0 or not finite are
    /// refused as a whole and the last good set stands; a consistent set
    /// lands.
    ///
    /// Band 0 soloed at unity is half of a tone at its split (120 Hz), and nearly all of
    /// it once the split is at 5 kHz.
    #[test]
    fn a_bad_crossover_set_is_held_until_consistent() {
        let (n, c) = node(4);
        unity(&c);
        c.band(0).set_solo(true);
        let mut n = prepared(n);
        // 120 Hz is 400 samples a period: 4000 frames is ten of them.
        let x = sine(120.0, FS, 4_000);
        let amp = |n: &mut MultibandCompressorNode<f32>| {
            render_chunked(n, std::slice::from_ref(&x), 500);
            amplitude_at(
                &render_chunked(n, std::slice::from_ref(&x), 500)[0],
                120.0,
                FS,
            )
        };
        for bad in [5_000.0, -1.0, 0.0, f64::NAN] {
            c.set_crossover(0, Hz::new(bad));
            let a = amp(&mut n);
            assert!((a - 0.5).abs() < 2e-3, "{bad}: {a}");
        }
        // Written top-down with a block between: the set stays out of order
        // (the lowest cell still reads 0) until the last write.
        c.set_crossover(2, Hz::new(9_000.0));
        assert!((amp(&mut n) - 0.5).abs() < 2e-3);
        c.set_crossover(1, Hz::new(8_000.0));
        assert!((amp(&mut n) - 0.5).abs() < 2e-3);
        c.set_crossover(0, Hz::new(5_000.0));
        let a = amp(&mut n);
        assert!(a > 0.99, "the consistent set arrived: {a}");
    }

    /// A driven make-up port moves its own band: band 1 alone heard (band 0
    /// soloed out), a 10 kHz tone at unity ratio is `x^4 / (1 + x^4)` of
    /// itself through the high-pass, times the driven +6 dB; the cell stays
    /// 0 dB.
    #[test]
    fn a_driven_makeup_port_moves_its_band() {
        let (n, c) = node(2);
        unity(&c);
        c.band(1).set_solo(true);
        let mut n = prepared(n);
        let port = PER_BAND + 5;
        let x = sine(10_000.0, FS, SETTLE + WINDOW);
        let signal = [6.0f32; 512];
        let mut params: Vec<Option<&[f32]>> = vec![None; port + 1];
        let mut y = Vec::new();
        for chunk in x.chunks(512) {
            params[port] = Some(&signal[..chunk.len()]);
            y.extend_from_slice(&drive(&mut n, RATE, Planes::new(&[chunk]).unwrap(), &params)[0]);
        }
        let x4 = warp(10_000.0, 1_000.0).powi(4);
        let want = x4 / (1.0 + x4) * libm::pow(10.0, 6.0 / 20.0);
        let amp = amplitude_at(&y[SETTLE..], 10_000.0, FS);
        assert!((amp - want).abs() < 1e-5, "{amp} vs {want}");
    }

    /// A driven threshold port is read per frame by its own band: band 0's port, driven to
    /// -30 dB over a cell at 0 dB (nothing to reduce), reduces band 0 by the static curve;
    /// band 1's port, driven alike, leaves band 0 alone.
    ///
    /// DC 0.5 is all band 0, at 4:1 over -30 dB: `(-6.02 + 30) * 3/4` dB.
    #[test]
    fn a_driven_threshold_port_is_read_per_frame_by_its_band() {
        let meter_driving = |port: usize| {
            let (n, c) = node(2);
            for b in 0..2 {
                c.band(b).set_threshold(0.0);
                c.band(b).set_ratio(4.0);
                c.band(b).set_attack(0.001);
                c.band(b).set_release(0.001);
            }
            let mut n = prepared(n);
            let signal = [-30.0f32; 512];
            let mut params: Vec<Option<&[f32]>> = vec![None; port + 1];
            for _ in 0..SETTLE / 512 {
                params[port] = Some(&signal);
                drive(
                    &mut n,
                    RATE,
                    Planes::new(&[&[0.5f32; 512]]).unwrap(),
                    &params,
                );
            }
            n.gain_reduction_db(0)
        };
        let reduction = (20.0 * libm::log10(0.5) + 30.0) * 0.75;
        let driven = meter_driving(0).get();
        assert!((driven - reduction).abs() < 0.01, "{driven} vs {reduction}");
        let other = meter_driving(PER_BAND);
        assert!(
            other < Db::new(1e-3),
            "band 1's port reduced band 0: {other:?}"
        );
    }

    /// A driven control is sampled at the stream grid's solve frame, not at the block's last
    /// frame: a ratio port that steps from 1 to 8 at frame 128 of a 256-frame block leaves
    /// the first 128 frames unreduced (the grid point at 128 is the first to read the 8), and
    /// the last is reduced.
    ///
    /// Band 0 at -30 dB over a settled DC of 0.5, with a 1 ms attack: a block-end read would
    /// have the ratio at 8 from frame 0.
    #[test]
    fn a_driven_control_is_sampled_on_the_grid_not_at_the_blocks_end() {
        let (n, c) = node(2);
        c.band(0).set_threshold(-30.0);
        c.band(0).set_ratio(1.0);
        c.band(0).set_attack(0.001);
        c.band(0).set_release(0.001);
        let mut n = prepared(n);
        // 24000 frames: the block that follows begins on a grid point.
        render_chunked(&mut n, &[vec![0.5f32; SETTLE]], 512);
        let port = 2;
        let mut signal = [1.0f32; 256];
        signal[128..].fill(8.0);
        let mut params: Vec<Option<&[f32]>> = vec![None; port + 1];
        params[port] = Some(&signal);
        let y = drive(
            &mut n,
            RATE,
            Planes::new(&[&[0.5f32; 256]]).unwrap(),
            &params,
        )
        .remove(0);
        assert!(
            y[..128].iter().all(|s| (s - 0.5).abs() < 1e-6),
            "reduced before the step: {}",
            y[..128].iter().fold(0.5f32, |m, s| m.min(*s))
        );
        assert!(y[255] < 0.3, "not reduced after it: {}", y[255]);
    }

    /// A driven crossover port moves its crossover: port `bands * 6` is
    /// crossover 0, and a driven 2 kHz leaves a 2 kHz tone at half amplitude
    /// in band 0 (soloed).
    #[test]
    fn a_driven_crossover_port_moves_the_split() {
        let (n, c) = node(2);
        unity(&c);
        c.band(0).set_solo(true);
        let mut n = prepared(n);
        let port = 2 * PER_BAND;
        let x = sine(2_000.0, FS, SETTLE + WINDOW);
        let signal = [2_000.0f32; 512];
        let mut params: Vec<Option<&[f32]>> = vec![None; port + 1];
        let mut y = Vec::new();
        for chunk in x.chunks(512) {
            params[port] = Some(&signal[..chunk.len()]);
            y.extend_from_slice(&drive(&mut n, RATE, Planes::new(&[chunk]).unwrap(), &params)[0]);
        }
        let amp = amplitude_at(&y[SETTLE..], 2_000.0, FS);
        assert!((amp - 0.5).abs() < TOL, "{amp}");
    }

    /// A one-frame block and a full 1024-frame block both render.
    #[test]
    fn any_block_up_to_the_prepared_maximum_renders() {
        let (n, c) = node(4);
        c.set_crossover(0, Hz::new(200.0));
        let mut n = prepared(n);
        let x = sine(1_000.0, FS, 1_024);
        let big = drive(&mut n, RATE, Planes::new(&[&x]).unwrap(), &[]).remove(0);
        assert!(big.iter().all(|s| s.is_finite()) && big[1_023].abs() > 0.0);
        for s in &x[..8] {
            let one = drive(
                &mut n,
                RATE,
                Planes::new(&[std::slice::from_ref(s)]).unwrap(),
                &[],
            )
            .remove(0);
            assert!(one[0].is_finite());
        }
    }
    /// A cell write reaches the render at the next grid point of the stream, not at the next
    /// block: a makeup written when the stream stands 8 frames past a grid point leaves the
    /// 8 frames to the next one as they were, and from there the makeup ramps over its 240
    /// frames, frame `k` at `12 k / 240` dB.
    ///
    /// Unity bands over a settled DC of 0.5, all in band 0.
    #[test]
    fn a_cell_write_lands_at_the_next_grid_point() {
        let (n, c) = node(2);
        unity(&c);
        let mut n = prepared(n);
        // 48 blocks of 500 frames, then 8: the stream is 24 008 frames in, 8 past a grid
        // point (a multiple of 16).
        for _ in 0..48 {
            render_chunked(&mut n, &[vec![0.5f32; 500]], 500);
        }
        render_chunked(&mut n, &[vec![0.5f32; 8]], 8);
        c.band(0).set_makeup(12.0);
        let y = render_chunked(&mut n, &[vec![0.5f32; 24]], 24).remove(0);
        for (i, &v) in y[..8].iter().enumerate() {
            assert!(
                (v - 0.5).abs() < 1e-6,
                "frame {i} before the grid point: {v}"
            );
        }
        for k in 1..=16 {
            let want = 0.5 * libm::pow(10.0, 12.0 * k as f64 / RAMP as f64 / 20.0);
            let got = f64::from(y[7 + k]);
            assert!((got - want).abs() < 1e-5, "step {k}: {got} vs {want}");
        }
    }

    /// Any block size gives the same bits: cell writes at fixed stream positions (a makeup, a
    /// ratio, a bypass, a solo), a driven makeup, threshold and crossover ramping across the
    /// stream, on a stereo node of three bands, in blocks from 1 to 1 000 frames.
    #[test]
    fn any_block_size_gives_the_same_bits() {
        let len = 4_000;
        let x: Vec<Vec<f32>> = (0..2)
            .map(|c| {
                (0..len)
                    .map(|i| {
                        0.7 * libm::sin(0.021 * f64::from(c + 1) * i as f64) as f32
                            + 0.2 * libm::sin(0.9 * i as f64) as f32
                    })
                    .collect()
            })
            .collect();
        let ramp = |lo: f32, hi: f32| -> Vec<f32> {
            (0..len)
                .map(|i| lo + (hi - lo) * i as f32 / len as f32)
                .collect()
        };
        let makeup = ramp(0.0, 9.0);
        let threshold = ramp(-40.0, -10.0);
        let cutoff = ramp(300.0, 900.0);
        let render = |block: usize| {
            let n = MultibandCompressorNode::<f32>::from(MultibandCompressorConfig::new(
                BusTopology::STEREO,
                MultibandBands::of(3),
            ));
            let c = Ctl::new(&n);
            let mut n = prepared(n);
            let ports = 3 * PER_BAND + 2;
            let mut out = vec![Vec::new(), Vec::new()];
            // Cell writes at these stream frames, blocks cut there.
            let writes = [1_000usize, 2_500];
            let mut at = 0;
            while at < len {
                if at == writes[0] {
                    c.band(0).set_ratio(6.0);
                    c.band(1).set_bypass(true);
                } else if at == writes[1] {
                    c.band(2).set_solo(true);
                    c.band(0).set_makeup(-3.0);
                }
                let stop = writes.iter().copied().find(|&w| w > at).unwrap_or(len);
                let end = (at + block).min(stop);
                let mut params: Vec<Option<&[f32]>> = vec![None; ports];
                params[MAKEUP + PER_BAND] = Some(&makeup[at..end]);
                params[THRESHOLD] = Some(&threshold[at..end]);
                params[3 * PER_BAND] = Some(&cutoff[at..end]);
                let inputs = [&x[0][at..end], &x[1][at..end]];
                for (o, y) in
                    out.iter_mut()
                        .zip(drive(&mut n, RATE, Planes::new(&inputs).unwrap(), &params))
                {
                    o.extend(y);
                }
                at = end;
            }
            out
        };
        let whole = render(1_000);
        assert!(
            whole.iter().flatten().all(|s| s.is_finite()) && whole[0].iter().any(|&s| s != 0.0),
            "the stream is not silence"
        );
        for block in [1, 5, 7, 16, 17, 63, 64, 100, 333, 1_024] {
            let got = render(block);
            for (c, (g, w)) in got.iter().zip(&whole).enumerate() {
                assert!(
                    g.iter().zip(w).all(|(a, b)| a.to_bits() == b.to_bits()),
                    "blocks of {block} differ on channel {c}"
                );
            }
        }
    }

    /// A reset node renders as a fresh one, the meters at unity: after a loud run with moved
    /// cells, a reset and a changed cell, the render is the bits of a fresh node with that
    /// cell from the start.
    #[test]
    fn a_reset_node_renders_as_a_fresh_one() {
        let x = sine(700.0, FS, 6_000);
        let (n, c) = node(3);
        c.band(0).set_threshold(-40.0);
        c.band(0).set_ratio(10.0);
        c.band(1).set_bypass(true);
        let mut n = prepared(n);
        render_chunked(&mut n, &[vec![0.9f32; 5_000]], 512);
        assert!(
            n.gain_reduction_db(0) > Db::new(10.0),
            "the node was reducing"
        );
        c.band(0).set_threshold(-18.0);
        c.band(0).set_ratio(3.0);
        c.band(1).set_bypass(false);
        c.band(2).set_makeup(4.0);
        n.reset();
        assert_eq!(n.gain_reduction_db(0), Db::UNITY);
        let got = render_chunked(&mut n, std::slice::from_ref(&x), 512);

        let (f, fc) = node(3);
        fc.band(0).set_threshold(-18.0);
        fc.band(0).set_ratio(3.0);
        fc.band(2).set_makeup(4.0);
        let mut f = prepared(f);
        let want = render_chunked(&mut f, std::slice::from_ref(&x), 512);
        assert!(
            got[0]
                .iter()
                .zip(&want[0])
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "a reset node differs from a fresh one"
        );
    }
}
