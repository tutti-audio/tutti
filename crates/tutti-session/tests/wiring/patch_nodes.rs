//! A patch's nodes: what `Session::patch` does to the keys an earlier patch
//! declared. A key kept with an equal declaration is untouched, a changed
//! initial is written (and a later `set` wins over it), a changed structure
//! swaps the node under its key, a changed config type removes and adds, and a
//! key left out is removed while a node `Graph::add` made is never touched.

use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, Fade, Io, Node, Prepare, Shape, Status, declare_params};
use tutti_rt::atomic_word;
use tutti_session::{
    ChangedBy, Declarations, DetachedSession, KeyError, NodeDecl, PatchError, ReplaceRefused,
    Session,
};
use tutti_units::{Hz, SampleRate, Samples, SyncSetting};

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
pub enum Mode {
    Off,
    On,
}
atomic_word!(Mode { Off, On });

/// A silent source with a cutoff, a kinded setting, and an option and a width
/// that are structure.
#[derive(Clone)]
pub struct Tone {
    layout: ChannelLayout,
    cells: ToneCells,
}

declare_params!([] Tone, ToneConfig, ToneCells {
    required { layout: ChannelLayout }
    params { Cutoff => cutoff: Hz = Hz::new(1_000.0) }
    options { seed: u64 = 0 }
    settings { mode @ SyncSetting: Mode = Mode::Off }
});

impl From<ToneConfig> for Tone {
    fn from(config: ToneConfig) -> Self {
        Self {
            layout: config.layout,
            cells: ToneCells::from_config(&config),
        }
    }
}

impl Node for Tone {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.layout).param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

/// Another node type with a cutoff at the address `Tone`'s has.
#[derive(Clone)]
pub struct Hum {
    cells: HumCells,
}

declare_params!([] Hum, HumConfig, HumCells {
    params { Cutoff => cutoff: Hz = Hz::new(1_000.0) }
});

impl From<HumConfig> for Hum {
    fn from(config: HumConfig) -> Self {
        Self {
            cells: HumCells::from_config(&config),
        }
    }
}

impl Node for Hum {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

fn session() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

fn tone() -> ToneConfig {
    ToneConfig::new(ChannelLayout::MONO)
}

fn declare(s: &mut DetachedSession, config: ToneConfig) -> tutti_session::Patched<&'static str> {
    s.patch(NodeDecl::new("a", config)).expect("patches")
}

/// **A key declared again with an equal config changes nothing**: the node and
/// the value set on it stay, and nothing is reported.
#[test]
fn a_redeclaration_with_an_equal_config_changes_nothing() {
    let mut s = session();
    declare(&mut s, tone());
    let key = s.key::<ToneConfig>(&"a").expect("declared");
    s.set(key.cutoff().to(Hz::new(800.0))).unwrap();

    let patched = declare(&mut s, tone());

    assert_eq!(s.key::<ToneConfig>(&"a").unwrap().erase(), key.erase());
    assert_eq!(s.get(key.cutoff()).unwrap(), Hz::new(800.0));
    assert!(patched.rebuilt.is_empty() && patched.removed.is_empty());
    assert!(patched.readded.is_empty() && patched.unconverged.is_empty());
}

/// **A changed initial is written to the running node, and a later `set` wins
/// over it**; the node is kept.
#[test]
fn a_changed_initial_is_written_and_a_later_set_wins() {
    let mut s = session();
    declare(&mut s, tone());
    let key = s.key::<ToneConfig>(&"a").unwrap();

    let patched = declare(&mut s, tone().cutoff(Hz::new(2_000.0)));
    assert_eq!(s.get(key.cutoff()).unwrap(), Hz::new(2_000.0));
    assert!(patched.rebuilt.is_empty(), "a value is no rebuild");
    assert_eq!(s.key::<ToneConfig>(&"a").unwrap().erase(), key.erase());

    s.set(key.cutoff().to(Hz::new(500.0))).unwrap();
    assert_eq!(s.get(key.cutoff()).unwrap(), Hz::new(500.0));
}

/// **An initial equal to the last declared one leaves a value that was set**,
/// though it differs from the set value.
#[test]
fn an_unchanged_initial_leaves_a_set_value() {
    let mut s = session();
    declare(&mut s, tone().cutoff(Hz::new(2_000.0)));
    let key = s.key::<ToneConfig>(&"a").unwrap();
    s.set(key.cutoff().to(Hz::new(500.0))).unwrap();

    declare(&mut s, tone().cutoff(Hz::new(2_000.0)));

    assert_eq!(s.get(key.cutoff()).unwrap(), Hz::new(500.0));
}

