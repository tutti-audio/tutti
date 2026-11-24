//! A node's `declare_params!` `settings` block: the config option is the
//! initial value, and a control-side `set` is what the node reads in the next
//! block it renders.
//!
//! The node reads its setting once per block and writes the setting's word to
//! every frame of its output, so the rendered samples show which value each
//! block read.

use std::sync::Arc;

use tutti_graph::testing as common;

use common::prepare;
use tutti_buffers::ChannelLayout;
use tutti_graph::{
    Cx, Editor, Executor, ForkByClone, Io, Node, OutputPort, PortSource, Prepare, Shape, Status,
    TransportPosition, declare_params,
};
use tutti_rt::{AtomicCell, AtomicWord, atomic_word};

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum Mode {
    Off,
    Slow = 3,
    Fast = 9,
}
atomic_word!(Mode { Off, Slow, Fast });

/// The cells the node and its host share.
#[derive(Debug)]
struct Cells {
    mode: AtomicCell<Mode>,
}

declare_params!([] Cells, settings CellsConfig { mode: Mode = Mode::Off });

impl From<CellsConfig> for Cells {
    fn from(config: CellsConfig) -> Self {
        Self {
            mode: AtomicCell::new(config.mode),
        }
    }
}

#[derive(Clone)]
struct Probe {
    cells: Arc<Cells>,
}

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let word = self.cells.mode.load().to_word();
        io.output(0).fill(word as f32);
        Status::Modified
    }
    fn reset(&mut self) {}
}

const NODE: tutti_graph::NodeKey = tutti_graph::NodeKey(1);

/// A graph over one probe, and a closure-free renderer of its next block.
struct Rig {
    ed: Editor,
    exec: Executor,
}

impl Rig {
    fn new(cells: &Arc<Cells>) -> Self {
        let (mut ed, mut exec): (Editor, Executor) = Editor::new(prepare(64));
        ed.insert(
            NODE,
            "probe",
            ForkByClone(Probe {
                cells: Arc::clone(cells),
            }),
        );
        ed.wire_outputs([PortSource::Node(OutputPort::new(NODE, 0))])
            .unwrap();
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

/// The config option is the value the first block reads; an unset entry is
/// its declared default.
#[test]
fn the_config_option_is_the_initial_value() {
    let set = Arc::new(Cells::from(CellsConfig::new().mode(Mode::Slow)));
    assert!(Rig::new(&set).block().iter().all(|&x| x == 3.0));

    let default = Arc::new(Cells::from(CellsConfig::new()));
    assert!(Rig::new(&default).block().iter().all(|&x| x == 0.0));
}

/// A control-side `set` lands in the next block, whole, and each later `set`
/// replaces it.
#[test]
fn a_control_side_set_is_read_in_the_next_block() {
    let cells = Arc::new(Cells::from(CellsConfig::new()));
    let mut rig = Rig::new(&cells);
    assert!(rig.block().iter().all(|&x| x == 0.0));
    cells.mode().store(Mode::Fast);
    assert!(rig.block().iter().all(|&x| x == 9.0));
    cells.mode().store(Mode::Slow);
    assert!(rig.block().iter().all(|&x| x == 3.0));
    assert_eq!(cells.mode().load(), Mode::Slow);
}
