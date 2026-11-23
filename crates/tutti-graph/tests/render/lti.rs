//! The linear time-invariant oracle: a path sum with delay compensation.
//!
//! A graph is **built from a grammar and its figure is built beside it**, one
//! node at a time, never computed from a finished graph. Every signal is a
//! sparse sum `Σ c · atom[n - d]` over the main inputs and the step of a
//! constant (`Series`), together with its departure (the frames of declared
//! latency on its longest path). A node's arrival is the latest departure
//! among the sources of its ports, each source is delayed by its gap to that
//! arrival (the compensation the latency pass defines), a node maps the sum at
//! its ports (a gain scales, a lag shifts, a smoother is `0.5 (1 + z^-1)`), and
//! a feedback edge delays by exactly its frames, unaligned. A loop of gain `g`
//! and `p` frames closes on `y = x + g z^-p y`: the geometric series
//! `Σ g^k z^-kp x`. The output channels are aligned to the latest departure
//! among them.
//!
//! Every figure is dyadic (gains are powers of two, inputs and constants are
//! multiples of `2^-4`, and a signal's weights are kept within a bound that
//! fits a mantissa), so `f32` and `f64` renders equal the series exactly.

use std::collections::BTreeMap;

use proptest::prelude::*;
use tutti_buffers::ChannelLayout;
use tutti_graph::testing::{Kind, Word, input_signal};
use tutti_graph::{
    Edge, FeedbackFrom, GraphSpec, InputPort, NodeKey, OutputPort, PortSource, Topology,
};
use tutti_units::Samples;

use crate::generator::{MAX_BLOCK, Rng, schedule, spec_for};
use crate::rig::Rig;

/// Frames a case renders: a feedback loop of the generator's longest delay
/// closes a few times within it.
const FRAMES: usize = 700;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Atom {
    /// Main input channel `c`, which is the run's input scaled by `2^-c`.
    Input(u16),
    /// A constant: `1` from frame 0.
    Step,
}

/// `Σ c · atom[n - d]` over `(atom, d)`.
#[derive(Clone, Debug, Default)]
struct Series(BTreeMap<(Atom, usize), f64>);

impl Series {
    fn of(atom: Atom, weight: f64) -> Self {
        Self(BTreeMap::from([((atom, 0), weight)]))
    }

    fn scaled(&self, g: f64) -> Self {
        Self(self.0.iter().map(|(&k, &c)| (k, c * g)).collect())
    }

    /// Delayed by `by` frames; a term past the run never sounds.
    fn shifted(&self, by: usize) -> Self {
        Self(
            self.0
                .iter()
                .filter(|&(&(_, d), _)| d + by < FRAMES)
                .map(|(&(a, d), &c)| ((a, d + by), c))
                .collect(),
        )
    }

    fn plus(mut self, other: &Self) -> Self {
        for (&k, &c) in &other.0 {
            *self.0.entry(k).or_insert(0.0) += c;
        }
        self
    }

    /// Weights a mantissa holds exactly against inputs on a `2^-4` grid.
    fn is_exact(&self) -> bool {
        self.0.values().map(|c| c.abs()).sum::<f64>() <= 64.0
            && self.0.values().all(|c| (c * 4096.0).fract() == 0.0)
    }

    /// The signal at frame `n`, given the run's main input.
    fn at(&self, n: usize, input: &[f64]) -> f64 {
        self.0
            .iter()
            .filter(|&(&(_, d), _)| d <= n)
            .map(|(&(atom, d), &c)| {
                c * match atom {
                    Atom::Input(ch) => input[n - d] * 0.5f64.powi(i32::from(ch)),
                    Atom::Step => 1.0,
                }
            })
            .sum()
    }
}

/// A source a later node may read: where it leaves, what it carries, and when.
#[derive(Clone, Debug)]
struct Signal {
    from: PortSource,
    series: Series,
    departure: usize,
}

