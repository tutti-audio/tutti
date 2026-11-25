//! The `published` block of the cells form of `declare_params!`: one cell per
//! entry, a config option for an entry with a default, the node's own reader,
//! and the parts a session reads (each cell under its kind, which sweeps
//! itself; no reclaimer).

use tutti_buffers::ChannelLayout;
use tutti_graph::{
    Cx, IntoNode, Io, Node, ParamNode, Prepare, PublishedKind, Shape, Status, declare_params,
};

/// A list of numbers the test node publishes.
#[derive(Clone, Debug, Default, PartialEq)]
struct Numbers(Vec<u32>);

/// The kinds of the two values: each one fixes the type it holds.
enum EventsKind {}
enum ChangesKind {}

impl PublishedKind for EventsKind {
    type Value = Numbers;
}

impl PublishedKind for ChangesKind {
    type Value = Numbers;
}

/// A node with a published value that starts at its default and one that
/// starts at a stated value.
#[derive(Clone)]
struct Holder {
    cells: HolderCells,
}

declare_params!([] Holder, HolderConfig, HolderCells {
    published {
        events: EventsKind,
        changes: ChangesKind = Numbers(vec![7]),
    }
});

impl From<HolderConfig> for Holder {
    fn from(config: HolderConfig) -> Self {
        Self {
            cells: HolderCells::from_config(&config),
        }
    }
}

impl Node for Holder {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

fn lent(node: &mut Holder) -> (Numbers, Numbers) {
    let events = node.cells.events().lend().clone();
    let changes = node.cells.changes().lend().clone();
    (events, changes)
}

/// An entry with no default starts at the value's default, an entry with one
/// starts at it, and the config's chained option sets it.
#[test]
fn a_published_entry_starts_at_its_default_and_the_option_sets_it() {
    let mut node = Holder::from(HolderConfig::new());
    assert_eq!(lent(&mut node), (Numbers(vec![]), Numbers(vec![7])));

    let mut node = Holder::from(HolderConfig::new().changes(Numbers(vec![1, 2])));
    assert_eq!(lent(&mut node).1, Numbers(vec![1, 2]));
}

/// The parts carry each cell under its kind, and a write through the cell
/// the parts carry is what the node lends next.
#[test]
fn the_parts_carry_each_cell_under_its_kind_and_a_write_reaches_the_node() {
    let node = Holder::from(HolderConfig::new());
    let probe = node.cells.clone();
    let parts = IntoNode::<f32>::into_parts(node);
    assert!(parts.published[0].cell::<EventsKind>().is_some());
    assert!(parts.published[0].cell::<ChangesKind>().is_none());
    assert!(parts.published[1].cell::<ChangesKind>().is_some());

    let cell = parts.published[0]
        .cell::<EventsKind>()
        .expect("the cell is held as the declared value");
    cell.replace(&Numbers(vec![9]));
    let mut probe = probe;
    assert_eq!(*probe.events().lend(), Numbers(vec![9]));
}

/// Each published entry sweeps its own cell, and the parts hand no reclaimer:
/// the value a block held across a replacement is freed by the entry once the
/// block is done.
#[test]
fn an_entry_frees_what_a_block_held_across_a_replacement() {
    let node = Holder::from(HolderConfig::new());
    let mut block = node.cells.clone();
    let parts = IntoNode::<f32>::into_parts(node);
    assert!(parts.reclaimer.is_none(), "the entries are the sweep");
    let entry = &parts.published[1];
    let cell = entry.cell::<ChangesKind>().expect("the declared value");
    {
        let _held = block.changes().lend();
        cell.replace(&Numbers(vec![3]));
        assert_eq!(entry.reclaim(), 1, "the block still holds the first value");
    }
    assert_eq!(entry.reclaim(), 0, "freed once the block is done");
}

/// A fork starts from the value published at the fork, with cells of its own.
#[test]
fn a_fork_starts_from_the_published_value_and_shares_nothing() {
    let node = Holder::from(HolderConfig::new());
    let mut original = node.clone();
    original
        .cells
        .events()
        .publisher()
        .publish(Numbers(vec![4]));
    let mut fork = original.fork_fresh();
    original
        .cells
        .events()
        .publisher()
        .publish(Numbers(vec![5]));
    assert_eq!(*fork.cells.events().lend(), Numbers(vec![4]));
}
