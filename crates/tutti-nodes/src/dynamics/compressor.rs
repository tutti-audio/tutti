//! The feed-forward [`CompressorNode`] with an external side-chain, soft knee and
//! makeup gain.

use tutti_buffers::{BusTopology, InputOutput, MaxBlock};
use tutti_buffers::{Float, Planes};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, CompressorBank, CompressorControls, CompressorSignals,
    KeyFilter, KeySource, Lerp, SimdLanes,
};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_units::{CompressionRatio, Db, Hz, Q, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// A compressor: `channels = N` means N audio inputs, then (for a side-chain key) N key
/// inputs, and N outputs, with one **linked** gain computed from the loudest key channel and
/// applied to every output.
///
/// A [`KernelNode`] over [`CompressorKernel`], the detector and the gain being
/// [`CompressorBank`]'s; this file holds the cells and how the graph reaches them.
///
/// Made from a [`CompressorConfig`] (`CompressorNode::from`): any width up to one bus (16
/// channels), on a checked [`BusTopology`], with the [`KeySource`], the [`KeyFilter`] and the
/// [`CompressorControls`] it names. The gain is linked, so the stereo image does not shift
/// when one side is louder.
///
/// # The key
///
/// The detector listens to a [`KeySource`], chosen when the node is built. A config listens to
/// the compressor's own signal ([`KeySource::Own`]) unless told otherwise, as an ordinary
/// compressor does: no key bus, nothing to wire. A side-chain compressor is built by naming
/// its key, `CompressorConfig::key(KeySource::Sidechain)`, so a compressor added like any effect never
/// waits on one. Its key is a second bus after the audio inputs, **and it must be wired**:
/// the graph feeds an unconnected input silence, which compresses nothing, so a compressor
/// whose key nobody wired does nothing. Wire it with a bus edge, for example
/// `graph.connect(kick.bus_output(0), comp.bus_input(1))` in `tutti-session`, or each key channel
/// into the inputs after the audio.
///
/// The node declares no tail: the release decays after the key goes silent, but it only
/// scales, so a silent input is a silent output.
///
/// # The key filter
///
/// [`CompressorConfig::key_filter`] puts a [`KeyFilter`] on the key: the detector hears the
/// key through a high-pass or a band-pass, and the signal the gain is applied to is never
/// filtered. It is off by default. A node built with one has two more controls after the
/// makeup, the cutoff ([`UnitParam::Cutoff`](tutti_units::UnitParam::Cutoff), [`Hz`], 100 Hz) and
/// the Q ([`UnitParam::Q`](tutti_units::UnitParam::Q), [`Q`], Butterworth); a node with
/// [`KeyFilter::Off`] declares neither, as a reverb that cannot freeze declares no freeze.
/// The filter's coefficients ramp to a new cutoff over one grid period.
///
/// # Controls
///
/// Threshold, knee, ratio, attack, release and makeup (as [`UnitParam::GainDb`](tutti_units::UnitParam::GainDb)) are live
/// [`Params`](crate::Params) shared across clones, set through the node's
/// [`ParamSet`](tutti_graph::ParamSet) or the graph's `set_param` (a knee under 0 reads as
/// 0, a ratio under 1 as 1 and a time under 0 as 0: a compressor does not expand).
///
/// **The threshold is read per frame** when the graph drives it: a control signal on its port is the
/// bank's own ([`CompressorSignals`]), and the cell is the base a frame nobody drives (or a
/// non-finite one) is compared against. **Every other control is on the stream grid** (16
/// frames from the last reset, [`Kernel::GRID`]): a cell write or a driven port is sampled
/// at the grid's solve frame, so it reaches the render within 16 frames and the render does
/// not depend on how the stream is cut into blocks. The detector's controls land whole (the
/// follower smooths them), and the makeup is a ramp, a control moving to a target: it ramps
/// over [`GAIN_RAMP`](tutti_dsp::GAIN_RAMP), 5 ms.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, a fork of it starts from
/// the values last set through its [`ParamSet`](tutti_graph::ParamSet). The graph prepares
/// it at the device rate before its first block.
///
/// `F` is the float of the graph it runs in, and of the detector, the gain and the key filter.
pub type CompressorNode<F> = KernelNode<CompressorKernel<F>>;

/// What a [`CompressorNode`] is made from: its main bus, the key its detector listens to, the
/// filter on that key and the controls it starts at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorConfig {
    input: BusTopology,
    key: KeySource,
    filter: KeyFilter,
    controls: CompressorControls,
}