/// **A kinded setting's changed initial is written** like a param's.
#[test]
fn a_changed_setting_initial_is_written() {
    let mut s = session();
    declare(&mut s, tone());
    let key = s.key::<ToneConfig>(&"a").unwrap();
    assert_eq!(s.get(key.sync()).unwrap(), Mode::Off);

    declare(&mut s, tone().mode(Mode::On));

    assert_eq!(s.get(key.sync()).unwrap(), Mode::On);
}

/// **A changed structure swaps the node under its key**: the key names the
/// incoming node, a value set on the outgoing one carries over, and the key is
/// in `rebuilt`.
#[test]
fn a_structure_change_rebuilds_the_node_under_its_key() {
    let mut s = session();
    declare(&mut s, tone());
    let key = s.key::<ToneConfig>(&"a").unwrap();
    s.set(key.cutoff().to(Hz::new(800.0))).unwrap();

    let patched = declare(&mut s, tone().seed(7));

    assert_eq!(patched.rebuilt, ["a"]);
    assert!(patched.unconverged.is_empty() && patched.readded.is_empty());
    assert_eq!(s.key::<ToneConfig>(&"a").unwrap().erase(), key.erase());
    assert_eq!(s.get(key.cutoff()).unwrap(), Hz::new(800.0));
}

/// **A swap across another shape is refused under a fade and taken when the
/// declaration is unfaded**: the refused key keeps its node and is in
/// `unconverged`, and a zero-length fade is the unfaded swap.
#[test]
fn a_shape_change_is_unconverged_under_a_fade_and_rebuilt_unfaded() {
    let mut s = session();
    declare(&mut s, tone());
    // Committed: a node added since the last commit has nothing to fade from.
    let _ = s.update();
    let wider = || ToneConfig::new(ChannelLayout::STEREO);

    let refused = declare(&mut s, wider());
    assert!(refused.rebuilt.is_empty());
    assert!(matches!(
        refused.unconverged.as_slice(),
        [("a", ReplaceRefused::ShapeChanged { .. })]
    ));

    let cut = s.patch(NodeDecl::new("a", wider()).unfaded()).unwrap();
    assert_eq!(cut.rebuilt, ["a"]);
    assert!(cut.unconverged.is_empty());
    let _ = s.update();

    let zero = Fade::new(Samples::ZERO);
    let back = s.patch(NodeDecl::new("a", tone()).fade(zero)).unwrap();
    assert_eq!(back.rebuilt, ["a"]);
}

/// **A config of another type removes the node and adds a new one**: the key
/// names a node of the new type, in `readded`.
#[test]
fn a_type_change_removes_and_adds() {
    let mut s = session();
    declare(&mut s, tone());
    let before = s.key::<ToneConfig>(&"a").unwrap().erase();

    let patched = s.patch(NodeDecl::new("a", HumConfig::new())).unwrap();

    assert_eq!(patched.readded, ["a"]);
    assert!(patched.rebuilt.is_empty());
    assert!(s.key::<ToneConfig>(&"a").is_err());
    let after = s.key::<HumConfig>(&"a").unwrap().erase();
    assert_ne!(after, before);
    assert!(!s.graph_ref().contains(before));
}

/// **A key dropped from the patch is removed**, in `removed`.
#[test]
fn a_key_dropped_from_the_patch_is_removed() {
    let mut s = session();
    s.patch((NodeDecl::new("a", tone()), NodeDecl::new("b", tone())))
        .unwrap();
    let a = s.key::<ToneConfig>(&"a").unwrap().erase();
    let b = s.key::<ToneConfig>(&"b").unwrap().erase();

    let patched = s.patch(NodeDecl::new("b", tone())).unwrap();

    assert_eq!(patched.removed, ["a"]);
    assert!(!s.graph_ref().contains(a));
    assert!(s.graph_ref().contains(b));
    assert!(s.key::<ToneConfig>(&"a").is_err());
}

