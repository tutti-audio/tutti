//! Loading a plugin file, as a test does where an app runs the load: a catalog
//! probes the file, the load runs on the main thread, and the node is added.
//!
//! The probe is the repo's `plugin-probe`, built beside the test binaries
//! (`cargo build -p tutti-plugin-probe`).

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Mutex;
use tutti_plugin::catalog::ResolvedHosting;
use tutti_plugin_db::PluginDb;
use tutti_plugin_db::{Catalog, CatalogConfig, Hosting};
use tutti_plugin_types::MainThreadMarker;
use tutti_plugin_types::{Locator, PluginId, PluginRef};
use tutti_rt::LockExt;
use tutti_session::{AnyPlugin, Key, LoadError, LoadInProcess, LoadOutOfProcess};
use tutti_session::{LoadPlugin as Load, PeerState, PluginInstance, Session};

/// A plugin to load: its ref, where it runs, and the state it starts from.
#[derive(Clone, Debug)]
pub struct Spec {
    plugin: PluginRef,
    hosting: Hosting,
    state: Option<PeerState>,
}

impl Spec {
    pub fn new(plugin: PluginRef) -> Self {
        Self {
            plugin,
            hosting: Hosting::Auto,
            state: None,
        }
    }

    #[must_use]
    pub fn hosting(mut self, hosting: Hosting) -> Self {
        self.hosting = hosting;
        self
    }

    /// With `state` restored before the plugin is activated (the entry checks
    /// that it is the plugin's own).
    #[must_use]
    pub fn restoring(mut self, state: PeerState) -> Self {
        self.state = Some(state);
        self
    }
}

/// Probes the plugin file at `path` into `catalog`, waiting for the probe.
///
/// The probe is a process of its own and inherits the environment. A test sets
/// `TUTTI_CLAP_PROBE_*` variables to make the plugin's server fail, render or
/// report in a way the test pins, and the probe must not see them: it would use
/// up a fault meant for the server (the plugin's "once" marker) or report a
/// plugin the test did not mean. So the probe asserts none is set, and a test
/// that sets one [`prime`]s the plugin's file first. Once the probe is done the catalog's
/// writer is joined, so the helper leaves no thread of its own behind.
pub fn probe(catalog: &mut Catalog, path: &std::path::Path) {
    let set: Vec<_> = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| key.starts_with("TUTTI_CLAP_PROBE_"))
        .collect();
    assert!(
        set.is_empty(),
        "probe before the plugin's variables are set (`prime`): {set:?}"
    );
    let task = catalog
        .register(path, tutti_fixtures::executor().downgrade())
        .expect("a plugin file");
    let scanned = tutti_fixtures::block_on(task).expect("the executor lives");
    let result = catalog
        .apply(scanned)
        .expect("the output of this catalog's own scan");
    assert!(result.failures().is_empty(), "{:?}", result.failures());
    catalog.save().run().expect("the catalog's table saves");
}

/// A catalog that probed `plugin`'s file, and the id the ref names in it (the
/// file's first plugin when the ref names none).
pub fn catalog_of(plugin: &PluginRef) -> (Catalog, PluginId, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let config =
        CatalogConfig::new(dir.path()).probe_locator(Locator::at(tutti_fixtures::plugin_probe()));
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));
    probe(&mut catalog, plugin.path());
    let file = tutti_plugin_db::canonical_path(plugin.path());
    let id = catalog
        .entries()
        .filter(|entry| entry.path == file)
        .filter_map(|entry| entry.id.cloned())
        .find(|id| plugin.id().is_none_or(|wanted| wanted == id))
        .expect("the probed file lists the plugin");
    (catalog, id, dir)
}

/// The CLAP id of the plugin `plugin` names: the text a test compares with the
/// reference plugin's constants.
///
/// # Panics
///
/// When the ref names no plugin, or one of another format.
pub fn clap_id(plugin: &PluginRef) -> &str {
    match plugin.id() {
        Some(PluginId::Clap(id)) => id,
        other => panic!("a CLAP plugin: {other:?}"),
    }
}

/// A plugin loaded and in a node, in one call.
pub trait LoadPlugin {
    /// Loads `spec` and adds its node.
    ///
    /// # Errors
    ///
    /// Why the load failed.
    fn load_plugin_blocking(
        &mut self,
        main: MainThreadMarker,
        spec: Spec,
    ) -> Result<Key<AnyPlugin>, LoadError> {
        self.load_plugin_handle(main, spec).map(|(_, node)| node)
    }

    /// As [`load_plugin_blocking`](Self::load_plugin_blocking), with the handle
    /// the node was made from.
    ///
    /// # Errors
    ///
    /// Why the load failed.
    fn load_plugin_handle(
        &mut self,
        main: MainThreadMarker,
        spec: Spec,
    ) -> Result<(PluginInstance, Key<AnyPlugin>), LoadError>;
}

/// A catalog `load_plugin_blocking` loads from: probed once per process, for the
/// file and the id a ref names.
struct Primed {
    file: PathBuf,
    id: Option<PluginId>,
    catalog: Catalog,
    plugin_id: PluginId,
    _dir: tempfile::TempDir,
}

static PRIMED: Mutex<Vec<Primed>> = Mutex::new(Vec::new());

/// Probes `plugin`'s file for the loads of this process. A test that sets a
/// `TUTTI_CLAP_PROBE_*` variable calls it first, before the variable.
pub fn prime(plugin: &PluginRef) {
    let mut primed = PRIMED.lock_recover();
    let file = tutti_plugin_db::canonical_path(plugin.path());
    let id = plugin.id().cloned();
    if !primed.iter().any(|p| p.file == file && p.id == id) {
        let (catalog, plugin_id, dir) = catalog_of(plugin);
        primed.push(Primed {
            file,
            id,
            catalog,
            plugin_id,
            _dir: dir,
        });
    }
}

