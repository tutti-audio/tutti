//! A node declared in the cells form of `declare_params!` is added bare, and
//! the macro writes its cells, its config and its traits.
//!
//! The node below states its params in an order no sorted guess would give
//! (`Q` before `Cutoff`, a counted kind last), so a port is not the position a
//! reader would assume. A source steps `Cutoff` and the second band's
//! `Threshold`; the node's outputs show which param each read took. An
//! old-form node in the same graph is driven through its own declaration.

use tutti_graph::testing as common;

use common::prepare;
use tutti_buffers::ChannelLayout;
use tutti_graph::contract::assert_param_fork;
use tutti_graph::{
    Cx, DeclaredNode, Editor, EventOutputPort, Executor, ForkTarget, Io, Node, NodeKey, OutputPort,
    ParamEvent, ParamIn, ParamInput, ParamNode, ParamSegment, ParamSet, Params, PortSource,
    Prepare, Shape, Status, TransportPosition, declare_params,
};
use tutti_rt::Param;
use tutti_units::{Db, Hz, Q, Samples, UnitAddr, UnitParam};

/// Four outputs: the reads of `Cutoff`, `Q`, band 1 and band 0.
#[derive(Clone)]
struct Bank {
    layout: ChannelLayout,
    cells: BankCells,
}

declare_params!([] Bank, BankConfig, BankCells {
    required { layout: ChannelLayout }
    params {
        Q => q: Q = Q::new(0.5),
        Cutoff => cutoff: Hz = Hz::new(1_000.0),
        Threshold => bands[2]: Db = Db::new(-10.0),
    }
});

impl From<BankConfig> for Bank {
    fn from(config: BankConfig) -> Self {
        Self {
            layout: config.layout,
            cells: BankCells::from_config(&config),
        }
    }
}

fn show(out: &mut [f32], input: ParamInput<'_>, base: f64) {
    match input {
        ParamInput::Base => out.fill(base as f32),
        ParamInput::Frames { values: v, .. } => out.copy_from_slice(v),
    }
}

impl Node for Bank {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.layout)
            .param_addrs(DeclaredNode::param_ports(self).as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let cutoff = self.declared_param::<{ UnitParam::Cutoff as u16 }, _>(&io);
        let q = self.declared_param::<{ UnitParam::Q as u16 }, _>(&io);
        let band0 = self.declared_param_at::<{ UnitParam::Threshold as u16 }, _>(&io, 0);
        let band1 = self.declared_param_at::<{ UnitParam::Threshold as u16 }, _>(&io, 1);
        show(io.output(0), cutoff, self.cells.cutoff.load().get());
        show(io.output(1), q, self.cells.q.load().get());
        show(io.output(2), band1, self.cells.bands[1].load().get());
        show(io.output(3), band0, self.cells.bands[0].load().get());
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        DeclaredNode::param_base(self, port)
    }
}

fn bank() -> Bank {
    Bank::from(
        BankConfig::new(ChannelLayout::from_count(4)).bands([Db::new(-10.0), Db::new(-20.0)]),
    )
}

/// The old form: one `Cutoff` cell, inserted as `Params(..)`. Its one output
/// is the read of `Cutoff`.
#[derive(Clone)]
struct Old {
    cutoff: Param<Hz>,
}

declare_params!([] Old, n {
    Cutoff => Hz = &n.cutoff,
});