/// **A node `Graph::add` made is not declared**: a patch that declares other
/// keys, or none, leaves it.
#[test]
fn a_node_added_by_the_graph_survives_a_patch() {
    let mut s = session();
    let by_hand = s.graph().add(Tone::from(tone())).erase();
    declare(&mut s, tone());

    let patched = s.patch(Declarations::<&'static str>::default()).unwrap();

    assert_eq!(patched.removed, ["a"]);
    assert!(s.graph_ref().contains(by_hand));
}

/// **A key declared twice in one patch is refused, naming it**, and nothing
/// changes.
#[test]
fn a_duplicate_key_is_a_patch_error_naming_it() {
    let mut s = session();
    declare(&mut s, tone());
    let nodes = s.graph_ref().node_count();

    let refused = s.patch((
        NodeDecl::new("b", tone()),
        NodeDecl::new("c", tone()),
        NodeDecl::new("b", tone()),
    ));

    assert_eq!(refused.unwrap_err(), PatchError::DuplicateKey("b"));
    assert_eq!(s.graph_ref().node_count(), nodes);
    assert!(s.key::<ToneConfig>(&"b").is_err());
}

/// **Every container declares its parts**: a tuple, an array, a `Vec`, an
/// `Option`, a collected `Declarations` and an extended one each add their keys.
#[test]
fn every_container_declares_its_parts() {
    let mut s = session();
    let mut extended = Declarations::<&'static str>::default();
    extended.extend([NodeDecl::new("extended", tone())]);
    let patched = s.patch((
        NodeDecl::new("tuple", tone()),
        [NodeDecl::new("array", tone())],
        vec![NodeDecl::new("vec", tone())],
        Some(NodeDecl::new("some", tone())),
        None::<NodeDecl<&'static str, ToneConfig>>,
        ["collected"]
            .into_iter()
            .map(|k| NodeDecl::new(k, tone()))
            .collect::<Declarations<_>>(),
        extended,
    ));
    patched.unwrap();
    for key in ["tuple", "array", "vec", "some", "collected", "extended"] {
        assert!(s.key::<ToneConfig>(&key).is_ok(), "{key}");
    }
}

/// **A changed initial reaches a watcher as an app change**: the patch's write
/// is told to the feed like a `set`, with the value it landed.
#[test]
fn a_changed_initial_is_seen_by_a_watcher() {
    let mut s = session();
    declare(&mut s, tone());
    let key = s.key::<ToneConfig>(&"a").unwrap();
    let mut watcher = s.watch(key.cutoff()).unwrap();
    assert!(watcher.changes().unwrap().is_none());

    declare(&mut s, tone().cutoff(Hz::new(2_000.0)));

    let change = watcher
        .changes()
        .unwrap()
        .expect("the patch moved the base");
    assert_eq!(change.value, Hz::new(2_000.0));
    assert_eq!(change.by, ChangedBy::App);
}

/// **One patch's changed initials are one gesture**, across keys.
#[test]
fn a_patchs_changed_initials_are_one_gesture() {
    let mut s = session();
    s.patch((NodeDecl::new("a", tone()), NodeDecl::new("b", tone())))
        .unwrap();
    let a = s.key::<ToneConfig>(&"a").unwrap();
    let b = s.key::<ToneConfig>(&"b").unwrap();
    let mut watcher = s.watch((a.cutoff(), b.cutoff())).unwrap();

    s.patch((
        NodeDecl::new("a", tone().cutoff(Hz::new(300.0))),
        NodeDecl::new("b", tone().cutoff(Hz::new(400.0))),
    ))
    .unwrap();

    let (a, b) = watcher.changes().unwrap();
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!((a.value, b.value), (Hz::new(300.0), Hz::new(400.0)));
    assert_eq!(a.gesture.unwrap().id, b.gesture.unwrap().id);
}

/// **A rebuild takes an initial the patch changed, and keeps a set value
/// otherwise**: structure and initial changed together read the new initial.
#[test]
fn a_rebuild_with_a_changed_initial_reads_the_new_initial() {
    let mut s = session();
    declare(&mut s, tone());
    let key = s.key::<ToneConfig>(&"a").unwrap();
    s.set(key.cutoff().to(Hz::new(800.0))).unwrap();
    let _ = s.update();

    let patched = declare(&mut s, tone().seed(7).cutoff(Hz::new(2_000.0)));

    assert_eq!(patched.rebuilt, ["a"]);
    assert_eq!(s.get(key.cutoff()).unwrap(), Hz::new(2_000.0));
}

/// **`key` names why it found no node**: undeclared, declared with another
/// config, or its node removed by hand.
#[test]
fn a_key_error_names_its_cause() {
    let mut s = session();
    assert_eq!(
        s.key::<ToneConfig>(&"a").unwrap_err(),
        KeyError::Undeclared("a")
    );
    declare(&mut s, tone());
    assert_eq!(
        s.key::<HumConfig>(&"a").unwrap_err(),
        KeyError::OtherConfig("a")
    );
    let key = s.key::<ToneConfig>(&"a").unwrap();
    let _ = s.remove(key).unwrap();
    assert_eq!(s.key::<ToneConfig>(&"a").unwrap_err(), KeyError::Gone("a"));
}

/// **An initial that is not finite is refused naming its key**, before
/// anything changes.
#[test]
fn a_non_finite_initial_is_a_patch_error_naming_its_key() {
    let mut s = session();
    let nodes = s.graph_ref().node_count();

    let refused = s.patch((
        NodeDecl::new("ok", tone()),
        NodeDecl::new("bad", tone().cutoff(Hz::new(f64::NAN))),
    ));

    assert_eq!(refused.unwrap_err(), PatchError::NotFinite("bad"));
    assert_eq!(s.graph_ref().node_count(), nodes);
}
