//! Loading a plugin file through the pool, as a test does where an app declares
//! the entry: a catalog probes the file, the entry is declared under a key of
//! its own, the session updates until it lands, and the node is added.
//!
//! The probe is the repo's `plugin-probe`, built beside the test binaries
//! (`cargo build -p tutti-plugin-probe`).

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use tutti_plugin::catalog::{
    Catalog, CatalogConfig, Hosting, Locator, NO_SCAN_DIRS, PluginId, PluginRef, PluginState,
};
use tutti_plugin_db::{PluginDb, ScanPoll};
use tutti_plugin_types::MainThreadMarker;
use tutti_session::{Key, LoadError, PluginNode, PoolEvent, Session, SessionEvent};

/// A plugin to load: its ref, where it runs, and the state it starts from.
#[derive(Clone, Debug)]
pub struct Spec {
    plugin: PluginRef,
    hosting: Hosting,
    state: Option<PluginState>,
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
    pub fn restoring(mut self, state: PluginState) -> Self {
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
    let mut task = catalog.register(path).expect("a plugin file");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match catalog.try_apply(&mut task) {
            ScanPoll::Running => {
                assert!(Instant::now() < deadline, "the probe never ended");
                std::thread::sleep(Duration::from_millis(2));
            }
            ScanPoll::Done(result) => {
                assert!(result.failures.is_empty(), "{:?}", result.failures);
                break;
            }
            ScanPoll::Died => panic!("the probe's worker died"),
        }
    }
    drop(task);
    // The catalog writes its table on a thread of its own when a probe ends;
    // `save` waits for it.
    catalog.save().expect("the catalog's table saves");
}

/// A catalog that probed `plugin`'s file, and the id the ref names in it (the
/// file's first plugin when the ref names none).
pub fn catalog_of(plugin: &PluginRef) -> (Catalog, PluginId, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let config = CatalogConfig::new(dir.path(), NO_SCAN_DIRS)
        .probe_locator(Locator::at(tutti_fixtures::plugin_probe()));
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));
    probe(&mut catalog, plugin.path());
    let file = tutti_plugin_db::canonical_path(plugin.path());
    let id = catalog
        .entries()
        .filter(|entry| entry.path == file)
        .map(|entry| entry.descriptor.id.clone())
        .find(|id| plugin.id().is_none_or(|wanted| wanted == id))
        .expect("the probed file lists the plugin");
    let id = PluginId::new(plugin.format(), id);
    (catalog, id, dir)
}

/// A plugin loaded through the pool and in a node, in one call.
pub trait LoadPlugin {
    /// Loads `spec` through the pool and adds its node.
    ///
    /// # Errors
    ///
    /// Why the load failed.
    fn load_plugin_blocking(
        &mut self,
        main: MainThreadMarker,
        spec: Spec,
    ) -> Result<Key<PluginNode>, LoadError>;
}

static KEYS: AtomicU64 = AtomicU64::new(0);

/// A catalog `load_plugin_blocking` loads from: probed once per process, for the
/// file and the id a ref names.
struct Primed {
    file: PathBuf,
    id: Option<String>,
    catalog: Catalog,
    plugin_id: PluginId,
    _dir: tempfile::TempDir,
}

static PRIMED: Mutex<Vec<Primed>> = Mutex::new(Vec::new());

/// Probes `plugin`'s file for the loads of this process. A test that sets a
/// `TUTTI_CLAP_PROBE_*` variable calls it first, before the variable.
pub fn prime(plugin: &PluginRef) {
    let mut primed = PRIMED.lock().unwrap_or_else(PoisonError::into_inner);
    let file = tutti_plugin_db::canonical_path(plugin.path());
    let id = plugin.id().map(str::to_owned);
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

impl<C: tutti_session::Clock> LoadPlugin for Session<C> {
    fn load_plugin_blocking(
        &mut self,
        main: MainThreadMarker,
        spec: Spec,
    ) -> Result<Key<PluginNode>, LoadError> {
        prime(&spec.plugin);
        let key = {
            let primed = PRIMED.lock().unwrap_or_else(PoisonError::into_inner);
            let file = tutti_plugin_db::canonical_path(spec.plugin.path());
            let id = spec.plugin.id().map(str::to_owned);
            let found = primed
                .iter()
                .find(|p| p.file == file && p.id == id)
                .expect("primed above");
            self.declare_entry(
                main,
                &found.catalog,
                &found.plugin_id,
                spec.hosting,
                spec.state,
            )
        };
        self.await_entry(main, key)
    }
}

/// What a test does with an entry it declares itself: the entry of `id` in
/// `catalog`, loaded and in a node.
pub trait LoadEntry {
    /// Declares the entry under a key of its own, updates until it lands, and
    /// adds its node.
    ///
    /// # Errors
    ///
    /// Why the load failed.
    fn load_entry_blocking(
        &mut self,
        main: MainThreadMarker,
        catalog: &Catalog,
        id: &PluginId,
        hosting: Hosting,
        state: Option<PluginState>,
    ) -> Result<Key<PluginNode>, LoadError> {
        let key = self.declare_entry(main, catalog, id, hosting, state);
        self.await_entry(main, key)
    }

    /// Declares the entry under a key of its own, after an update on the main
    /// thread (an in-process load's main stage waits for one).
    fn declare_entry(
        &mut self,
        main: MainThreadMarker,
        catalog: &Catalog,
        id: &PluginId,
        hosting: Hosting,
        state: Option<PluginState>,
    ) -> u64;

    /// Updates until the entry under `key` lands, and adds its node.
    ///
    /// # Errors
    ///
    /// Why the load failed.
    fn await_entry(
        &mut self,
        main: MainThreadMarker,
        key: u64,
    ) -> Result<Key<PluginNode>, LoadError>;
}

impl<C: tutti_session::Clock> LoadEntry for Session<C> {
    fn declare_entry(
        &mut self,
        main: MainThreadMarker,
        catalog: &Catalog,
        id: &PluginId,
        hosting: Hosting,
        state: Option<PluginState>,
    ) -> u64 {
        let key = KEYS.fetch_add(1, Ordering::Relaxed);
        let _ = self.main(main).update();
        let mut declared = self.plugin_entry(key, id, catalog).hosting(hosting);
        if let Some(state) = state {
            declared = declared
                .initial_state(state)
                .expect("a state of the plugin");
        }
        self.pool(declared).expect("a pool the session takes");
        key
    }

    fn await_entry(
        &mut self,
        main: MainThreadMarker,
        key: u64,
    ) -> Result<Key<PluginNode>, LoadError> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            for event in self.main(main).update() {
                let SessionEvent::Pool { key: k, event } = event else {
                    continue;
                };
                if k.get::<u64>() != Some(&key) {
                    continue;
                }
                match event {
                    PoolEvent::Failed(error) => return Err(error),
                    PoolEvent::Loaded => {
                        return Ok(self.add_pooled_plugin(&key).expect("the entry just loaded"));
                    }
                    _ => {}
                }
            }
            assert!(Instant::now() < deadline, "the load never landed");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