impl Node for Old {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_addrs(self.declared_ports().as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let cutoff = self.declared_param::<{ UnitParam::Cutoff as u16 }, _>(&io);
        show(io.output(0), cutoff, self.cutoff.load().get());
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

impl ParamNode for Old {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut f = self.clone();
        f.cutoff.detach();
        f
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

const NODE: NodeKey = NodeKey(1);
const SRC: NodeKey = NodeKey(2);
const OLD: NodeKey = NodeKey(3);

fn render_block(exec: &mut Executor, outputs: usize) -> Vec<Vec<f32>> {
    let mut outs = vec![vec![0.0; 64]; outputs];
    {
        let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(|o| &mut o[..]).collect();
        exec.process(64, &TransportPosition::default(), &[], &mut refs);
    }
    outs
}

/// `Bank` bare, a source stepping `to` at it, and its four outputs.
fn render(to: &[(UnitAddr, f64)]) -> [Vec<f32>; 4] {
    let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(64));
    ed.insert(NODE, "bank", bank());
    ed.insert(
        SRC,
        "steps",
        tutti_graph::ForkByClone(Steps { to: to.to_vec() }),
    );
    ed.spec_mut().topology.set_outputs(
        (0..4)
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
    let outs = render_block(&mut exec, 4);
    ed.collect();
    outs.try_into().expect("four outputs")
}

/// A driven `Cutoff` reaches the read of `Cutoff` and no other, whatever
/// port its row is, and the params nothing drives read their bases.
#[test]
fn a_read_follows_its_row_not_its_position() {
    let [cutoff, q, band1, band0] = render(&[(UnitAddr::from(UnitParam::Cutoff), 7_000.0)]);
    assert!(
        cutoff.iter().all(|&x| x == 7_000.0),
        "cutoff follows its lane"
    );
    assert!(q.iter().all(|&x| x == 0.5), "q keeps its base");
    assert!(band1.iter().all(|&x| x == -20.0));
    assert!(band0.iter().all(|&x| x == -10.0));
}

/// A counted kind is read at its own index: stepping band 1 leaves band 0
/// and the rows before it alone.
#[test]
fn a_counted_read_is_at_its_index() {
    let [cutoff, q, band1, band0] = render(&[(UnitAddr::new(UnitParam::Threshold, 1), -3.0)]);
    assert!(band1.iter().all(|&x| x == -3.0), "band 1 follows its lane");
    assert!(band0.iter().all(|&x| x == -10.0), "band 0 keeps its base");
    assert!(cutoff.iter().all(|&x| x == 1_000.0));
    assert!(q.iter().all(|&x| x == 0.5));
}

/// The ports are the params in the order declared, each counted entry's
/// indices in turn, and a port's base is its cell's value.
#[test]
fn the_ports_are_the_params_in_declared_order() {
    let node = bank();
    let ports = DeclaredNode::param_ports(&node);
    assert_eq!(
        ports.as_slice(),
        [
            UnitAddr::from(UnitParam::Q),
            UnitAddr::from(UnitParam::Cutoff),
            UnitAddr::new(UnitParam::Threshold, 0),
            UnitAddr::new(UnitParam::Threshold, 1),
        ]
    );
    let bases: Vec<_> = (0..5).map(|k| DeclaredNode::param_base(&node, k)).collect();
    assert_eq!(
        bases,
        [Some(0.5), Some(1_000.0), Some(-10.0), Some(-20.0), None]
    );
}

/// `set` and `get` reach each index's own cell, and a bare `UnitParam` is
/// index 0.
#[test]
fn set_and_get_address_each_index_independently() {
    let node = bank();
    let set = node.param_set();
    assert_eq!(set.get(UnitAddr::new(UnitParam::Threshold, 1)), Some(-20.0));
    assert!(set.set(UnitAddr::new(UnitParam::Threshold, 1), -33.0));
    assert_eq!(set.get(UnitAddr::new(UnitParam::Threshold, 0)), Some(-10.0));
    assert_eq!(
        node.cells.bands[1].load(),
        Db::new(-33.0),
        "the node's own cell"
    );
    assert!(set.set(UnitParam::Threshold, -6.0));
    assert_eq!(node.cells.bands[0].load(), Db::new(-6.0));
    assert_eq!(set.params().count(), 4);
}

/// The config's required values and options are the node's initial state:
/// a param no option names holds its declared default.
#[test]
fn a_config_sets_the_initial_cells_and_defaults_the_rest() {
    let node = Bank::from(BankConfig::new(ChannelLayout::from_count(4)).cutoff(Hz::new(250.0)));
    let set = node.param_set();
    assert_eq!(set.get(UnitParam::Cutoff), Some(250.0));
    assert_eq!(set.get(UnitParam::Q), Some(0.5), "the declared default");
    assert_eq!(set.get(UnitAddr::new(UnitParam::Threshold, 1)), Some(-10.0));
    assert_eq!(node.layout, ChannelLayout::from_count(4));
}

/// `Clone` shares the cells and `detached` copies them apart at the values
/// they hold.
#[test]
fn clone_shares_cells_and_detached_does_not() {
    let a = bank();
    let shared = a.clone();
    let apart = a.cells.detached();
    a.cells.cutoff.store(Hz::new(42.0));
    assert_eq!(shared.cells.cutoff.load(), Hz::new(42.0), "a clone shares");
    assert_eq!(
        apart.cutoff.load(),
        Hz::new(1_000.0),
        "detached is a copy at its time"
    );
    apart.cutoff.store(Hz::new(7.0));
    assert_eq!(
        a.cells.cutoff.load(),
        Hz::new(42.0),
        "and shares nothing after"
    );
}

/// The contract's fork check covers every param the macro addresses, each
/// index of the counted one included.
#[test]
fn the_contract_fork_check_covers_each_declared_param() {
    assert_param_fork(bank());
}

/// A fork of the graph starts from each index's own value, and its cells are
/// its own.
#[test]
fn a_fork_starts_from_each_indexs_value() {
    let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(64));
    let controls = ed.insert(NODE, "bank", bank()).controls;
    ed.spec_mut().topology.set_outputs(
        (0..4)
            .map(|port| PortSource::Node(OutputPort::new(NODE, port)))
            .collect::<Vec<_>>(),
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    let band1 = UnitAddr::new(UnitParam::Threshold, 1);
    assert!(controls.set(band1, -33.0));
    let (mut fork_ed, mut fork_exec) = ed.fork(ForkTarget::MainOutput, prepare(64)).expect("forks");
    assert!(controls.set(band1, -1.0), "a write after the fork");
    let [_, _, f1, f0] = <[_; 4]>::try_from(render_block(&mut fork_exec, 4)).unwrap();
    assert!(f1.iter().all(|&x| x == -33.0), "band 1 as it was set");
    assert!(f0.iter().all(|&x| x == -10.0), "band 0 as it was");
    fork_ed.collect();
    let [_, _, l1, _] = <[_; 4]>::try_from(render_block(&mut exec, 4)).unwrap();
    assert!(l1.iter().all(|&x| x == -1.0), "the live node moved on");
}

/// An old-form node (`Params(..)`) and a new-form node (bare) in one graph,
/// each driven through its own declaration: the old one's `Cutoff` follows
/// its lane, the new one's second band follows its lane, and neither read
/// moves the other node's params.
#[test]
fn an_old_form_node_and_a_new_form_node_share_a_graph() {
    let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(64));
    ed.insert(NODE, "bank", bank());
    let old = ed
        .insert(
            OLD,
            "old",
            Params(Old {
                cutoff: Param::new(Hz::new(500.0)),
            }),
        )
        .controls;
    let to = vec![
        (UnitAddr::from(UnitParam::Cutoff), 7_000.0),
        (UnitAddr::new(UnitParam::Threshold, 1), -3.0),
    ];
    ed.insert(SRC, "steps", tutti_graph::ForkByClone(Steps { to }));
    // Outputs: the old node's read, then the bank's cutoff, band 1 and q.
    ed.spec_mut().topology.set_outputs(vec![
        PortSource::Node(OutputPort::new(OLD, 0)),
        PortSource::Node(OutputPort::new(NODE, 0)),
        PortSource::Node(OutputPort::new(NODE, 2)),
        PortSource::Node(OutputPort::new(NODE, 1)),
    ]);
    // `Cutoff` drives the old node only; band 1 drives the bank only.
    ed.spec_mut().connect_param(
        ParamIn {
            node: OLD,
            param: UnitAddr::from(UnitParam::Cutoff),
        },
        EventOutputPort::new(SRC, 0),
    );
    ed.spec_mut().connect_param(
        ParamIn {
            node: NODE,
            param: UnitAddr::new(UnitParam::Threshold, 1),
        },
        EventOutputPort::new(SRC, 0),
    );
    ed.commit().expect("commits");
    exec.apply_pending();
    let [old_cutoff, bank_cutoff, band1, q]: [Vec<f32>; 4] =
        render_block(&mut exec, 4).try_into().unwrap();
    ed.collect();
    assert!(
        old_cutoff.iter().all(|&x| x == 7_000.0),
        "old node follows its lane"
    );
    assert!(
        bank_cutoff.iter().all(|&x| x == 1_000.0),
        "bank cutoff is on its base"
    );
    assert!(
        band1.iter().all(|&x| x == -3.0),
        "bank band 1 follows its lane"
    );
    assert!(q.iter().all(|&x| x == 0.5), "bank q is on its base");
    assert_eq!(
        old.get(UnitParam::Cutoff),
        Some(500.0),
        "the old node's knob is untouched"
    );
}
