//! A plugin declared as a node of a patch (`PluginDecl`), the same cases in both
//! hostings with the same figures: `tests/plugin` runs them against a real
//! `plugin-server`, `tests/main_thread` in this process. The shared test plugin
//! (CLAP), whose parameters are the witness: the value the plugin holds is what
//! the host last wrote or an editor last set.
//!
//! **Build `plugin-server` and `plugin-probe` first**
//! (`cargo build -p tutti-plugin-server -p tutti-plugin-probe`).

use crate::loading::{OpenFrom as _, catalog_of};
use crate::param_reads::ParamReads as _;
use tutti_buffers::{ChannelLayout, InterleavedMut, MaxBlock};
use tutti_graph::NodeKey;
use tutti_plugin_db::{Catalog, Hosting};
use tutti_plugin_types::{PluginId, PluginRef};
use tutti_session::{
    LoadedPlugin, MainOutput, Mention, PatchError, Peer as _, PluginDecl, PluginPatchError,
    Presence, Resource as _, Section, Session, WireStart as _,
};
use tutti_units::{Normalized, ParamId, Samples};

/// The shared plugin's `level` parameter, which a host may write.
const LEVEL: ParamId = ParamId::new(0);
/// Its second persistent parameter.
const AUX: ParamId = ParamId::new(1);
/// Its read-only meter.
const METER: ParamId = ParamId::new(4);
/// An id the plugin does not list.
const NOWHERE: ParamId = ParamId::new(99);

const BLOCK: usize = 512;

/// Every case, by name: the runner of each binary names them once.
macro_rules! cases {
    ($run:ident) => {
        $run! {
            redeclaring_the_same_plugin_keeps_its_node,
            declaring_another_plugin_swaps_the_node,
            one_plugin_under_two_keys_is_refused_and_not_taken,
            a_removed_plugin_is_spent,
            a_parameter_the_plugin_does_not_list_is_refused,
            a_read_only_parameter_is_refused,
            a_patch_refused_elsewhere_leaves_the_plugin_for_the_next,
            a_plugin_is_wired_in_the_patch_that_adds_it,
            a_section_that_declares_its_plugin_again_keeps_it,
            an_initial_is_written_once_per_declared_change,
        }
    };
}
pub(crate) use cases;

/// A session, and the catalog its loads resolve from.
struct Rig {
    session: Session,
    catalog: Catalog,
    id: PluginId,
    hosting: Hosting,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new(hosting: Hosting) -> Self {
        if hosting == Hosting::OutOfProcess {
            tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        }
        let real = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
        let plugin = PluginRef::file(tutti_fixtures::publish_with_extension(&real, "clap"));
        let (catalog, id, dir) = catalog_of(&plugin);
        let session = Session::builder()
            .max_block(MaxBlock::of(Samples::new(BLOCK)))
            .build()
            .expect("an offline session builds");
        Self {
            session,
            catalog,
            id,
            hosting,
            _dir: dir,
        }
    }

    /// A plugin loaded, and in no node. Two loads are two plugins.
    fn load(&self) -> LoadedPlugin {
        self.session
            .open_from(
                tutti_fixtures::main_thread(),
                &self.catalog,
                &self.id,
                self.hosting,
                None,
            )
            .expect("the shared plugin loads")
    }

    /// The node of the one plugin the session hosts.
    fn node(&self) -> NodeKey {
        let mut plugins = self.session.plugins();
        let node = plugins.next().expect("a plugin is hosted").node();
        assert!(plugins.next().is_none(), "one plugin is hosted");
        node
    }

    fn hosted(&self) -> usize {
        self.session.plugins().count()
    }

    /// What the plugin at `node` holds of `id`.
    fn held(&self, node: NodeKey, id: ParamId) -> f64 {
        self.session
            .plugin_view(node)
            .expect("hosted")
            .handle()
            .parameter_value(id)
            .expect("the plugin lists the parameter")
            .get()
    }

    /// An edit made in the plugin's own editor: the handle sends the parameter,
    /// and the session's writes do not see it.
    fn edit(&self, node: NodeKey, id: ParamId, value: f64) {
        self.session
            .plugin_view(node)
            .expect("hosted")
            .handle()
            .send_parameter(id, Normalized::of(value))
            .expect("the plugin takes the edit");
    }

    /// Renders and updates until the plugin at `node` holds `want` of `id`, for
    /// at most the hang guard.
    fn until_held(&mut self, node: NodeKey, id: ParamId, want: f64, what: &str) {
        let mut block = vec![0.0f32; BLOCK * 2];
        tutti_fixtures::wait_until(what, tutti_fixtures::HANG_GUARD, || {
            self.session
                .render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
            let _ = self.session.update();
            (self.held(node, id) - want).abs() < 1e-9
        });
    }
}