impl CompressorConfig {
    /// A compressor on the bus `input` that listens to its own signal ([`KeySource::Own`]),
    /// with no key filter and at [`CompressorControls::DEFAULT`], which does nothing (ratio 1,
    /// threshold 0 dB): a caller names what it wants with [`controls`](Self::controls).
    ///
    /// The main bus keeps its speaker placement. A width past one bus is refused by
    /// [`BusTopology::default_for`] before it reaches here: `new(BusTopology::default_for(n)?)`.
    #[must_use]
    pub fn new(input: BusTopology) -> Self {
        Self {
            input,
            key: KeySource::Own,
            filter: KeyFilter::Off,
            controls: CompressorControls::DEFAULT,
        }
    }

    /// The signal the detector listens to. A [`KeySource::Sidechain`] is a second bus as wide
    /// as the main one, which is a level to read, not a placement, so it has none.
    /// **A side-chain key must be wired** (see the [node](CompressorNode)).
    #[must_use]
    pub fn key(self, key: KeySource) -> Self {
        Self { key, ..self }
    }

    /// The filter the detector hears the key through, starting at the `key_cutoff` and
    /// `key_q` of the [`controls`](Self::controls) (100 Hz and Butterworth in the default's).
    /// The node gains the cutoff and Q controls after its others; [`KeyFilter::Off`], the
    /// default, has none, and the two fields of the controls are only what a later filter
    /// starts at.
    #[must_use]
    pub fn key_filter(self, filter: KeyFilter) -> Self {
        Self { filter, ..self }
    }

    /// The values the node's cells start at: the threshold (`Db`, typically negative, since 0 dB
    /// is full scale), the knee width, the ratio (`4.0` is 4 dB in for 1 dB out above the
    /// threshold; at least 1, no expansion), the envelope times (short attacks catch
    /// transients, long releases sound smoother) and the makeup gain (applied after the
    /// reduction, unconditionally), as [`CompressorControls`] says. The envelope's one-pole
    /// coefficients are derived from the times at the rate
    /// [`Node::prepare`](tutti_graph::Node::prepare) hands it.
    #[must_use]
    pub fn controls(self, controls: CompressorControls) -> Self {
        Self { controls, ..self }
    }
}

impl<F: Float + SimdLanes + Lerp> From<CompressorConfig> for CompressorNode<F> {
    fn from(config: CompressorConfig) -> Self {
        Self::with_kernel(
            CompressorKernel::new(config.input, config.key, config.filter),
            CompressorSet::new(
                config.filter,
                config.controls.key_cutoff,
                config.controls.key_q,
            ),
            config.controls,
        )
    }
}

/// The compressor's controls: threshold, knee, ratio, attack, release and makeup, then the
/// key filter's cutoff and Q if its [`KeyFilter`] is not [`Off`](KeyFilter::Off).
#[derive(Clone, Copy, Debug)]
pub struct CompressorSet {
    filter: KeyFilter,
    /// The cutoff the filter starts at: a part the ports do not carry until the node has a
    /// filter, so a node given a filter later starts there.
    cutoff: Hz,
    /// The Q the filter starts at, held as `cutoff` is.
    q: Q,
}

impl CompressorSet {
    fn new(filter: KeyFilter, cutoff: Hz, q: Q) -> Self {
        Self { filter, cutoff, q }
    }
}