impl<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>> LoadPlugin for Session<K, C> {
    fn load_plugin_handle(
        &mut self,
        main: MainThreadMarker,
        spec: Spec,
    ) -> Result<(PluginInstance, Key<AnyPlugin>), LoadError> {
        prime(&spec.plugin);
        let primed = PRIMED.lock_recover();
        let file = tutti_plugin_db::canonical_path(spec.plugin.path());
        let id = spec.plugin.id().cloned();
        let found = primed
            .iter()
            .find(|p| p.file == file && p.id == id)
            .expect("primed above");
        self.load_from(
            main,
            &found.catalog,
            &found.plugin_id,
            spec.hosting,
            spec.state,
        )
    }
}

/// What a test does with a plugin of its own catalog: `id` in `catalog`, loaded
/// and in a node.
pub trait LoadFrom {
    /// Loads `id` (hosted as `hosting`, from `state` when given) and adds its
    /// node.
    ///
    /// # Errors
    ///
    /// Why the load failed.
    fn load_from(
        &mut self,
        main: MainThreadMarker,
        catalog: &Catalog,
        id: &PluginId,
        hosting: Hosting,
        state: Option<PeerState>,
    ) -> Result<(PluginInstance, Key<AnyPlugin>), LoadError>;
}

impl<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>> LoadFrom for Session<K, C> {
    fn load_from(
        &mut self,
        main: MainThreadMarker,
        catalog: &Catalog,
        id: &PluginId,
        hosting: Hosting,
        state: Option<PeerState>,
    ) -> Result<(PluginInstance, Key<AnyPlugin>), LoadError> {
        let load = match hosting {
            Hosting::Auto => self.load_plugin(id, catalog),
            Hosting::InProcess => self.load_plugin_as(id, catalog, ResolvedHosting::InProcess),
            Hosting::OutOfProcess => {
                self.load_plugin_as(id, catalog, ResolvedHosting::OutOfProcess)
            }
        };
        let opened = match load? {
            Load::InProcess(mut load) => {
                if let Some(state) = state {
                    load.initial_state(state).expect("a state of the plugin");
                }
                load.run(main)?
            }
            Load::OutOfProcess(mut load) => {
                if let Some(state) = state {
                    load.initial_state(state).expect("a state of the plugin");
                }
                load.block()?
            }
        };
        let handle = opened.instance();
        let node = self.apply(opened).expect("a plugin just loaded enters");
        Ok((handle, node))
    }
}

/// A load of a known hosting, for a test that pins it: the load is built and
/// its variant taken.
pub trait Hosted {
    /// `id` loaded out of process.
    ///
    /// # Panics
    ///
    /// When the load fails to build or resolves in process.
    fn load_out_of_process(&self, id: &PluginId, catalog: &Catalog) -> LoadOutOfProcess;

    /// `id` loaded in process.
    ///
    /// # Panics
    ///
    /// When the load fails to build or does not resolve in process.
    fn load_in_process(&self, id: &PluginId, catalog: &Catalog) -> LoadInProcess;
}

impl<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>> Hosted for Session<K, C> {
    fn load_out_of_process(&self, id: &PluginId, catalog: &Catalog) -> LoadOutOfProcess {
        match self.load_plugin_as(id, catalog, ResolvedHosting::OutOfProcess) {
            Ok(Load::OutOfProcess(load)) => load,
            other => panic!("an out-of-process load: {other:?}"),
        }
    }

    fn load_in_process(&self, id: &PluginId, catalog: &Catalog) -> LoadInProcess {
        match self.load_plugin_as(id, catalog, ResolvedHosting::InProcess) {
            Ok(Load::InProcess(load)) => load,
            other => panic!("an in-process load: {other:?}"),
        }
    }
}

/// An out-of-process load run to its node on the calling thread: the test's
/// wait on the future.
pub trait Block {
    /// Awaits the load.
    ///
    /// # Errors
    ///
    /// Why the load failed.
    fn block(self) -> Result<tutti_session::LoadedPlugin, LoadError>;
}

impl Block for LoadOutOfProcess {
    fn block(self) -> Result<tutti_session::LoadedPlugin, LoadError> {
        tutti_fixtures::block_on(self)
    }
}

/// A plugin's state or preset work as an app runs it for the plugin's hosting:
/// on the calling main thread for a plugin in process, awaited for one in a
/// plugin server.
pub trait Perform {
    /// What the work yields.
    type Output;

    /// Runs the work.
    fn perform(self, main: MainThreadMarker) -> Self::Output;
}

impl Perform for tutti_session::SaveState {
    type Output = Result<PeerState, tutti_plugin_types::StateError>;

    fn perform(self, main: MainThreadMarker) -> Self::Output {
        match self {
            Self::InProcess(save) => save.run(main),
            Self::OutOfProcess(save) => tutti_fixtures::block_on(save),
        }
    }
}

impl Perform for tutti_session::RestoreState {
    type Output = Result<tutti_session::RestoredState, tutti_plugin_types::StateError>;

    fn perform(self, main: MainThreadMarker) -> Self::Output {
        match self {
            Self::InProcess(restore) => restore.run(main),
            Self::OutOfProcess(restore) => tutti_fixtures::block_on(restore),
        }
    }
}

impl Perform for tutti_session::LoadPreset {
    type Output = Result<tutti_session::LoadedPreset, tutti_plugin_types::PresetError>;

    fn perform(self, main: MainThreadMarker) -> Self::Output {
        match self {
            Self::InProcess(load) => load.run(main),
            Self::OutOfProcess(load) => tutti_fixtures::block_on(load),
        }
    }
}
