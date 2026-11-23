//! The config the cells form of `declare_params!` writes: generic over the
//! graph float, plain `options` with a default, and the derives an author
//! passes through.

use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Float};
use tutti_graph::{Cx, Io, Node, ParamNode, Prepare, Shape, Status, declare_params};
use tutti_units::{Hz, Mix, UnitParam};

/// A node holding a table of the graph float, so its config and its cells
/// are generic over it.
#[derive(Clone)]
struct Table<F: Float> {
    table: Arc<[F]>,
    cells: TableCells<F>,
}

declare_params!([F: Float] Table<F>, TableConfig<F: Float>, TableCells<F: Float> {
    required { table: Arc<[F]> }
    params {
        Cutoff => cutoff: Hz = Hz::new(1_000.0),
    }
});

impl<F: Float> From<TableConfig<F>> for Table<F> {
    fn from(config: TableConfig<F>) -> Self {
        Self {
            cells: TableCells::from_config(&config),
            table: config.table,
        }
    }
}

impl<F: Float> Node<F> for Table<F> {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_addrs(self.declared_ports().as_slice())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_, F>) -> Status {
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

/// A config generic over the float builds at either width, holds the table it
/// was given, and its cells read back the param's default.
#[test]
fn a_config_generic_over_the_float_builds_at_both_widths_and_reads_back_its_param() {
    let narrow = Table::<f32>::from(TableConfig::new(Arc::from([0.5_f32, 1.0])));
    assert_eq!(&*narrow.table, &[0.5_f32, 1.0]);
    assert_eq!(narrow.param_set().get(UnitParam::Cutoff), Some(1_000.0));

    let wide = Table::<f64>::from(TableConfig::new(Arc::from([0.25_f64])).cutoff(Hz::new(440.0)));
    assert_eq!(&*wide.table, &[0.25_f64]);
    assert_eq!(wide.param_set().get(UnitParam::Cutoff), Some(440.0));
}

/// A node with `options`: plain values with a default, no param.
#[derive(Clone)]
struct Seeded {
    seed: u64,
    voices: u8,
    cells: SeededCells,
}

declare_params!([] Seeded, SeededConfig, SeededCells {
    params {
        Wet => wet: Mix = Mix::DRY,
    }
    options {
        seed: u64 = 0,
        voices: u8 = 32,
    }
});

impl From<SeededConfig> for Seeded {
    fn from(config: SeededConfig) -> Self {
        Self {
            seed: config.seed,
            voices: config.voices,
            cells: SeededCells::from_config(&config),
        }
    }
}

impl Node for Seeded {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_addrs(self.declared_ports().as_slice())
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

/// `Config::new()` holds each option's declared default, and a chained option
/// holds what it was given and leaves the others at their default.
#[test]
fn an_option_holds_its_default_until_its_chained_option_sets_it() {
    let default = Seeded::from(SeededConfig::new());
    assert_eq!((default.seed, default.voices), (0, 32));

    let set = Seeded::from(SeededConfig::new().seed(7));
    assert_eq!((set.seed, set.voices), (7, 32));
}

/// An option is not a param: it has no address, so the node's set holds only
/// the declared param.
#[test]
fn an_option_is_not_a_param() {
    let node = Seeded::from(SeededConfig::new().seed(7).voices(4));
    assert_eq!(node.param_set().params().count(), 1);
}

/// A config with a passed-through `#[derive(Copy, PartialEq)]`.
#[derive(Clone)]
struct Plain {
    cells: PlainCells,
}

declare_params!([] Plain, #[derive(Copy, PartialEq)] PlainConfig, PlainCells {
    params {
        Cutoff => cutoff: Hz = Hz::new(1_000.0),
        Wet => wet: Option<Mix> = None,
    }
    options { seed: u64 = 0 }
});

/// A derive written on the config reaches the struct: `Copy` copies it and
/// `PartialEq` tells two configs apart by every entry.
#[test]
fn a_derive_passed_to_the_config_makes_it_copy_and_comparable() {
    let config = PlainConfig::new().cutoff(Hz::new(500.0));
    let copy = config;
    assert!(config == copy);
    assert!(config != PlainConfig::new());
    assert!(PlainConfig::new() == PlainConfig::new());
    assert!(PlainConfig::new().seed(1) != PlainConfig::new());
    assert!(PlainConfig::new().wet(Some(Mix::WET)) != PlainConfig::new());
}