control_set! {
    CompressorSet(s): CompressorControls = CompressorControls {
        key_cutoff: s.cutoff,
        key_q: s.q,
        ..CompressorControls::DEFAULT
    };
    live: (if s.filter == KeyFilter::Off { 6 } else { 8 });
    Threshold => Db = threshold { fast },
    Knee => Db = knee { whole },
    Ratio => CompressionRatio = ratio { whole },
    Attack => Seconds = attack { whole },
    Release => Seconds = release { whole },
    // The makeup ramps in the bank, over `GAIN_RAMP`: the reader must not ramp it
    // across the block as well. It is addressed as `GainDb`.
    GainDb => Db = makeup { whole },
    // The filter's coefficients ramp in the bank, over one grid period.
    Cutoff => Hz = key_cutoff { whole },
    Q => Q = key_q { whole };

    fn defaults(&self) -> CompressorControls {
        CompressorControls::DEFAULT
    }
}

/// The compressor as a [`Kernel`]: a [`CompressorBank`] sized at the prepared rate, its
/// detector on the [`KeySource`] it was built with.
///
/// A compressor rings on only as a gain (it scales its input), so it declares no tail and
/// never parks: its follower must keep tracking the key, which the main input's silence
/// says nothing of.
#[derive(Clone, Debug)]
pub struct CompressorKernel<F: Float + SimdLanes> {
    input: BusTopology,
    key: KeySource,
    filter: KeyFilter,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<CompressorBank<F>>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
}

impl<F: Float + SimdLanes + Lerp> CompressorKernel<F> {
    /// A compressor on `input` whose detector reads `key`.
    fn new(input: BusTopology, key: KeySource, filter: KeyFilter) -> Self {
        Self {
            input,
            key,
            filter,
            bank: None,
            applied: false,
        }
    }
}

impl<F: Float + SimdLanes + Lerp> Kernel for CompressorKernel<F> {
    type Float = F;
    type Set = CompressorSet;

    /// A cell that moves reaches the render within 16 frames, wherever the blocks fall.
    const GRID: usize = COEFF_INTERVAL;

    /// The input bus, then, for a [`KeySource::Sidechain`], the key bus as wide; one output
    /// bus where the input is.
    fn shape(&self) -> Shape {
        let output = Bus::banded(self.input, BusRole::Main, 1).expect("one band is a bus");
        let main = Buses::main(self.input);
        let inputs = match self.key {
            KeySource::Own => main,
            KeySource::Sidechain => main
                .with(Bus::new(self.input, BusRole::Sidechain))
                .expect("a main bus and a key are two buses"),
        };
        Shape::buses(inputs, Buses::NONE.with(output).expect("one bus")).cost(Cost::Moderate)
    }

    fn prepare(&mut self, rate: SampleRate, _: MaxBlock) {
        self.bank = Some(CompressorBank::new(rate).key_filter(self.filter, self.input.layout()));
        self.applied = false;
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &CompressorControls) {
        let Some(bank) = &mut self.bank else { return };
        bank.retarget(controls);
        if !self.applied {
            bank.jump_to_target();
            self.applied = true;
        }
    }

    fn render(
        &mut self,
        block: &mut InputOutput<'_, F>,
        signals: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let input = block.input();
        self.render_keyed(block, input, signals)
    }

    fn render_keyed(
        &mut self,
        block: &mut InputOutput<'_, F>,
        key: Planes<'_, F>,
        signals: ControlSignals<'_, F>,
    ) -> Result<(), BlockMismatch> {
        let signals = CompressorSignals {
            threshold: signals.get(CompressorSet::THRESHOLD_SIGNAL),
        };
        match &mut self.bank {
            Some(bank) => bank.process(signals, key, block),
            None => Ok(()),
        }
    }

    fn tail(&self) -> Tail {
        Tail::None
    }
}