/// **The same plugin declared again keeps its node**: the key behind the app's
/// key, the instance and the plugin's server are the ones it had, and nothing is
/// reported as changed.
pub(crate) fn redeclaring_the_same_plugin_keeps_its_node(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let loaded = rig.load();
    let instance = loaded.instance();
    rig.session
        .patch(PluginDecl::new("fx", loaded.clone()))
        .unwrap();
    let node = rig.node();
    let server = rig
        .session
        .plugin_view(node)
        .unwrap()
        .diagnostics()
        .server_pid();
    let id = instance.id();

    let patched = rig
        .session
        .patch(PluginDecl::new("fx", loaded.clone()))
        .unwrap();

    assert!(
        patched.rebuilt.is_empty() && patched.readded.is_empty() && patched.removed.is_empty(),
        "{patched:?}"
    );
    assert_eq!(rig.node(), node, "the node behind the key is the same");
    assert_eq!(instance.id(), id);
    assert!(matches!(instance.presence(), Presence::Live));
    assert_eq!(
        rig.session
            .plugin_view(node)
            .unwrap()
            .diagnostics()
            .server_pid(),
        server,
        "the plugin was not loaded again"
    );
}

/// **A different plugin at the same key is a swap**: the old instance reads
/// `Closed`, the new one is live at the same key, and no second node is made.
pub(crate) fn declaring_another_plugin_swaps_the_node(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let (first, second) = (rig.load(), rig.load());
    rig.session
        .patch(PluginDecl::new("fx", first.clone()))
        .unwrap();
    let node = rig.node();

    let patched = rig
        .session
        .patch(PluginDecl::new("fx", second.clone()))
        .unwrap();

    assert_eq!(patched.rebuilt, ["fx"], "{patched:?}");
    assert!(patched.unconverged.is_empty(), "{patched:?}");
    assert_eq!(rig.node(), node, "the swap keeps the key");
    assert!(matches!(first.instance().presence(), Presence::Closed));
    assert!(matches!(second.instance().presence(), Presence::Live));
}

/// **One plugin under two keys is refused with nothing taken**: the graph is as
/// it was, and the plugin is declared by the next patch that is clean.
pub(crate) fn one_plugin_under_two_keys_is_refused_and_not_taken(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let loaded = rig.load();

    let refused = rig
        .session
        .patch((
            PluginDecl::new("a", loaded.clone()),
            PluginDecl::new("b", loaded.clone()),
        ))
        .unwrap_err();

    assert_eq!(
        refused,
        PatchError::Plugin(PluginPatchError::DeclaredTwice {
            first: "a",
            second: "b"
        })
    );
    assert_eq!(rig.hosted(), 0);
    assert!(matches!(loaded.instance().presence(), Presence::Live));
    rig.session.patch(PluginDecl::new("a", loaded)).unwrap();
    assert_eq!(rig.hosted(), 1);
}

/// **A plugin whose node was removed is spent**: declaring it again is refused
/// by name, and nothing is added.
pub(crate) fn a_removed_plugin_is_spent(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let loaded = rig.load();
    rig.session
        .patch(PluginDecl::new("fx", loaded.clone()))
        .unwrap();
    let patched = rig.session.patch(Vec::<PluginDecl<&str>>::new()).unwrap();
    assert_eq!(patched.removed, ["fx"]);
    assert_eq!(rig.hosted(), 0);

    let refused = rig
        .session
        .patch(PluginDecl::new("fx", loaded))
        .unwrap_err();

    assert_eq!(
        refused,
        PatchError::Plugin(PluginPatchError::Spent { key: "fx" })
    );
    assert!(refused.to_string().contains("load the plugin again"));
    assert_eq!(rig.hosted(), 0);
}

/// **A declared parameter the plugin does not list is refused at the patch**,
/// before the plugin is taken.
pub(crate) fn a_parameter_the_plugin_does_not_list_is_refused(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let loaded = rig.load();

    let refused = rig
        .session
        .patch(PluginDecl::new("fx", loaded.clone()).initial(NOWHERE, Normalized::of(0.5)))
        .unwrap_err();

    assert_eq!(
        refused,
        PatchError::Plugin(PluginPatchError::NoSuchParam {
            key: "fx",
            id: NOWHERE
        })
    );
    assert_eq!(rig.hosted(), 0);
    rig.session.patch(PluginDecl::new("fx", loaded)).unwrap();
    assert_eq!(rig.hosted(), 1);
}

/// **A declared parameter the plugin lists read-only is refused at the patch**,
/// before the plugin is taken.
pub(crate) fn a_read_only_parameter_is_refused(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let loaded = rig.load();

    let refused = rig
        .session
        .patch(PluginDecl::new("fx", loaded.clone()).initial(METER, Normalized::of(0.5)))
        .unwrap_err();

    assert_eq!(
        refused,
        PatchError::Plugin(PluginPatchError::ParamReadOnly {
            key: "fx",
            id: METER
        })
    );
    assert_eq!(rig.hosted(), 0);
    assert!(matches!(loaded.instance().presence(), Presence::Live));
}

