//! A node declared in the cells form of `declare_params!` enters the session
//! through its `NodeParts`: its params are addressed from the parts, a write
//! through its key reaches the cell the node reads, and a fork starts from the
//! values set. The node has no `SessionNode` method of its own: its one empty
//! impl is the admission the session's `add` still asks of every type.

use tutti_buffers::{ChannelLayout, InterleavedMut};
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status, declare_params};
use tutti_session::{DetachedSession, MainOutput, Session, SessionNode};
use tutti_units::{Amplitude, SampleRate};

/// A source of its `level`, on one channel.
#[derive(Clone)]
struct Level {
    cells: LevelCells,
}

declare_params!([] Level, LevelConfig, LevelCells {
    params {
        Volume => level: Amplitude = Amplitude::new(0.25),
    }
});

impl From<LevelConfig> for Level {
    fn from(config: LevelConfig) -> Self {
        Self {
            cells: LevelCells::from_config(&config),
        }
    }
}

impl Node for Level {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_addrs(self.declared_ports().as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(self.cells.level.load().get() as f32);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

impl SessionNode for Level {}

fn session_with_level() -> (DetachedSession, tutti_session::Key<Level>) {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let level = s.graph().add(Level::from(LevelConfig::new()));
    s.graph()
        .set_sources(MainOutput::channel(0), [level.output(0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    (s, level)
}

fn frame(s: &mut DetachedSession) -> f32 {
    let mut out = [0.0f32];
    s.render_frame(InterleavedMut::one_frame(&mut out).unwrap());
    out[0]
}

/// **A cells-form node's params are addressed from its parts**: the key reads
/// the declared default, a write lands in the cell the node renders from, and
/// the next block carries it.
#[test]
fn a_cells_node_is_addressed_by_its_parts_and_a_write_reaches_its_next_block() {
    let (mut s, level) = session_with_level();
    assert_eq!(s.get(level.volume()).unwrap(), Amplitude::new(0.25));
    assert_eq!(frame(&mut s), 0.25, "the declared default renders");

    s.set(level.volume().to(Amplitude::new(0.75))).unwrap();
    assert_eq!(s.get(level.volume()).unwrap(), Amplitude::new(0.75));
    assert_eq!(frame(&mut s), 0.75, "the written value renders next block");
}

/// **A fork of a cells-form node starts from the values set at the fork**,
/// and a later write does not reach it: the parts' fork detaches the cells.
#[test]
fn a_cells_node_forks_from_the_values_set() {
    let (s, level) = session_with_level();
    s.set(level.volume().to(Amplitude::new(0.5))).unwrap();
    let mut fork = s
        .graph_ref()
        .testing()
        .fork_live()
        .expect("a cells node forks");
    s.set(level.volume().to(Amplitude::new(0.9))).unwrap();
    let mut forked = [0.0f32];
    fork.tick(InterleavedMut::one_frame(&mut forked).unwrap());
    assert_eq!(forked, [0.5]);
}