impl CompressorSet {
    /// The control signal of the threshold: the one [`fast`](crate::ControlRead::Fast) row.
    const THRESHOLD_SIGNAL: usize = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::drive_kernel;
    use crate::test_support::{RATE_44K, prepared_at, tick, tick_fed};
    use tutti_graph::contract::{assert_param_fork, drive};
    use tutti_graph::{Node, ParamNode};
    use tutti_units::UnitParam;

    // The nodes under test are `f32` (the session's audio format), so their tolerances are
    // `f32`'s; each node has an `f64` kernel test below.

    /// The controls of a compressor the tests name four of, the others at the default's.
    fn four(threshold: f64, ratio: f64, attack: f64, release: f64) -> CompressorControls {
        CompressorControls {
            threshold: Db::new(threshold),
            ratio: CompressionRatio::new(ratio),
            attack: Seconds::new(attack),
            release: Seconds::new(release),
            ..CompressorControls::DEFAULT
        }
    }

    fn comp(threshold: f64, ratio: f64, attack: f64, release: f64) -> CompressorNode<f32> {
        CompressorNode::<f32>::from(
            CompressorConfig::new(BusTopology::MONO)
                .key(KeySource::Sidechain)
                .controls(four(threshold, ratio, attack, release)),
        )
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(CompressorNode::<f32>::from(
            CompressorConfig::new(BusTopology::STEREO)
                .key(KeySource::Sidechain)
                .controls(CompressorControls {
                    knee: Db::new(6.0),
                    makeup: Db::new(3.0),
                    ..four(-20.0, 4.0, 0.001, 0.1)
                }),
        ));
    }

    /// A block longer than 64 frames renders whole: the gain control signal is sized from the
    /// prepared `MaxBlock`, not a constant.
    #[test]
    fn a_block_up_to_the_prepared_maximum_renders() {
        let mut node = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let audio = vec![0.5f32; 1024];
        let sc = vec![0.9f32; 1024];
        let out = drive(
            &mut node,
            RATE_44K,
            Planes::new(&[&audio, &sc]).unwrap(),
            &[],
        )
        .remove(0);
        assert!(out[1023] < 0.5, "the loud sidechain reduced the tail");
    }

    /// A NaN ratio at construction is read as no compression, not as 0, which is below
    /// the type's floor: the output stays finite, `reset` is not needed to recover, and a
    /// later finite ratio takes effect.
    #[test]
    fn a_nan_ratio_at_construction_is_finite_and_recovers() {
        let mut node = prepared_at(comp(-20.0, f64::NAN, 0.0001, 0.1), RATE_44K);
        let audio = vec![0.5f32; 256];
        let sc = vec![0.9f32; 256];
        for _ in 0..16 {
            let out = drive(
                &mut node,
                RATE_44K,
                Planes::new(&[&audio, &sc]).unwrap(),
                &[],
            )
            .remove(0);
            assert!(out.iter().all(|s| s.is_finite()), "NaN ratio");
            assert!(
                out.iter().all(|&s| s == 0.5),
                "NaN ratio compresses nothing"
            );
        }
        assert!(node.param_set().set(UnitParam::Ratio, 4.0));
        let mut last = 0.5;
        for _ in 0..16 {
            let out = drive(
                &mut node,
                RATE_44K,
                Planes::new(&[&audio, &sc]).unwrap(),
                &[],
            )
            .remove(0);
            assert!(out.iter().all(|s| s.is_finite()), "after the write");
            last = out[255];
        }
        assert!(last < 0.5, "the finite ratio compresses: {last}");
        Node::reset(&mut node);
        let out = drive(
            &mut node,
            RATE_44K,
            Planes::new(&[&audio, &sc]).unwrap(),
            &[],
        )
        .remove(0);
        assert!(out.iter().all(|s| s.is_finite()), "after reset");
    }

