//! The noise gate [`GateNode`]: closes below a threshold, keyed from a side-chain or from
//! its own signal.

use tutti_buffers::{BusTopology, InputOutput, MaxBlock};
use tutti_buffers::{Float, Planes};
use tutti_dsp::{
    BlockMismatch, COEFF_INTERVAL, GateBank, GateControls, GateSignals, KeyFilter, KeySource, Lerp,
    SimdLanes,
};
use tutti_graph::{Bus, BusRole, Buses, Cost, Shape};
use tutti_units::{CompressionRatio, Db, Hz, Q, SampleRate, Seconds, Tail};

use crate::kernel::{ControlSignals, Kernel, KernelNode};

/// A noise gate: `channels = N` means N audio inputs, then (for a side-chain key) N key
/// inputs, and N outputs, with one **linked** open/closed decision from the loudest key
/// channel applied to every output.
///
/// A [`KernelNode`] over [`GateKernel`], made from a [`GateConfig`] (`GateNode::from`), the detector and the gain being [`GateBank`]'s;
/// this file holds the cells and how the graph reaches them.
///
/// # The key
///
/// The detector listens to a [`KeySource`], chosen when the node is built:
///
/// - A [`GateConfig`] listens to its own signal ([`KeySource::Own`]) unless told otherwise:
///   no key bus, as an ordinary gate does, and nothing wired.
/// - [`GateConfig::key`] with [`KeySource::Sidechain`] has a **side-chain** key: a second
///   bus, as wide as the signal, after the audio inputs. A keyed gate is only built by
///   naming its key, so a gate added like any effect never waits on one. **The key must
///   be wired**: the graph feeds an unconnected input silence, and a gate keyed by silence
///   stays closed (at its range). Wire it with a bus edge, for example
///   `graph.connect(kick.bus_output(0), gate.bus_input(1))` in `tutti-session`, or each key
///   channel into the inputs after the audio.
///
/// A finite [`ratio`](GateControls::ratio) (`UnitParam::Ratio`, after the range) makes the gate a
/// downward expander ([`GateBank`]): the gain under the threshold falls `ratio` dB per dB down
/// to the range. The ratio is a live control, read on the grid.
///
/// The node declares no tail: the release decays after the key goes silent, but it only
/// scales, so a silent input is a silent output.
///
/// # The key filter
///
/// [`GateConfig::key_filter`] puts a [`KeyFilter`] on the key: the detector hears the
/// key through a high-pass or a band-pass, and the signal the gate is applied to is never
/// filtered. It is off by default. A node built with one has two more controls after the
/// range, the cutoff ([`UnitParam::Cutoff`](tutti_units::UnitParam::Cutoff), [`Hz`], 100 Hz) and
/// the Q ([`UnitParam::Q`](tutti_units::UnitParam::Q), [`Q`], Butterworth); a node with
/// [`KeyFilter::Off`] declares neither, as a reverb that cannot freeze declares no freeze.
/// The filter's coefficients ramp to a new cutoff over one grid period.
///
/// # Controls
///
/// Threshold, attack, hold, release, range and ratio are live [`Params`](crate::Params) shared
/// across clones, set through the node's [`ParamSet`](tutti_graph::ParamSet) or the
/// graph's `set_param` (a time under 0 reads as 0 and a range over 0 dB as 0 dB).
///
/// **The threshold is read per frame** when the graph drives it: a control signal on its port is the
/// bank's own ([`GateSignals`]), and the cell is the base a frame nobody drives (or a
/// non-finite one) is compared against. **Every other control is on the stream grid** (16
/// frames from the last reset, [`Kernel::GRID`]): a cell write or a driven port is sampled
/// at the grid's solve frame, so it reaches the render within 16 frames and the render does
/// not depend on how the stream is cut into blocks. The times land whole (the follower
/// smooths them), and the range is a ramp, a control moving to a target: it ramps over
/// [`GAIN_RAMP`](tutti_dsp::GAIN_RAMP), 5 ms.
///
/// # In a graph
///
/// A graph node ([`IntoNode`](tutti_graph::IntoNode)): inserted, a fork of it starts from
/// the values last set through its [`ParamSet`](tutti_graph::ParamSet). The graph prepares
/// it at the device rate before its first block.
///
/// `F` is the float of the graph it runs in, and of the detector, the gain and the key filter.
pub type GateNode<F> = KernelNode<GateKernel<F>>;

