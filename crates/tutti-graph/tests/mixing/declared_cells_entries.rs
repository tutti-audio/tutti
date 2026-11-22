//! The cells form of `declare_params!` beyond plain params: an `Option`
//! entry, a set-only entry (after `;`), a `settings` block, and a node with
//! no params at all. Each node is added bare.
//!
//! `Sparse` reads three params and writes each read to an output, so the
//! rendered samples show which port each read took. `Switched` and `Lonely`
//! have no params and render a setting and a required value.

use tutti_graph::testing as common;

use common::prepare;
use tutti_buffers::ChannelLayout;
use tutti_graph::contract::assert_param_fork;
use tutti_graph::{
    Cx, Editor, EventOutputPort, Executor, HasParam, Io, Node, OutputPort, ParamEvent, ParamIn,
    ParamInput, ParamNode, ParamSegment, PortSource, Prepare, Shape, Status, TransportPosition,
    declare_params,
};
use tutti_rt::{AtomicWord, atomic_word};
use tutti_units::{Amplitude, Db, Hz, Mix, Q, Samples, UnitAddr, UnitParam};

/// Three outputs: the reads of `Cutoff`, `Q` and `Volume`.
#[derive(Clone)]
struct Sparse {
    cells: SparseCells,
}

declare_params!([] Sparse, SparseConfig, SparseCells {
    params {
        Volume => volume: Option<Amplitude> = None,
        Q => q: Q = Q::new(0.5),
        Cutoff => cutoff: Hz = Hz::new(1_000.0);
        Wet => wet: Mix = Mix::DRY,
        Threshold => bands[2]: Db = Db::new(-10.0),
    }
});

impl From<SparseConfig> for Sparse {
    fn from(config: SparseConfig) -> Self {
        Self {
            cells: SparseCells::from_config(&config),
        }
    }
}

fn show(out: &mut [f32], input: ParamInput<'_>, base: f64) {
    match input {
        ParamInput::Base => out.fill(base as f32),
        ParamInput::Frames { values: v, .. } => out.copy_from_slice(v),
    }
}