    /// A compressor with its own key compresses its own signal: no key bus (one input a channel), and
    /// with nothing wired a loud signal is reduced and a quiet one under the threshold is not.
    #[test]
    fn a_compressor_with_its_own_key_compresses_its_own_signal() {
        let mk = || {
            CompressorNode::<f32>::from(
                CompressorConfig::new(BusTopology::MONO).controls(four(-20.0, 4.0, 0.0001, 0.1)),
            )
        };
        let shape = mk().shape();
        assert_eq!(
            (shape.audio_in().count(), shape.audio_out().count()),
            (1, 1)
        );
        let mut output = [0.0f32];
        let mut loud = prepared_at(mk(), RATE_44K);
        for _ in 0..1000 {
            tick(&mut loud, &[0.5], &mut output);
        }
        assert!(output[0] < 0.4, "a loud signal ducks itself: {}", output[0]);
        let mut quiet = prepared_at(mk(), RATE_44K);
        for _ in 0..1000 {
            tick(&mut quiet, &[0.05], &mut output);
        }
        assert_eq!(output[0], 0.05, "under the threshold nothing is reduced");
    }

    #[test]
    fn test_compressor_mono_reduces_gain_on_loud_sidechain() {
        let mut node = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let mut output = [0.0f32];
        for _ in 0..1000 {
            tick(&mut node, &[0.5, 0.9], &mut output);
        }
        assert!(output[0] < 0.5);
    }

    #[test]
    fn test_compressor_mono_no_reduction_below_threshold() {
        let mut node = prepared_at(comp(-10.0, 4.0, 0.001, 0.1), RATE_44K);
        let mut output = [0.0f32];
        for _ in 0..1000 {
            tick(&mut node, &[0.5, 0.1], &mut output);
        }
        // A key at -20 dB under a -10 dB threshold: the gain is 1.
        assert_eq!(output[0], 0.5);
    }

    #[test]
    fn test_compressor_soft_knee_differs_from_hard_knee() {
        let mut hard = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let mut soft = prepared_at(
            CompressorNode::<f32>::from(
                CompressorConfig::new(BusTopology::MONO)
                    .key(KeySource::Sidechain)
                    .controls(CompressorControls {
                        knee: Db::new(12.0),
                        ..four(-20.0, 4.0, 0.0001, 0.1)
                    }),
            ),
            RATE_44K,
        );
        let mut hard_out = [0.0f32];
        let mut soft_out = [0.0f32];
        for _ in 0..1000 {
            tick(&mut hard, &[0.5, 0.15], &mut hard_out);
            tick(&mut soft, &[0.5, 0.15], &mut soft_out);
        }
        assert!(
            (hard_out[0] - soft_out[0]).abs() > 0.001,
            "Soft knee should produce different output near threshold: hard={}, soft={}",
            hard_out[0],
            soft_out[0]
        );
    }

    #[test]
    fn test_compressor_makeup_gain() {
        let mut with = prepared_at(
            CompressorNode::<f32>::from(
                CompressorConfig::new(BusTopology::MONO)
                    .key(KeySource::Sidechain)
                    .controls(CompressorControls {
                        makeup: Db::new(6.0),
                        ..four(-20.0, 4.0, 0.0001, 0.1)
                    }),
            ),
            RATE_44K,
        );
        let mut without = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let mut output = [0.0f32];
        let mut output_no_makeup = [0.0f32];
        for _ in 0..1000 {
            tick(&mut with, &[0.5, 0.9], &mut output);
            tick(&mut without, &[0.5, 0.9], &mut output_no_makeup);
        }
        // Six dB is a factor of 10^(6/20), the same gain on the same reduction.
        let ratio = f64::from(output[0]) / f64::from(output_no_makeup[0]);
        assert!((ratio - 10f64.powf(6.0 / 20.0)).abs() < 1e-4, "{ratio}");
    }

    /// `reset` clears the follower: a quiet key after it reads no reduction, where without
    /// it the slow release (100 ms) would still hold the last one.
    #[test]
    fn a_reset_clears_the_follower() {
        let mut node = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let mut output = [0.0f32];
        for _ in 0..1000 {
            tick(&mut node, &[0.5, 0.9], &mut output);
        }
        tick(&mut node, &[0.5, 0.0], &mut output);
        assert!(output[0] < 0.5, "the release still holds the reduction");

        Node::reset(&mut node);
        tick(&mut node, &[0.5, 0.0], &mut output);
        assert_eq!(output[0], 0.5, "a reset follower is at no reduction");
    }