struct Built {
    kinds: BTreeMap<NodeKey, Kind>,
    topology: Topology,
    /// Each output channel's figure.
    outputs: Vec<Series>,
}

struct Builder {
    rng: Rng,
    kinds: BTreeMap<NodeKey, Kind>,
    topology: Topology,
    signals: Vec<Signal>,
    next: u64,
}

impl Builder {
    fn new(seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let inputs = 1 + rng.below(3) as u16;
        let topology = Topology {
            inputs: ChannelLayout::from_count(inputs),
            ..Topology::default()
        };
        let signals = (0..inputs)
            .map(|c| Signal {
                from: PortSource::MainInput(c),
                series: Series::of(Atom::Input(c), 1.0),
                departure: 0,
            })
            .collect();
        Self {
            rng,
            kinds: BTreeMap::new(),
            topology,
            signals,
            next: 1,
        }
    }

    fn key(&mut self) -> NodeKey {
        self.next += 1;
        NodeKey(self.next)
    }

    fn add(&mut self, key: NodeKey, kind: Kind) {
        self.topology.nodes.insert(key, spec_for(&kind));
        self.kinds.insert(key, kind);
    }

    /// Up to two distinct sources for one port, silence if none.
    fn sources(&mut self) -> Vec<usize> {
        let mut picked: Vec<usize> = Vec::new();
        for _ in 0..self.rng.below(3) {
            let i = self.rng.below(self.signals.len() as u64) as usize;
            if !picked.contains(&i) {
                picked.push(i);
            }
        }
        picked
    }

    /// Wires `picked` into `at`; returns the port's figure aligned to
    /// `arrival`.
    fn wire(&mut self, at: InputPort, picked: &[usize], arrival: usize) -> Series {
        let mut edges: Vec<Edge> = picked
            .iter()
            .map(|&i| Edge::Direct(self.signals[i].from))
            .collect();
        edges.sort();
        if !edges.is_empty() {
            self.topology.edges.insert(at, edges);
        }
        picked.iter().fold(Series::default(), |sum, &i| {
            let s = &self.signals[i];
            sum.plus(&s.series.shifted(arrival - s.departure))
        })
    }

    /// A node of `kind` with one port per entry of `ports`; `f` maps the
    /// figures at its ports to its output, `latency` is what it declares.
    fn node(&mut self, kind: Kind, latency: usize, f: impl Fn(&[Series]) -> Series) -> Option<()> {
        let ports = (0..crate::generator::shape(&kind).audio_in().count())
            .map(|_| self.sources())
            .collect::<Vec<_>>();
        let arrival = ports
            .iter()
            .flatten()
            .map(|&i| self.signals[i].departure)
            .max()
            .unwrap_or(0);
        let key = self.key();
        let figures: Vec<Series> = ports
            .iter()
            .enumerate()
            .map(|(p, picked)| self.wire(InputPort::new(key, p as u16), picked, arrival))
            .collect();
        let out = f(&figures);
        if !out.is_exact() {
            self.topology.edges.retain(|at, _| at.node() != key);
            return None;
        }
        self.add(key, kind);
        self.signals.push(Signal {
            from: PortSource::Node(OutputPort::new(key, 0)),
            series: out,
            departure: arrival + latency,
        });
        Some(())
    }

