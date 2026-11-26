//! The `NodeConfig` the cells form of `declare_params!` writes: what is
//! structure (`required`, `options`, whether an optional param is given) and
//! what is a value (a param's or a setting's initial), the initials by
//! address, the node `build` makes, the host's values a build can take, and the
//! identity an `Asset` compares by.

use std::sync::{Arc, OnceLock};

use tutti_buffers::ChannelLayout;
use tutti_graph::{
    Asset, Cx, HostValues, IntoNode, Io, Node, NodeConfig, Prepare, Shape, Status, declare_params,
};
use tutti_rt::{Publisher, atomic_word};
use tutti_units::{Amplitude, Db, Hz, SettingKind, SyncSetting, UnitAddr, UnitParam};

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
pub enum Mode {
    Off,
    On,
}
atomic_word!(Mode { Off, On });

/// A node with one of each: a `required` handle and width, an option, three
/// kinds of param and a setting.
#[derive(Clone)]
pub struct Voiced {
    layout: ChannelLayout,
    cells: VoicedCells,
}

declare_params!([] Voiced, VoicedConfig, VoicedCells {
    required { layout: ChannelLayout, table: Asset<Vec<f32>> }
    params {
        Cutoff => cutoff: Hz = Hz::new(1_000.0),
        Threshold => bands[2]: Db = Db::new(-10.0),
        Volume => volume: Option<Amplitude> = None,
    }
    options { seed: u64 = 0 }
    settings { mode @ SyncSetting: Mode = Mode::Off, shape: Mode = Mode::Off }
});

impl From<VoicedConfig> for Voiced {
    fn from(config: VoicedConfig) -> Self {
        Self {
            layout: config.layout,
            cells: VoicedCells::from_config(&config),
        }
    }
}

impl Node for Voiced {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.layout).param_ports(self.declared_ports())
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

/// One table every `config()` shares, so two of them hold the same asset.
fn config() -> VoicedConfig {
    static TABLE: OnceLock<Asset<Vec<f32>>> = OnceLock::new();
    let table = TABLE.get_or_init(|| Asset::new(vec![0.5_f32]));
    VoicedConfig::new(ChannelLayout::MONO, table.clone())
}

/// A param's or a setting's initial is a value: a config differing only in
/// those makes the same node, and its initials say where.
#[test]
fn a_param_or_setting_initial_is_a_value_not_structure() {
    let base = config();
    let louder = config().cutoff(Hz::new(2_000.0)).mode(Mode::On);
    assert!(base.same_structure(&louder));
    assert!(louder.same_structure(&base));
    assert_ne!(base.initials().params(), louder.initials().params());
    assert_eq!(base.initials().settings(), &[(SettingKind::Sync, 0)]);
    assert_eq!(louder.initials().settings(), &[(SettingKind::Sync, 1)]);
}

/// A `required` value is structure: a different width, or a different asset,
/// is a different node.
#[test]
fn a_required_change_is_structure() {
    let base = config();
    let wider = VoicedConfig::new(ChannelLayout::STEREO, base.table.clone());
    assert!(!base.same_structure(&wider));
    let other_table = VoicedConfig::new(ChannelLayout::MONO, Asset::new(vec![0.5_f32]));
    assert!(!base.same_structure(&other_table));
}

/// An option is structure: it has no cell, so it can change only by a new node.
#[test]
fn an_option_change_is_structure() {
    assert!(!config().same_structure(&config().seed(7)));
    assert!(config().seed(7).same_structure(&config().seed(7)));
}

/// An `Option` param is a port when it has a value, so giving or withdrawing
/// it changes the node's shape; changing the value it has does not.
#[test]
fn giving_or_withdrawing_an_optional_param_is_structure() {
    let given = config().volume(Some(Amplitude::UNITY));
    assert!(!config().same_structure(&given));
    assert!(given.same_structure(&config().volume(Some(Amplitude::new(0.5)))));
}

