//! A node declared in the cells form of `declare_params!` enters the session
//! through its `NodeParts`: its params are addressed from the parts, a write
//! through its key reaches the cell the node reads, and a fork starts from the
//! values set. The node implements nothing for the session: `add` takes its
//! `IntoNode`. A `published` value of such a node is written through its key and lands at
//! the update, with nothing implemented for the session.

use tutti_buffers::{ChannelLayout, InterleavedMut};
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status, declare_params};
use tutti_midi_runtime::{ClipEvents, ClipEventsKind, TimedMidiEvent};
use tutti_session::{DetachedSession, MainOutput, Session};
use tutti_ump::MidiChannel;
use tutti_units::{Amplitude, Beat, SampleRate};

/// A source of its `level`, on one channel.
#[derive(Clone)]
pub struct Level {
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
        Shape::source_unassigned(ChannelLayout::MONO).param_ports(self.declared_ports())
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

/// A source of how many events its published `events` hold, on one channel.
#[derive(Clone)]
pub struct Count {
    cells: CountCells,
}

declare_params!([] Count, CountConfig, CountCells {
    published { events: ClipEventsKind }
});

impl From<CountConfig> for Count {
    fn from(config: CountConfig) -> Self {
        Self {
            cells: CountCells::from_config(&config),
        }
    }
}

impl Node for Count {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let events = self.cells.events().lend();
        io.output(0).fill(events.len() as f32);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

fn events_of(count: u8) -> ClipEvents {
    (0..count)
        .map(|n| {
            let on = tutti_fixtures::midi::note_on(MidiChannel::FIRST, 60 + n, 0xFFFF);
            TimedMidiEvent::new(Beat::new(f64::from(n)), on).unwrap()
        })
        .collect()
}

fn session_with_count() -> (DetachedSession, tutti_session::Key<Count>, CountCells) {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let node = Count::from(CountConfig::new());
    let probe = node.cells.clone();
    let count = s.graph().add(node);
    s.graph()
        .set_sources(MainOutput::channel(0), [count.output(0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    (s, count, probe)
}

/// **A published value written through its key lands at the update and the
/// node reads it the next block**: the node starts at the value's default,
/// the write is staged until the update, and the block after it renders the
/// new value's length.
#[test]
fn a_published_value_lands_at_the_update_and_the_next_block_reads_it() {
    let (mut s, count, _probe) = session_with_count();
    assert_eq!(frame(&mut s), 0.0, "the default value is empty");

    s.set(count.events().to(events_of(3))).unwrap();
    assert_eq!(frame(&mut s), 0.0, "a write waits for the update");
    let _ = s.update();
    assert_eq!(frame(&mut s), 3.0, "the written value renders next block");
    assert_eq!(s.get(count.events()).unwrap().len(), 3, "and reads back");
}

/// **The update's reclaim sweep frees the value a block held across a
/// replacement**: the node's parts carry the cell as a published entry and no
/// reclaimer, and the session sweeps the entry. A reader of the cell stands in
/// for a block in progress across the publish.
#[test]
fn an_update_frees_a_published_value_a_block_held_across_a_replacement() {
    let (mut s, count, probe) = session_with_count();
    let mut block = probe.clone();
    {
        let _held = block.events().lend();
        s.set(count.events().to(events_of(1))).unwrap();
        let _ = s.update();
    }
    assert_eq!(
        probe.clone().events().publisher().retired_len(),
        1,
        "the first value is retired while the block holds it"
    );
    let _ = s.update();
    assert_eq!(
        probe.clone().events().publisher().retired_len(),
        0,
        "and the sweep frees it once the block has gone"
    );
}

/// **A fork reads the published value as it was set**, and a later write does
/// not reach it.
#[test]
fn a_fork_reads_the_published_value_as_set() {
    let (mut s, count, _probe) = session_with_count();
    s.set(count.events().to(events_of(2))).unwrap();
    let _ = s.update();
    let mut fork = s
        .graph_ref()
        .testing()
        .fork_live()
        .expect("a cells node forks");
    s.set(count.events().to(events_of(5))).unwrap();
    let _ = s.update();
    let mut forked = [0.0f32];
    fork.tick(InterleavedMut::one_frame(&mut forked).unwrap());
    assert_eq!(forked, [2.0]);
}

/// A kind no session addresses.
enum Unaddressed {}

impl tutti_graph::PublishedKind for Unaddressed {
    type Value = u8;
}

/// A source of silence that publishes a value of a kind the session does not
/// know.
#[derive(Clone)]
pub struct Silent {
    cells: SilentCells,
}

declare_params!([] Silent, SilentConfig, SilentCells {
    published { level: Unaddressed }
});

impl From<SilentConfig> for Silent {
    fn from(config: SilentConfig) -> Self {
        Self {
            cells: SilentCells::from_config(&config),
        }
    }
}

impl Node for Silent {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let level = *self.cells.level().lend();
        io.output(0).fill(f32::from(level));
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

/// **A node that publishes a kind the session does not know is added and
/// renders**: the session has nothing that can address the value, and leaves
/// it out.
#[test]
fn a_node_publishing_an_unknown_kind_is_added_and_renders() {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let silent = s.graph().add(Silent::from(SilentConfig::new()));
    s.graph()
        .set_sources(MainOutput::channel(0), [silent.output(0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert_eq!(frame(&mut s), 0.0, "the value starts at its default");
}

/// **An entry of a kind the session does not address is swept all the same**:
/// the update frees the value a block held across a replacement.
#[test]
fn an_update_frees_the_value_of_an_unaddressed_entry_a_block_held() {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let node = Silent::from(SilentConfig::new());
    let mut block = node.cells.clone();
    let publisher = block.level().publisher().clone();
    let silent = s.graph().add(node);
    s.graph()
        .set_sources(MainOutput::channel(0), [silent.output(0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    {
        let _held = block.level().lend();
        publisher.publish(7);
        assert_eq!(publisher.retired_len(), 1, "the first value is held");
    }
    let _ = s.update();
    assert_eq!(publisher.retired_len(), 0, "the sweep freed it");
}