/// What a [`GateNode`] is made from: its main bus, the key its detector listens to, the
/// filter on that key and the controls it starts at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateConfig {
    input: BusTopology,
    key: KeySource,
    filter: KeyFilter,
    controls: GateControls,
}

impl GateConfig {
    /// A gate on the bus `input` that listens to its own signal ([`KeySource::Own`]), with no
    /// key filter and at [`GateControls::DEFAULT`].
    ///
    /// The main bus keeps its speaker placement. A width past one bus is refused by
    /// [`BusTopology::default_for`] before it reaches here: `new(BusTopology::default_for(n)?)`.
    #[must_use]
    pub fn new(input: BusTopology) -> Self {
        Self {
            input,
            key: KeySource::Own,
            filter: KeyFilter::Off,
            controls: GateControls::DEFAULT,
        }
    }

    /// The signal the detector listens to. A [`KeySource::Sidechain`] is a second bus as wide
    /// as the main one, which is a level to read, not a placement, so it has none.
    /// **A side-chain key must be wired** (see the [node](GateNode)).
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

    /// The values the node's cells start at, as [`GateControls`] says: the threshold (the key
    /// level in [`Db`] at or above which the gate opens), the times (`attack` how fast it
    /// opens, `hold` how long it stays open after the key falls back below the threshold,
    /// which is what stops a gate chattering on a key hovering at it, and `release` how fast
    /// it then closes) and the `range`, how far a closed gate attenuates.
    ///
    /// **The range is not defaulted for you.** [`GateControls::DEFAULT`] has a range of
    /// 0 dB, a gate that passes everything (what a non-finite control falls back to). A
    /// caller that wants a gate writes the range:
    /// `GateControls { threshold, range: Db::new(-80.0), ..GateControls::DEFAULT }`, which is
    /// effectively silent when closed. A gentler `-12.0` ducks the signal without removing
    /// it, which sounds more natural on drums and room mics.
    ///
    /// The attack and release coefficients and the hold count are derived from the times at
    /// the rate [`Node::prepare`](tutti_graph::Node::prepare) hands it.
    #[must_use]
    pub fn controls(self, controls: GateControls) -> Self {
        Self { controls, ..self }
    }
}

impl<F: Float + SimdLanes + Lerp> From<GateConfig> for GateNode<F> {
    fn from(config: GateConfig) -> Self {
        Self::with_kernel(
            GateKernel::new(config.input, config.key, config.filter),
            GateSet::new(
                config.filter,
                config.controls.key_cutoff,
                config.controls.key_q,
            ),
            config.controls,
        )
    }
}

/// The gate's controls: threshold, attack, hold, release and range, then the key filter's
/// cutoff and Q if its [`KeyFilter`] is not [`Off`](KeyFilter::Off).
#[derive(Clone, Copy, Debug)]
pub struct GateSet {
    filter: KeyFilter,
    /// The cutoff the filter starts at: a part the ports do not carry until the node has a
    /// filter, so a node given a filter later starts there.
    cutoff: Hz,
    /// The Q the filter starts at, held as `cutoff` is.
    q: Q,
}

impl GateSet {
    fn new(filter: KeyFilter, cutoff: Hz, q: Q) -> Self {
        Self { filter, cutoff, q }
    }
}