/// **A patch refused for another part leaves the plugin untaken**: the next
/// clean patch with the same load adds it.
pub(crate) fn a_patch_refused_elsewhere_leaves_the_plugin_for_the_next(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let loaded = rig.load();

    let refused = rig
        .session
        .patch((
            PluginDecl::new("fx", loaded.clone()),
            Mention::new("fx").connect(Mention::new("nowhere")),
        ))
        .unwrap_err();

    assert_eq!(refused, PatchError::Undeclared("nowhere"));
    assert_eq!(rig.hosted(), 0);
    assert!(matches!(loaded.instance().presence(), Presence::Live));
    rig.session.patch(PluginDecl::new("fx", loaded)).unwrap();
    assert_eq!(rig.hosted(), 1);
}

/// **A wire from a declared plugin is checked against the ports its plugin
/// reports and made in the patch that adds the node**, and kept by the next.
pub(crate) fn a_plugin_is_wired_in_the_patch_that_adds_it(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let loaded = rig.load();
    let part = || {
        (
            PluginDecl::new("fx", loaded.clone()),
            Mention::new("fx").connect(MainOutput),
        )
    };

    let added = rig.session.patch(part()).unwrap();
    let kept = rig.session.patch(part()).unwrap();

    assert!(added.dormant.is_empty(), "{added:?}");
    assert!(kept.dormant.is_empty(), "{kept:?}");
    assert_eq!(rig.hosted(), 1);
    let refused = rig
        .session
        .patch((
            PluginDecl::new("fx", loaded.clone()),
            Mention::new("fx").connect(Mention::new("nowhere")),
        ))
        .unwrap_err();
    assert_eq!(refused, PatchError::Undeclared("nowhere"));
}

/// **A section that declares its plugin again keeps it**: the same load is
/// values (no install, the same node), and another load is a swap.
pub(crate) fn a_section_that_declares_its_plugin_again_keeps_it(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let (first, second) = (rig.load(), rig.load());
    let section = |revision: u32, loaded: &LoadedPlugin| {
        let loaded = loaded.clone();
        Section::new("track", revision, move |_| PluginDecl::new("fx", loaded))
    };
    rig.session.patch(section(1, &first)).unwrap();
    let node = rig.node();

    let carried = rig.session.patch(section(1, &first)).unwrap();
    let again = rig.session.patch(section(2, &first)).unwrap();

    for patched in [&carried, &again] {
        assert!(
            patched.rebuilt.is_empty() && patched.readded.is_empty(),
            "{patched:?}"
        );
    }
    assert_eq!(rig.node(), node);
    assert!(matches!(first.instance().presence(), Presence::Live));

    let swapped = rig.session.patch(section(3, &second)).unwrap();

    assert_eq!(swapped.rebuilt, ["fx"], "{swapped:?}");
    assert_eq!(rig.node(), node);
    assert!(matches!(first.instance().presence(), Presence::Closed));
    assert!(matches!(second.instance().presence(), Presence::Live));
}

/// **An initial is written when the declared value changes, and only then**: an
/// edit made in the plugin's editor stands through a patch that declares the
/// value it declared before.
pub(crate) fn an_initial_is_written_once_per_declared_change(hosting: Hosting) {
    let mut rig = Rig::new(hosting);
    let loaded = rig.load();
    let decl = |level: f64, aux: f64| {
        PluginDecl::new("fx", loaded.clone())
            .initial(LEVEL, Normalized::of(level))
            .initial(AUX, Normalized::of(aux))
    };
    rig.session.patch(decl(0.25, 0.5)).unwrap();
    let node = rig.node();
    rig.until_held(node, LEVEL, 0.25, "the declared level reaches the plugin");
    rig.until_held(node, AUX, 0.5, "the declared aux reaches the plugin");

    // The editor moves the level; the same declared values change nothing.
    rig.edit(node, LEVEL, 0.75);
    rig.until_held(node, LEVEL, 0.75, "the editor's edit reaches the plugin");
    rig.session.patch(decl(0.25, 0.5)).unwrap();
    // Writes reach the plugin in the order they were sent: once the aux the
    // next patch changes has arrived, an unwanted write of the level has too.
    rig.session.patch(decl(0.25, 0.625)).unwrap();
    rig.until_held(node, AUX, 0.625, "the changed aux reaches the plugin");
    assert!(
        (rig.held(node, LEVEL) - 0.75).abs() < 1e-9,
        "the editor's level stands: the declared value did not change"
    );

    // A changed declared value is written over the editor's.
    rig.session.patch(decl(0.5, 0.625)).unwrap();
    rig.until_held(node, LEVEL, 0.5, "the changed level reaches the plugin");
}