/// An asset equals itself and its clones, and no other allocation, though its
/// content is equal.
#[test]
fn an_asset_is_equal_by_identity() {
    let asset = Asset::new(vec![1.0_f32, 2.0]);
    assert_eq!(asset, asset.clone());
    let twin = Asset::from(Arc::new(vec![1.0_f32, 2.0]));
    assert_eq!(*asset, *twin);
    assert_ne!(asset, twin);
    assert_eq!(asset, Asset::from(Arc::clone(asset.arc())));
}

/// The initials are the declared params' initial raws, each at its address, a
/// counted kind at each index, an absent optional param left out.
#[test]
fn initials_list_each_declared_param_at_its_address() {
    assert_eq!(
        config().initials().params(),
        [
            (UnitAddr::first(UnitParam::Cutoff), 1_000.0),
            (UnitAddr::new(UnitParam::Threshold, 0), -10.0),
            (UnitAddr::new(UnitParam::Threshold, 1), -10.0),
        ],
    );
    let given = config().volume(Some(Amplitude::new(0.25)));
    assert!(
        given
            .initials()
            .params()
            .contains(&(UnitAddr::first(UnitParam::Volume), 0.25))
    );
}

/// The node `build` makes starts every declared param at the config's initial,
/// read back through its controls.
#[test]
fn a_built_node_starts_its_params_at_the_initials() {
    let config = config()
        .cutoff(Hz::new(440.0))
        .volume(Some(Amplitude::new(0.25)));
    let (_node, controls) = config.build(&HostValues::new()).into_node();
    let initials = config.initials();
    assert_eq!(initials.params().len(), 4);
    assert_eq!(controls.params().count(), initials.params().len());
    for &(addr, value) in initials.params() {
        assert_eq!(controls.get(addr), Some(value));
    }
}

/// The node `build` makes starts its setting at the config's initial word.
#[test]
fn a_built_node_starts_its_setting_at_the_initial() {
    let config = config().mode(Mode::On);
    let node = config.build(&HostValues::new());
    let settings = IntoNode::<f32>::into_parts(node).settings;
    assert_eq!(settings.len(), 1);
    assert_eq!(settings[0].kind(), SettingKind::Sync);
    assert_eq!(settings[0].load::<Mode>(), Some(Mode::On));
}

/// A setting with no kind has no address, so a change cannot be written to a
/// running node: it is structure. A kinded setting stays a value.
#[test]
fn a_setting_with_no_kind_is_structure() {
    assert!(!config().same_structure(&config().shape(Mode::On)));
    assert!(
        config()
            .shape(Mode::On)
            .same_structure(&config().shape(Mode::On))
    );
    assert!(config().same_structure(&config().mode(Mode::On)));
    // It has no address, so the initials list only the kinded one.
    assert_eq!(config().shape(Mode::On).initials().settings().len(), 1);
}

/// A host's value reaches a reader only when provided, and a reader made from
/// the set lends what the host publishes afterwards.
#[test]
fn a_host_value_is_read_through_a_reader_registered_at_build() {
    let none = HostValues::new();
    assert!(none.reader::<u32>().is_none());

    let cell = Publisher::new(1_u32);
    let host = HostValues::new().provide(&cell);
    let mut reader = host.reader::<u32>().expect("provided");
    assert_eq!(*reader.lend(), 1);
    cell.publish(7);
    assert_eq!(*reader.lend(), 7);
    assert!(host.reader::<u64>().is_none());
}

/// The set is keyed by the value's type: two provided types each read their own.
#[test]
fn host_values_of_two_types_do_not_collide() {
    let narrow = Publisher::new(3_u32);
    let wide = Publisher::new(9_u64);
    let host = HostValues::new().provide(&narrow).provide(&wide);
    let mut a = host.reader::<u32>().expect("u32 provided");
    let mut b = host.reader::<u64>().expect("u64 provided");
    assert_eq!(*a.lend(), 3);
    assert_eq!(*b.lend(), 9);
}