control_set! {
    GateSet(s): GateControls = GateControls {
        key_cutoff: s.cutoff,
        key_q: s.q,
        ..GateControls::DEFAULT
    };
    live: (if s.filter == KeyFilter::Off { 6 } else { 8 });
    Threshold => Db = threshold { fast },
    Attack => Seconds = attack { whole },
    Hold => Seconds = hold { whole },
    Release => Seconds = release { whole },
    // The range ramps in the bank, over `GAIN_RAMP`: the reader must not ramp it
    // across the block as well.
    Range => Db = range { whole },
    // A ratio of `+inf` is a gate, so it is valid at infinity.
    Ratio => CompressionRatio = ratio { whole, not_nan },
    // The filter's coefficients ramp in the bank, over one grid period.
    Cutoff => Hz = key_cutoff { whole },
    Q => Q = key_q { whole };

    fn defaults(&self) -> GateControls {
        GateControls::DEFAULT
    }
}

/// The gate as a [`Kernel`]: a [`GateBank`] sized at the prepared rate, its detector on the
/// [`KeySource`] it was built with.
///
/// A gate rings on only as a gain (it scales its input), so it declares no tail and never
/// parks: its follower must keep tracking the key, which the main input's silence says
/// nothing of.
#[derive(Clone, Debug)]
pub struct GateKernel<F: Float + SimdLanes> {
    input: BusTopology,
    key: KeySource,
    filter: KeyFilter,
    /// `None` until [`prepare`](Kernel::prepare).
    bank: Option<GateBank<F>>,
    /// Whether the first retarget after `prepare` has been applied.
    applied: bool,
}

impl<F: Float + SimdLanes + Lerp> GateKernel<F> {
    /// A gate on `input` whose detector reads `key`.
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

impl<F: Float + SimdLanes + Lerp> Kernel for GateKernel<F> {
    type Float = F;
    type Set = GateSet;

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
        self.bank = Some(GateBank::new(rate).key_filter(self.filter, self.input.width()));
        self.applied = false;
    }

    fn reset(&mut self) {
        if let Some(bank) = &mut self.bank {
            bank.reset();
        }
    }

    fn retarget(&mut self, controls: &GateControls) {
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
        let signals = GateSignals {
            threshold: signals.get(GateSet::THRESHOLD_SIGNAL),
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

impl GateSet {
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

    /// The controls of a gate the tests name four of: the closed floor is -80 dB (the
    /// node's range before it was a control), not `GateControls::DEFAULT`'s 0 dB.
    fn four(threshold: f64, attack: f64, hold: f64, release: f64) -> GateControls {
        GateControls {
            threshold: Db::new(threshold),
            attack: Seconds::new(attack),
            hold: Seconds::new(hold),
            release: Seconds::new(release),
            range: Db::new(-80.0),
            ..GateControls::DEFAULT
        }
    }

    fn with_range(c: GateControls, range: f64) -> GateControls {
        GateControls {
            range: Db::new(range),
            ..c
        }
    }

    fn gate(threshold: f64, attack: f64, hold: f64, release: f64) -> GateNode<f32> {
        GateNode::<f32>::from(
            GateConfig::new(BusTopology::MONO)
                .key(KeySource::Sidechain)
                .controls(four(threshold, attack, hold, release)),
        )
    }

    /// A fork starts from the values last set through the node's `ParamSet` and shares no
    /// cell with it.
    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(GateNode::<f32>::from(
            GateConfig::new(BusTopology::STEREO)
                .key(KeySource::Sidechain)
                .controls(with_range(four(-30.0, 0.001, 0.01, 0.1), -20.0)),
        ));
    }

    /// A block longer than 64 frames renders whole: the gain control signal is sized from the
    /// prepared `MaxBlock`, not a constant.
    #[test]
    fn a_block_up_to_the_prepared_maximum_renders() {
        let mut node = prepared_at(gate(-20.0, 0.0001, 0.01, 0.1), RATE_44K);
        let audio = vec![0.5f32; 1024];
        let sc = vec![0.9f32; 1024];
        let out = drive(
            &mut node,
            RATE_44K,
            Planes::new(&[&audio, &sc]).unwrap(),
            &[],
        )
        .remove(0);
        assert!(out[1023] > 0.3, "the loud sidechain opened the gate");
    }

    /// A new gate is closed: the first frames are the signal at the range, -80 dB.
    #[test]
    fn a_new_gate_is_closed() {
        let mut node = prepared_at(gate(-30.0, 0.001, 0.01, 0.1), RATE_44K);
        let mut out = [0.0f32];
        tick(&mut node, &[0.5, 0.0], &mut out);
        assert!((out[0] / (0.5 * 1.0e-4) - 1.0).abs() < 1e-4, "{}", out[0]);
    }

    #[test]
    fn a_loud_side_chain_opens_the_gate() {
        let mut node = prepared_at(gate(-20.0, 0.0001, 0.01, 0.1), RATE_44K);
        let mut out = [0.0f32];
        for _ in 0..500 {
            tick(&mut node, &[0.5, 0.9], &mut out);
        }
        assert!((out[0] - 0.5).abs() < 1e-3, "open: {}", out[0]);
    }

    #[test]
    fn a_quiet_side_chain_closes_the_gate() {
        let mut node = prepared_at(
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::MONO)
                    .key(KeySource::Sidechain)
                    .controls(with_range(four(-20.0, 0.001, 0.001, 0.001), -60.0)),
            ),
            RATE_44K,
        );
        let mut out = [0.0f32];
        for _ in 0..500 {
            tick(&mut node, &[0.5, 0.9], &mut out);
        }
        assert!(out[0] > 0.45);
        for _ in 0..2000 {
            tick(&mut node, &[0.5, 0.01], &mut out);
        }
        assert!(out[0] < 0.01, "closed to -60 dB: {}", out[0]);
    }