impl Node for Sparse {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::from_count(3))
            .param_addrs(self.declared_ports().as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let cutoff = self.declared_param::<{ UnitParam::Cutoff as u16 }, _>(&io);
        let q = self.declared_param::<{ UnitParam::Q as u16 }, _>(&io);
        let volume = self.declared_param::<{ UnitParam::Volume as u16 }, _>(&io);
        show(io.output(0), cutoff, self.cells.cutoff.load().get());
        show(io.output(1), q, self.cells.q.load().get());
        let base = self.cells.volume.as_ref().map_or(0.0, |v| v.load().get());
        show(io.output(2), volume, base);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

/// At the first frame of every block, steps each address in `to`.
#[derive(Clone)]
struct Steps {
    to: Vec<(UnitAddr, f64)>,
}

impl Steps {
    const SHAPE: Shape = Shape::EMPTY
        .events(0, 1)
        .param_events()
        .event_capacity(tutti_graph::testing::cap_of(8));
    const OUT: tutti_graph::OutEvents = Self::SHAPE.event_output(0).expect("declared");
}

impl Node for Steps {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let at = io.offset(0).expect("a block has a frame");
        for &(addr, v) in &self.to {
            let seg = ParamSegment::unit(addr, v, v, Samples::new(0));
            let _ = io
                .event_out(Self::OUT)
                .push_param(ParamEvent::segment(at, seg));
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

const NODE: tutti_graph::NodeKey = tutti_graph::NodeKey(1);
const SRC: tutti_graph::NodeKey = tutti_graph::NodeKey(2);

fn render_block(exec: &mut Executor, outputs: usize) -> Vec<Vec<f32>> {
    let mut outs = vec![vec![0.0; 64]; outputs];
    {
        let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(|o| &mut o[..]).collect();
        exec.process(64, &TransportPosition::default(), &[], &mut refs);
    }
    outs
}

/// `Sparse` bare, a source stepping `to` at it, and its three outputs.
fn render(sparse: Sparse, to: &[(UnitAddr, f64)]) -> [Vec<f32>; 3] {
    let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(64));
    ed.insert(NODE, "sparse", sparse);
    ed.insert(
        SRC,
        "steps",
        tutti_graph::ForkByClone(Steps { to: to.to_vec() }),
    );
    ed.spec_mut().topology.set_outputs(
        (0..3)
            .map(|port| PortSource::Node(OutputPort::new(NODE, port)))
            .collect::<Vec<_>>(),
    );
    for &(addr, _) in to {
        ed.spec_mut().connect_param(
            ParamIn {
                node: NODE,
                param: addr,
            },
            EventOutputPort::new(SRC, 0),
        );
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    let outs = render_block(&mut exec, 3);
    ed.collect();
    outs.try_into().expect("three outputs")
}

fn sparse() -> Sparse {
    Sparse::from(SparseConfig::new())
}

fn present() -> Sparse {
    Sparse::from(SparseConfig::new().volume(Some(Amplitude::new(0.8))))
}

/// An `Option` entry that is `None` is no port and no param, so a driven
/// `Cutoff` reaches the read of `Cutoff` and no other, whatever position its
/// row is in. (The cells form of `declared_reads`'
/// `a_read_follows_its_row_not_its_position`, whose first row is an absent
/// `Option` cell.)
#[test]
fn an_absent_option_entry_is_no_port_and_a_read_follows_its_row() {
    let node = sparse();
    assert_eq!(
        node.declared_ports().as_slice(),
        [
            UnitAddr::from(UnitParam::Q),
            UnitAddr::from(UnitParam::Cutoff)
        ],
        "the absent row takes no port"
    );
    assert_eq!(node.param_set().get(UnitParam::Volume), None);

    let [cutoff, q, volume] = render(node, &[(UnitAddr::from(UnitParam::Cutoff), 7_000.0)]);
    assert!(
        cutoff.iter().all(|&x| x == 7_000.0),
        "cutoff follows its lane"
    );
    assert!(q.iter().all(|&x| x == 0.5), "q keeps its base");
    assert!(
        volume.iter().all(|&x| x == 0.0),
        "an absent param reads its base"
    );
}

/// An `Option` entry a config gives a value is the first port, with its
/// value as the base, and a lane driven into it reaches its read.
#[test]
fn a_present_option_entry_is_a_port_at_its_row() {
    let node = present();
    assert_eq!(
        node.declared_ports().as_slice(),
        [
            UnitAddr::from(UnitParam::Volume),
            UnitAddr::from(UnitParam::Q),
            UnitAddr::from(UnitParam::Cutoff),
        ]
    );
    assert_eq!(node.declared_base(0), Some(0.8));
    assert_eq!(node.param_set().get(UnitParam::Volume), Some(0.8));

    let [cutoff, q, volume] = render(present(), &[(UnitAddr::from(UnitParam::Volume), 0.3)]);
    assert!(volume.iter().all(|&x| x == 0.3), "volume follows its lane");
    assert!(cutoff.iter().all(|&x| x == 1_000.0));
    assert!(q.iter().all(|&x| x == 0.5));
}

/// A set-only entry (after `;`) is in the node's set and its typed unit, and
/// is no port: the ports and their bases stop at the last driven row.
#[test]
fn a_set_only_entry_is_addressable_and_no_port() {
    fn unit_of<N: HasParam<P, Unit = U>, U, const P: u16>(_: &N) {}
    let node = sparse();
    unit_of::<_, Mix, { UnitParam::Wet as u16 }>(&node);
    unit_of::<_, Db, { UnitParam::Threshold as u16 }>(&node);

    let ports = node.declared_ports();
    assert_eq!(ports.as_slice().len(), 2, "no port past the `;`");
    assert_eq!(node.declared_base(2), None);

    let set = node.param_set();
    assert_eq!(set.get(UnitParam::Wet), Some(0.0));
    assert!(set.set(UnitParam::Wet, 1.0));
    assert_eq!(node.cells.wet.load(), Mix::WET, "the node's own cell");
    assert!(set.set(UnitAddr::new(UnitParam::Threshold, 1), -33.0));
    assert_eq!(node.cells.bands[1].load(), Db::new(-33.0));
    assert_eq!(set.get(UnitAddr::new(UnitParam::Threshold, 0)), Some(-10.0));
    assert_eq!(set.params().count(), 5, "q, cutoff, mix and two bands");
}

/// The contract's fork check covers an `Option` cell and the set-only ones.
#[test]
fn the_contract_fork_check_covers_option_and_set_only_cells() {
    assert_param_fork(present());
}

/// A node with settings and no params: the two words a node reads once per
/// block.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum Mode {
    Off,
    Slow = 3,
    Fast = 9,
}
atomic_word!(Mode { Off, Slow, Fast });

#[derive(Clone)]
struct Switched {
    cells: SwitchedCells,
}

declare_params!([] Switched, SwitchedConfig, SwitchedCells {
    settings { mode: Mode = Mode::Off }
});

impl From<SwitchedConfig> for Switched {
    fn from(config: SwitchedConfig) -> Self {
        Self {
            cells: SwitchedCells::from_config(&config),
        }
    }
}

impl Node for Switched {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_addrs(self.declared_ports().as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let word = self.cells.mode.load().to_word();
        io.output(0).fill(word as f32);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

/// A graph over one bare `node` and its next block's single output.
struct Rig {
    ed: Editor,
    exec: Executor,
}

impl Rig {
    fn new(node: impl tutti_graph::IntoNode) -> Self {
        let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(64));
        ed.insert(NODE, "node", node);
        ed.spec_mut()
            .topology
            .set_outputs([PortSource::Node(OutputPort::new(NODE, 0))]);
        ed.commit().expect("commits");
        exec.apply_pending();
        Self { ed, exec }
    }

    fn block(&mut self) -> Vec<f32> {
        let mut out = vec![0.0; 64];
        self.exec
            .process(64, &TransportPosition::default(), &[], &mut [&mut out[..]]);
        self.ed.collect();
        out
    }
}

/// The config option is the value the first block reads, and an unset entry
/// is its declared default. (The cells form of `declared_settings`'
/// `the_config_option_is_the_initial_value`.)
#[test]
fn a_settings_config_option_is_the_initial_value() {
    let set = Switched::from(SwitchedConfig::new().mode(Mode::Slow));
    assert!(Rig::new(set).block().iter().all(|&x| x == 3.0));

    let default = Switched::from(SwitchedConfig::new());
    assert!(Rig::new(default).block().iter().all(|&x| x == 0.0));
}

/// A control-side `store` through a clone's cells lands in the next block,
/// whole, and each later `store` replaces it. (The cells form of
/// `declared_settings`' `a_control_side_set_is_read_in_the_next_block`.)
#[test]
fn a_settings_store_is_read_in_the_next_block() {
    let node = Switched::from(SwitchedConfig::new());
    let host = node.cells.clone();
    let mut rig = Rig::new(node);
    assert!(rig.block().iter().all(|&x| x == 0.0));
    host.mode().store(Mode::Fast);
    assert!(rig.block().iter().all(|&x| x == 9.0));
    host.mode().store(Mode::Slow);
    assert!(rig.block().iter().all(|&x| x == 3.0));
    assert_eq!(host.mode().load(), Mode::Slow);
}

/// A clone's settings are the same cell and a fork's are its own, at the
/// value they held at the fork.
#[test]
fn a_clone_shares_a_setting_and_a_fork_owns_it() {
    let node = Switched::from(SwitchedConfig::new().mode(Mode::Slow));
    let shared = node.clone();
    let fork = ParamNode::<f32>::fork_fresh(&node);
    node.cells.mode().store(Mode::Fast);
    assert_eq!(shared.cells.mode().load(), Mode::Fast, "a clone shares");
    assert_eq!(
        fork.cells.mode().load(),
        Mode::Slow,
        "a fork holds its copy"
    );
    fork.cells.mode().store(Mode::Off);
    assert_eq!(
        node.cells.mode().load(),
        Mode::Fast,
        "and shares nothing after"
    );
}

/// A node with a required value and nothing else: no ports, an empty set.
#[derive(Clone)]
struct Lonely {
    layout: ChannelLayout,
    cells: LonelyCells,
}

declare_params!([] Lonely, LonelyConfig, LonelyCells {
    required { layout: ChannelLayout }
    params {}
});

impl From<LonelyConfig> for Lonely {
    fn from(config: LonelyConfig) -> Self {
        Self {
            layout: config.layout,
            cells: LonelyCells::from_config(&config),
        }
    }
}

impl Node for Lonely {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.layout).param_addrs(self.declared_ports().as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(2.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A node with no params and no settings is added bare, renders, and has an
/// empty set and no ports.
#[test]
fn a_node_with_no_params_is_added_bare_and_renders() {
    let node = Lonely::from(LonelyConfig::new(ChannelLayout::MONO));
    assert_eq!(node.layout, ChannelLayout::MONO);
    assert!(node.declared_ports().as_slice().is_empty());
    assert_eq!(node.param_set().params().count(), 0);
    assert!(Rig::new(node).block().iter().all(|&x| x == 2.0));
}