    /// A ratio written below 1 reads as 1: a compressor does not expand, and at ratio 1
    /// nothing is reduced.
    #[test]
    fn a_ratio_below_one_reads_as_one() {
        let mut node = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        assert!(node.param_set().set(UnitParam::Ratio, 0.5));
        let mut output = [0.0f32];
        for _ in 0..100 {
            tick(&mut node, &[0.5, 0.9], &mut output);
        }
        assert_eq!(output[0], 0.5);
    }

    #[test]
    fn test_compressor_stereo_reduces_on_loud_sidechain() {
        let mut node = prepared_at(
            CompressorNode::<f32>::from(
                CompressorConfig::new(BusTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(four(-20.0, 4.0, 0.0001, 0.1)),
            ),
            RATE_44K,
        );
        let mut output = [0.0f32; 2];
        for _ in 0..1000 {
            tick(&mut node, &[0.5, 0.5, 0.9, 0.9], &mut output);
        }
        assert!((output[0] - output[1]).abs() < 0.001);
        assert!(output[0] < 0.5);
    }

    /// `2 * width` inputs (audio, then key) and `width` outputs for a side-chain compressor
    /// and `width` inputs for an own-key one, at every width: a config listens to its
    /// own signal at its width unless it names a key.
    #[test]
    fn the_width_and_the_key_choice_are_the_port_layout() {
        let c = four(-20.0, 4.0, 0.001, 0.1);
        let wide = BusTopology::default_for(tutti_buffers::ChannelLayout::from(4u16))
            .expect("at most one bus wide");
        for (node, inputs, outputs) in [
            (
                CompressorNode::<f32>::from(
                    CompressorConfig::new(BusTopology::MONO)
                        .key(KeySource::Sidechain)
                        .controls(c),
                ),
                2,
                1,
            ),
            (
                CompressorNode::<f32>::from(
                    CompressorConfig::new(BusTopology::STEREO)
                        .key(KeySource::Sidechain)
                        .controls(c),
                ),
                4,
                2,
            ),
            (
                CompressorNode::<f32>::from(
                    CompressorConfig::new(wide)
                        .key(KeySource::Sidechain)
                        .controls(c),
                ),
                8,
                4,
            ),
            (
                CompressorNode::<f32>::from(CompressorConfig::new(BusTopology::MONO).controls(c)),
                1,
                1,
            ),
            (
                CompressorNode::<f32>::from(CompressorConfig::new(BusTopology::STEREO).controls(c)),
                2,
                2,
            ),
            (
                CompressorNode::<f32>::from(CompressorConfig::new(wide).controls(c)),
                4,
                4,
            ),
            (
                CompressorNode::<f32>::from(CompressorConfig::new(BusTopology::MONO).controls(c)),
                1,
                1,
            ),
            (
                CompressorNode::<f32>::from(CompressorConfig::new(BusTopology::STEREO).controls(c)),
                2,
                2,
            ),
        ] {
            let shape = node.shape();
            assert_eq!(
                (shape.audio_in().count(), shape.audio_out().count()),
                (inputs, outputs)
            );
        }
    }

    /// The controls given to a config are the cells' values, all six of them.
    #[test]
    fn the_config_controls_reach_the_cells() {
        let c = CompressorControls {
            threshold: Db::new(-18.0),
            knee: Db::new(6.0),
            ratio: CompressionRatio::new(3.0),
            attack: Seconds::new(0.002),
            release: Seconds::new(0.25),
            makeup: Db::new(2.5),
            ..CompressorControls::DEFAULT
        };
        for node in [
            CompressorNode::<f32>::from(
                CompressorConfig::new(BusTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(c),
            ),
            CompressorNode::<f32>::from(CompressorConfig::new(BusTopology::MONO).controls(c)),
        ] {
            let bases: Vec<_> = (0..6).map(|k| node.param_base(k)).collect();
            assert_eq!(
                bases,
                [-18.0, 6.0, 3.0, 0.002, 0.25, 2.5].map(Some),
                "threshold, knee, ratio, attack, release, makeup"
            );
        }
    }

    /// The shape declares every control in the order of the set (threshold, knee, ratio,
    /// attack, release, makeup as `GainDb`), with the cells as their bases, no latency and
    /// no tail, at every width.
    #[test]
    fn compressor_declares_its_params() {
        let m = prepared_at(comp(-20.0, 4.0, 0.0001, 0.1), RATE_44K);
        let shape = m.shape();
        assert_eq!(
            shape.params.as_slice(),
            &[
                UnitParam::Threshold,
                UnitParam::Knee,
                UnitParam::Ratio,
                UnitParam::Attack,
                UnitParam::Release,
                UnitParam::GainDb
            ][..]
        );
        assert_eq!(shape.audio_in().count(), 2);
        assert_eq!(shape.latency, tutti_units::Latency::ZERO);
        assert_eq!(shape.tail, tutti_units::Tail::None);
        for (k, base) in [-20.0, 0.0, 4.0, 0.0001, 0.1, 0.0].into_iter().enumerate() {
            assert_eq!(m.param_base(k), Some(base), "control {k}'s base");
        }
    }

    /// A fed threshold is read per frame: a frame fed a lower threshold than the cell's is
    /// reduced more than one at the cell's, and the cell is the base a frame nobody
    /// drives sits at.
    #[test]
    fn a_fed_threshold_is_the_one_read_per_frame() {
        let mut held = prepared_at(comp(-10.0, 4.0, 0.0, 0.0), RATE_44K);
        let mut fed = prepared_at(comp(-10.0, 4.0, 0.0, 0.0), RATE_44K);
        let (mut a, mut b) = ([0.0f32], [0.0f32]);
        tick(&mut held, &[0.5, 0.5], &mut a);
        tick_fed(&mut fed, &[0.5, 0.5], &[Some(-40.0)], &mut b);
        assert!(
            b[0] < a[0],
            "a lower threshold reduces more: {} vs {}",
            b[0],
            a[0]
        );
        // A key at -6 dB, ratio 4, threshold -40: 25.5 dB of reduction, instant follower.
        let want = 0.5 * 10f32.powf(-0.75 * (-6.0206 + 40.0) / 20.0);
        assert!((b[0] - want).abs() < 1e-4, "{} vs {want}", b[0]);
    }

    /// A threshold that moves inside a block is read at the frame it moves on, not at the
    /// stream grid's next solve (16 frames on): frame 5 of a block whose control signal steps from
    /// 20 dB to -40 dB there is reduced, the frame before it is not.
    #[test]
    fn a_threshold_that_steps_inside_a_block_acts_on_that_frame() {
        let mut node = prepared_at(comp(-10.0, 4.0, 0.0, 0.0), RATE_44K);
        let n = 64;
        let (audio, key) = (vec![0.5f32; n], vec![0.5f32; n]);
        let signal: Vec<f32> = (0..n).map(|i| if i < 5 { 20.0 } else { -40.0 }).collect();
        let out = drive(
            &mut node,
            RATE_44K,
            Planes::new(&[&audio, &key]).unwrap(),
            &[Some(&signal)],
        )
        .remove(0);
        assert_eq!(out[4], 0.5, "over the 20 dB threshold: no reduction");
        assert!(
            out[5] < 0.2,
            "the -40 dB threshold reduces at once: {}",
            out[5]
        );
    }

    /// A control other than the threshold is sampled on the stream's 16-frame grid, so a
    /// write reaches the render within 16 frames wherever the blocks fall, and (the
    /// follower being instant here) lands whole.
    #[test]
    fn a_cell_write_reaches_the_render_within_a_grid_step() {
        let mut node = prepared_at(comp(-20.0, 2.0, 0.0, 0.0), RATE_44K);
        let mut out = [0.0f32];
        for _ in 0..40 {
            tick(&mut node, &[0.5, 0.5], &mut out);
        }
        let before = out[0];
        assert!(node.param_set().set(UnitParam::Ratio, 8.0));
        for _ in 0..16 {
            tick(&mut node, &[0.5, 0.5], &mut out);
        }
        assert!(
            out[0] < before,
            "ratio 8 reduces more than 2: {} vs {before}",
            out[0]
        );
        // A key at 0.5 (-6.02 dB) over -20 dB at 8:1: 7/8 of the 13.98 dB over it.
        let want = 0.5 * 10f32.powf(-0.875 * (20.0 * 0.5f32.log10() + 20.0) / 20.0);
        assert!((out[0] - want).abs() < 1e-4, "{} vs {want}", out[0]);
    }

    /// A makeup written between blocks ramps over 5 ms (240 frames at 48 kHz) wherever the
    /// blocks fall: monotone, starting near the old gain and landing on the new one, which
    /// is the old times 10^(6/20).
    #[test]
    fn a_makeup_write_ramps_over_five_milliseconds() {
        let rate = SampleRate::SR_48K;
        let mut node = tutti_graph::contract::prepared(comp(0.0, 1.0, 0.0, 0.0), rate, 64);
        let (audio, key) = (vec![0.5f32; 64], vec![0.0f32; 64]);
        for _ in 0..4 {
            drive(&mut node, rate, Planes::new(&[&audio, &key]).unwrap(), &[]);
        }
        assert!(node.param_set().set(UnitParam::GainDb, 6.0));
        let mut out = Vec::new();
        for _ in 0..8 {
            out.extend(
                drive(&mut node, rate, Planes::new(&[&audio, &key]).unwrap(), &[]).remove(0),
            );
        }
        // The write is sampled on the grid (within 16 frames), then 240 frames of ramp.
        assert!(
            out[0] < 0.5 * 1.01,
            "the first frame is near the old gain: {}",
            out[0]
        );
        for w in out.windows(2) {
            assert!(w[1] >= w[0], "the ramp is monotone: {w:?}");
            // 6 dB over 240 frames is 0.025 dB a frame, a factor of 1.0029.
            assert!(w[1] / w[0] < 1.0035, "no step in the ramp: {w:?}");
        }
        let want = 0.5 * 10f32.powf(6.0 / 20.0);
        assert!(
            (out[out.len() - 1] - want).abs() < 1e-5,
            "{}",
            out[out.len() - 1]
        );
        assert!(
            out[16 + 240 + 16] > want * 0.9999,
            "arrived by 5 ms after the grid point"
        );
    }

    /// An `f64` kernel keeps its detector and gain in `f64`: a constant key `L` dB over the
    /// threshold `T` of more than 24 bits comes out at `T + (L - T) / R` dB with instant
    /// times, to 1e-12 dB (an `f32` state is off by 1e-7).
    #[test]
    fn an_f64_kernel_lands_a_wide_key_on_the_static_curve_in_f64() {
        use tutti_fixtures::signal::wide_signal;
        let (threshold, ratio) = (-30.0f64, 5.0f64);
        let key = vec![wide_signal(1)[0]; 128];
        let mut k = CompressorKernel::<f64>::new(BusTopology::MONO, KeySource::Own, KeyFilter::Off);
        k.prepare(
            SampleRate::SR_48K,
            MaxBlock::of(tutti_units::Samples::new(128)),
        );
        let y = drive_kernel(
            &mut k,
            &[(0, four(threshold, ratio, 0.0, 0.0))],
            std::slice::from_ref(&key),
        );
        let level = 20.0 * key[0].log10();
        let want = threshold + (level - threshold) / ratio;
        let got = 20.0 * y[0][127].log10();
        assert!((got - want).abs() < 1e-12, "{got} dB against {want} dB");
    }
}