    /// `S = x + z^-d (G(S))`: a sum fed by `x` and by the feedback of a gain
    /// (and a lag of `lag` frames, when `lag > 0`) after it.
    fn feedback_loop(&mut self) -> Option<()> {
        let x = self.rng.below(self.signals.len() as u64) as usize;
        let g = [0.5, -0.5, 0.25, -0.25][self.rng.below(4) as usize];
        let lag = [0usize, 0, 5, 23][self.rng.below(4) as usize];
        let delay = MAX_BLOCK + 17 * self.rng.below(3) as usize;
        let (sum, gain, lagged) = (self.key(), self.key(), self.key());
        let arrival = self.signals[x].departure;
        let aligned = self.signals[x].series.clone();
        // y = Σ_k g^k x z^-k(d + lag): the series the loop closes on.
        let period = delay + lag;
        let mut y = Series::default();
        let mut term = aligned;
        for _ in 0..=FRAMES / period {
            y = y.plus(&term);
            term = term.scaled(g).shifted(period);
        }
        if !y.is_exact() {
            return None;
        }
        let tail = if lag > 0 { lagged } else { gain };
        self.add(sum, Kind::Sum { inputs: 2 });
        self.add(
            gain,
            Kind::Gain {
                gain: g as f32,
                width: 1,
            },
        );
        let from = self.signals[x].from;
        self.topology
            .edges
            .insert(InputPort::new(sum, 0), vec![Edge::Direct(from)]);
        self.topology.edges.insert(
            InputPort::new(sum, 1),
            vec![Edge::Feedback(FeedbackFrom::new(
                OutputPort::new(tail, 0),
                Samples::new(delay),
            ))],
        );
        let node = |k: NodeKey| PortSource::Node(OutputPort::new(k, 0));
        self.topology
            .edges
            .insert(InputPort::new(gain, 0), vec![Edge::Direct(node(sum))]);
        let gained = y.scaled(g);
        let mut push = |from: PortSource, series: Series, departure: usize| {
            self.signals.push(Signal {
                from,
                series,
                departure,
            });
        };
        push(node(sum), y, arrival);
        push(node(gain), gained.clone(), arrival);
        if lag > 0 {
            self.kinds.insert(lagged, Kind::Lag { latency: lag });
            self.topology
                .nodes
                .insert(lagged, spec_for(&Kind::Lag { latency: lag }));
            self.topology
                .edges
                .insert(InputPort::new(lagged, 0), vec![Edge::Direct(node(gain))]);
            self.signals.push(Signal {
                from: node(lagged),
                series: gained.shifted(lag),
                departure: arrival + lag,
            });
        }
        Some(())
    }

    fn finish(mut self) -> Built {
        let channels = 1 + self.rng.below(3);
        let mut chosen: Vec<Vec<usize>> = Vec::new();
        for _ in 0..channels {
            let n = 1 + self.rng.below(2);
            let mut one = Vec::new();
            for _ in 0..n {
                let i = self.rng.below(self.signals.len() as u64) as usize;
                if !one.contains(&i) {
                    one.push(i);
                }
            }
            chosen.push(one);
        }
        let time = chosen
            .iter()
            .flatten()
            .map(|&i| self.signals[i].departure)
            .max()
            .unwrap_or(0);
        let mut outputs = Vec::new();
        self.topology
            .set_outputs(chosen.iter().map(|c| self.signals[c[0]].from));
        for (ch, one) in chosen.iter().enumerate() {
            let mut sources: Vec<PortSource> = one.iter().map(|&i| self.signals[i].from).collect();
            sources.sort();
            self.topology.outputs[ch] = sources;
            outputs.push(one.iter().fold(Series::default(), |sum, &i| {
                let s = &self.signals[i];
                sum.plus(&s.series.shifted(time - s.departure))
            }));
        }
        Built {
            kinds: self.kinds,
            topology: self.topology,
            outputs,
        }
    }
}