    /// A range written over 0 dB reads as 0 dB: a closed gate that gains nothing and
    /// loses nothing.
    #[test]
    fn a_range_over_zero_reads_as_zero() {
        let mut node = prepared_at(
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::MONO)
                    .key(KeySource::Sidechain)
                    .controls(with_range(four(-20.0, 0.001, 0.01, 0.1), 10.0)),
            ),
            RATE_44K,
        );
        let mut out = [0.0f32];
        for _ in 0..100 {
            tick(&mut node, &[0.5, 0.0], &mut out);
        }
        assert_eq!(out[0], 0.5);
        assert!(node.param_set().set(UnitParam::Range, 6.0));
        for _ in 0..100 {
            tick(&mut node, &[0.5, 0.0], &mut out);
        }
        assert_eq!(out[0], 0.5);
    }

    /// With a -12 dB range a closed gate attenuates the signal by that figure and does not
    /// mute it.
    #[test]
    fn the_range_attenuates_rather_than_mutes() {
        let mut node = prepared_at(
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::MONO)
                    .key(KeySource::Sidechain)
                    .controls(with_range(four(-20.0, 0.001, 0.001, 0.001), -12.0)),
            ),
            RATE_44K,
        );
        let mut out = [0.0f32];
        for _ in 0..2000 {
            tick(&mut node, &[0.5, 0.01], &mut out);
        }
        let want = 0.5 * 10f32.powf(-12.0 / 20.0);
        assert!((out[0] - want).abs() < 1e-5, "{} vs {want}", out[0]);
    }

    /// A ratio written through the param set moves the settled gain to the closed form: a key
    /// 12 dB under the -20 dB threshold is at the range (-60 dB) at the default infinite ratio,
    /// and settles at `-(r - 1) 12` dB after a write of `r = 2` and of `r = 4` (-36 dB), to
    /// `f32`'s `1e-5`.
    #[test]
    fn a_moved_ratio_moves_the_settled_gain_to_the_closed_form() {
        let mut node = prepared_at(
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::MONO)
                    .key(KeySource::Sidechain)
                    .controls(with_range(four(-20.0, 0.0, 0.0, 0.0), -60.0)),
            ),
            RATE_44K,
        );
        let mut out = [0.0f32];
        let key = 10f32.powf(-32.0 / 20.0);
        for (ratio, want_db) in [(f64::INFINITY, -60.0f32), (2.0, -12.0), (4.0, -36.0)] {
            assert!(node.param_set().set(UnitParam::Ratio, ratio));
            for _ in 0..2000 {
                tick(&mut node, &[0.5, key], &mut out);
            }
            let want = 0.5 * 10f32.powf(want_db / 20.0);
            assert!(
                (out[0] / want - 1.0).abs() < 1e-4,
                "ratio {ratio}: {} vs {want}",
                out[0]
            );
        }
    }

    /// A ratio of 2 in the controls the node is built with makes it a downward expander: a
    /// side-chain key 12 dB under the -20 dB threshold settles at `-(2 - 1) 12 = -12` dB, the
    /// range (-60 dB) not reached, to `f32`'s `1e-5`.
    #[test]
    fn a_built_ratio_makes_the_node_an_expander() {
        let mut node = prepared_at(
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::MONO)
                    .key(KeySource::Sidechain)
                    .controls(GateControls {
                        ratio: CompressionRatio::new(2.0),
                        ..with_range(four(-20.0, 0.0, 0.0, 0.0), -60.0)
                    }),
            ),
            RATE_44K,
        );
        let mut out = [0.0f32];
        let key = 10f32.powf(-32.0 / 20.0);
        for _ in 0..2000 {
            tick(&mut node, &[0.5, key], &mut out);
        }
        let want = 0.5 * 10f32.powf(-12.0 / 20.0);
        assert!((out[0] - want).abs() < 1e-5, "{} vs {want}", out[0]);
    }

    /// `reset` clears the follower and the hold: after it a gate that was open and holding
    /// is closed.
    #[test]
    fn a_reset_closes_the_gate() {
        let mut node = prepared_at(gate(-20.0, 0.0001, 0.5, 0.1), RATE_44K);
        let mut out = [0.0f32];
        for _ in 0..500 {
            tick(&mut node, &[0.5, 0.9], &mut out);
        }
        tick(&mut node, &[0.5, 0.0], &mut out);
        assert!(out[0] > 0.49, "the hold keeps it open");
        Node::reset(&mut node);
        tick(&mut node, &[0.5, 0.0], &mut out);
        assert!(out[0] < 1e-3, "a reset gate is closed: {}", out[0]);
    }

    #[test]
    fn a_stereo_gate_is_linked() {
        let mut node = prepared_at(
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(four(-20.0, 0.0001, 0.01, 0.1)),
            ),
            RATE_44K,
        );
        let mut out = [0.0f32; 2];
        for _ in 0..500 {
            tick(&mut node, &[0.8, 0.3, 0.9, 0.001], &mut out);
        }
        // Both channels open on the loudest key; the signal ratio is kept.
        assert!(
            (out[0] - 0.8).abs() < 1e-3 && (out[1] - 0.3).abs() < 1e-3,
            "{out:?}"
        );
    }

    /// `2 * width` inputs (audio, then key) and `width` outputs for a side-chain gate, and
    /// `width` inputs for an own-key one, at every width: a config listens to its
    /// own signal at its width unless it names a key.
    #[test]
    fn the_width_and_the_key_choice_are_the_port_layout() {
        let c = four(-20.0, 0.001, 0.01, 0.1);
        let wide = BusTopology::default_for(tutti_buffers::ChannelLayout::from(4u16))
            .expect("at most one bus wide");
        for (node, inputs, outputs) in [
            (
                GateNode::<f32>::from(
                    GateConfig::new(BusTopology::MONO)
                        .key(KeySource::Sidechain)
                        .controls(c),
                ),
                2,
                1,
            ),
            (
                GateNode::<f32>::from(
                    GateConfig::new(BusTopology::STEREO)
                        .key(KeySource::Sidechain)
                        .controls(c),
                ),
                4,
                2,
            ),
            (
                GateNode::<f32>::from(GateConfig::new(wide).key(KeySource::Sidechain).controls(c)),
                8,
                4,
            ),
            (
                GateNode::<f32>::from(GateConfig::new(BusTopology::MONO).controls(c)),
                1,
                1,
            ),
            (
                GateNode::<f32>::from(GateConfig::new(BusTopology::STEREO).controls(c)),
                2,
                2,
            ),
            (
                GateNode::<f32>::from(GateConfig::new(wide).controls(c)),
                4,
                4,
            ),
            (
                GateNode::<f32>::from(GateConfig::new(BusTopology::MONO).controls(c)),
                1,
                1,
            ),
            (
                GateNode::<f32>::from(GateConfig::new(BusTopology::STEREO).controls(c)),
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

    /// The controls given to a config are the cells' values, the range too.
    #[test]
    fn the_config_controls_reach_the_cells() {
        let c = GateControls {
            threshold: Db::new(-35.0),
            attack: Seconds::new(0.002),
            hold: Seconds::new(0.03),
            release: Seconds::new(0.25),
            range: Db::new(-15.0),
            ..GateControls::DEFAULT
        };
        for node in [
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(c),
            ),
            GateNode::<f32>::from(GateConfig::new(BusTopology::MONO).controls(c)),
        ] {
            let bases: Vec<_> = (0..6).map(|k| node.param_base(k)).collect();
            assert_eq!(
                bases,
                [-35.0, 0.002, 0.03, 0.25, -15.0, f64::INFINITY].map(Some),
                "threshold, attack, hold, release, range, ratio"
            );
        }
    }

    /// A gate with its own key gates on its own signal: with nothing wired a loud one passes and
    /// one under the threshold is closed to the range.
    #[test]
    fn a_gate_with_its_own_key_gates_on_its_own_signal() {
        let mk = || {
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::MONO)
                    .controls(with_range(four(-20.0, 0.0001, 0.001, 0.001), -40.0)),
            )
        };
        let mut out = [0.0f32];
        let mut node = prepared_at(mk(), RATE_44K);
        for _ in 0..500 {
            tick(&mut node, &[0.5], &mut out);
        }
        assert!((out[0] - 0.5).abs() < 1e-3, "a loud signal opens itself");
        let mut node = prepared_at(mk(), RATE_44K);
        for _ in 0..500 {
            tick(&mut node, &[0.01], &mut out);
        }
        assert!((out[0] / (0.01 * 0.01) - 1.0).abs() < 1e-3, "{}", out[0]);
    }

    /// The shape declares every control in the order of the set (threshold, attack, hold,
    /// release, range), with the cells as their bases, no latency and no tail.
    #[test]
    fn gate_declares_its_params() {
        let m = prepared_at(
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::MONO)
                    .key(KeySource::Sidechain)
                    .controls(with_range(four(-20.0, 0.0001, 0.01, 0.1), -30.0)),
            ),
            RATE_44K,
        );
        let shape = m.shape();
        assert_eq!(
            shape.params.as_slice(),
            &[
                UnitParam::Threshold,
                UnitParam::Attack,
                UnitParam::Hold,
                UnitParam::Release,
                UnitParam::Range,
                UnitParam::Ratio
            ][..]
        );
        assert_eq!(shape.audio_in().count(), 2);
        assert_eq!(shape.latency, tutti_units::Latency::ZERO);
        assert_eq!(shape.tail, tutti_units::Tail::None);
        for (k, base) in [-20.0, 0.0001, 0.01, 0.1, -30.0, f64::INFINITY]
            .into_iter()
            .enumerate()
        {
            assert_eq!(m.param_base(k), Some(base), "control {k}'s base");
        }
    }

    /// A threshold written between blocks is read again: a key that was under it is over the
    /// lowered one, and raising it closes the gate again.
    #[test]
    fn a_changed_threshold_applies_within_a_grid_step() {
        let mut node = prepared_at(gate(-20.0, 0.0, 0.0, 0.0), RATE_44K);
        let mut out = [0.0f32];
        for _ in 0..40 {
            tick(&mut node, &[1.0, 0.05], &mut out); // -26 dB
        }
        assert!(out[0] < 0.01, "closed");
        assert!(node.param_set().set(UnitParam::Threshold, -30.0));
        for _ in 0..40 {
            tick(&mut node, &[1.0, 0.05], &mut out);
        }
        assert_eq!(out[0], 1.0, "open");
        assert!(node.param_set().set(UnitParam::Threshold, -20.0));
        for _ in 0..40 {
            tick(&mut node, &[1.0, 0.05], &mut out);
        }
        assert!(out[0] < 0.01, "raising it closes the gate again");
    }

    /// A fed threshold is read per frame: a frame fed a threshold over the key is closed
    /// where the cell's would open it, and one fed under it is open where the cell's would
    /// not.
    #[test]
    fn a_fed_threshold_is_the_one_read_per_frame() {
        let (mut a, mut b) = ([0.0f32], [0.0f32]);
        let mut open_base = prepared_at(gate(-30.0, 0.0, 0.0, 0.0), RATE_44K);
        tick(&mut open_base, &[1.0, 0.1], &mut a); // -20 dB key over -30
        assert_eq!(a[0], 1.0);
        let mut fed = prepared_at(gate(-30.0, 0.0, 0.0, 0.0), RATE_44K);
        tick_fed(&mut fed, &[1.0, 0.1], &[Some(-10.0)], &mut b);
        assert!(b[0] < 0.01, "a fed -10 dB closes it: {}", b[0]);
        let mut closed_base = prepared_at(gate(-10.0, 0.0, 0.0, 0.0), RATE_44K);
        tick_fed(&mut closed_base, &[1.0, 0.1], &[Some(-30.0)], &mut b);
        assert_eq!(b[0], 1.0, "a fed -30 dB opens it");
    }

    /// NaN on the key is ignored by the max-abs detector (reads as silent): the gate stays
    /// closed and finite, and opens on the next finite level. Audio NaN passes through, as
    /// a gain does.
    #[test]
    fn a_nan_key_does_not_poison_the_gate() {
        let mut node = prepared_at(gate(-20.0, 0.0001, 0.01, 0.1), RATE_44K);
        let audio = vec![1.0f32; 64];
        let nan = vec![f32::NAN; 64];
        let out = drive(
            &mut node,
            RATE_44K,
            Planes::new(&[&audio, &nan]).unwrap(),
            &[],
        )
        .remove(0);
        assert!(out.iter().all(|y| y.is_finite()));
        assert!(out[63] < 1e-3, "closed");
        let (audio, loud) = (vec![1.0f32; 1024], vec![0.5f32; 1024]);
        let out = drive(
            &mut node,
            RATE_44K,
            Planes::new(&[&audio, &loud]).unwrap(),
            &[],
        )
        .remove(0);
        assert!(out[1023] > 0.99, "recovers on a finite key");
    }

    /// A control written NaN is read as the last value it held (`LastGood`): the gate
    /// keeps its threshold and its output stays finite.
    #[test]
    fn a_nan_write_keeps_the_last_threshold() {
        let mut node = prepared_at(gate(-20.0, 0.0, 0.0, 0.0), RATE_44K);
        assert!(node.param_set().set(UnitParam::Threshold, f64::NAN));
        let mut out = [0.0f32];
        for _ in 0..40 {
            tick(&mut node, &[1.0, 0.5], &mut out);
        }
        assert_eq!(
            out[0], 1.0,
            "a -6 dB key is still over the -20 dB threshold"
        );
    }

    /// An `f64` kernel keeps its gain in `f64`: a key over the threshold with an instant attack
    /// opens the gate to a gain of 1 to 2^-50 whatever the range, so a signal of more than 24
    /// bits comes out as it went in.
    #[test]
    fn an_f64_kernel_with_an_open_gate_passes_a_wide_signal_unchanged() {
        use tutti_fixtures::signal::{assert_close_f64, wide_signal};
        let x = wide_signal(128);
        let mut k = GateKernel::<f64>::new(BusTopology::MONO, KeySource::Own, KeyFilter::Off);
        k.prepare(
            SampleRate::SR_48K,
            MaxBlock::of(tutti_units::Samples::new(128)),
        );
        let controls = GateControls {
            threshold: Db::new(-40.0),
            range: Db::new(-80.0),
            ..GateControls::DEFAULT
        };
        let y = drive_kernel(&mut k, &[(0, controls)], std::slice::from_ref(&x));
        assert_close_f64(&y[0], &x, 2.0f64.powi(-50));
    }
}