fn build(seed: u64) -> Built {
    let mut b = Builder::new(seed);
    for _ in 0..3 + b.rng.below(8) {
        match b.rng.below(9) {
            0 => {
                let value = (b.rng.below(9) as f32 - 4.0) * 0.25 + 0.125;
                let key = b.key();
                b.add(key, Kind::Const { value, width: 1 });
                b.signals.push(Signal {
                    from: PortSource::Node(OutputPort::new(key, 0)),
                    series: Series::of(Atom::Step, f64::from(value)),
                    departure: 0,
                });
            }
            1 => {
                let g = 0.25 + b.rng.below(8) as f64 * 0.25;
                let kind = Kind::Gain {
                    gain: g as f32,
                    width: 1,
                };
                b.node(kind, 0, |p| p[0].scaled(g));
            }
            2 => {
                let inputs = 1 + b.rng.below(3) as usize;
                b.node(Kind::Sum { inputs }, 0, |p| {
                    p.iter().fold(Series::default(), |s, x| s.plus(x))
                });
            }
            3 => {
                b.node(Kind::Smooth, 0, |p| {
                    p[0].scaled(0.5).plus(&p[0].shifted(1).scaled(0.5))
                });
            }
            4 | 5 => {
                let latency = b.rng.below(24) as usize;
                b.node(Kind::Lag { latency }, latency, |p| p[0].shifted(latency));
            }
            6 => {
                b.node(Kind::Thru { width: 1 }, 0, |p| p[0].clone());
            }
            7 => {
                b.node(Kind::ThruInPlace { width: 1 }, 0, |p| p[0].clone());
            }
            _ => {
                b.feedback_loop();
            }
        }
    }
    b.finish()
}

/// The built graph through the executor at sample format `F` and block
/// schedule `which`, against each output channel's series.
fn renders_its_series<F: Word>(seed: u64, which: usize) {
    let built = build(seed);
    let valid = GraphSpec::new(built.topology)
        .validate()
        .expect("built graphs are valid");
    let mut rig = Rig::<F>::new(MAX_BLOCK);
    rig.switch(&valid, &built.kinds);
    let input: Vec<f64> = input_signal::<f64>(0, FRAMES);
    let mut frame = 0;
    for n in schedule(which, seed, FRAMES) {
        let got = rig.block(n, &input_signal::<F>(frame as u64, n));
        for (c, channel) in got.iter().enumerate() {
            for (i, &y) in channel.iter().enumerate() {
                let f = frame + i;
                if f >= FRAMES {
                    break;
                }
                assert_eq!(
                    y.to_f64(),
                    built.outputs[c].at(f, &input),
                    "seed {seed}, channel {c}, frame {f}"
                );
            }
        }
        frame += n;
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: tutti_fixtures::cases(256), ..ProptestConfig::default() })]

    /// Every built graph renders its path sum, delay-compensated and with
    /// loops closed, exactly, at every block schedule.
    #[test]
    fn a_path_sum_with_compensation_renders_exactly(seed in any::<u64>(), which in 0usize..5) {
        renders_its_series::<f32>(seed, which);
    }

    /// The same at `f64`.
    #[test]
    fn a_path_sum_with_compensation_renders_exactly_at_f64(seed in any::<u64>(), which in 0usize..5) {
        renders_its_series::<f64>(seed, which);
    }
}

/// The grammar is not vacuous: over a fixed range of seeds the built graphs
/// hold compensation (a source delayed to a later arrival), loops, fan-in at
/// a port and a summed output channel.
#[test]
fn the_grammar_builds_compensation_loops_and_fan_in() {
    let (mut loops, mut fan_in, mut summed, mut compensated) = (0, 0, 0, 0);
    for seed in 0..512u64 {
        let built = build(seed);
        let t = &built.topology;
        loops += usize::from(
            t.edges
                .values()
                .flatten()
                .any(|e| matches!(e, Edge::Feedback(_))),
        );
        fan_in += usize::from(t.edges.values().any(|es| es.len() > 1));
        summed += usize::from(t.outputs.iter().any(|s| s.len() > 1));
        let valid = GraphSpec::new(t.clone()).validate().expect("valid");
        let (plan, _) = tutti_graph::compile(
            &valid,
            &tutti_graph::testing::shapes_of(&built.kinds),
            &tutti_graph::testing::prepare(MAX_BLOCK),
            None,
        )
        .expect("compiles");
        compensated += usize::from(!plan.delays().is_empty());
    }
    assert!(loops > 50, "only {loops} graphs hold a loop");
    assert!(fan_in > 100, "only {fan_in} graphs fan in at a port");
    assert!(summed > 50, "only {summed} graphs sum an output channel");
    assert!(
        compensated > 100,
        "only {compensated} graphs hold compensation"
    );
}
