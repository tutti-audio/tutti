//! Loading a plugin file through the pool, as a test does where an app declares
//! the entry: a catalog probes the file, the entry is declared under a key of
//! its own, the session updates until it lands, and the node is added.
//!
//! The probe is the repo's `plugin-probe`, built beside the test binaries
//! (`cargo build -p tutti-plugin-probe`).

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tutti_plugin::catalog::{
    Catalog, CatalogConfig, Hosting, Locator, NO_SCAN_DIRS, PluginId, PluginRef, PluginState,
};
use tutti_plugin::error::StateError;
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

    /// The state the plugin starts from, if one was given.
    pub fn state(&self) -> Option<&PluginState> {
        self.state.as_ref()
    }

    #[must_use]
    pub fn hosting(mut self, hosting: Hosting) -> Self {
        self.hosting = hosting;
        self
    }

    /// With `state` restored before the plugin is activated.
    ///
    /// # Errors
    ///
    /// `StateError::OtherPlugin` when the state was saved from another plugin.
    pub fn restoring(mut self, state: PluginState) -> Result<Self, StateError> {
        state.check_for(&self.plugin)?;
        self.state = Some(state);
        Ok(self)
    }
}

/// Probes the plugin file at `path` into `catalog`, waiting for the probe.
///
/// The probe is a process of its own and inherits the environment, which a test
/// uses to inject faults into the plugin's server (`TUTTI_CLAP_PROBE_*`): those
/// are taken out for the probe's life, so the probe does not use up a fault
/// meant for the server.
pub fn probe(catalog: &mut Catalog, path: &std::path::Path) {
    let faults: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
        .filter(|(key, _)| key.to_string_lossy().starts_with("TUTTI_CLAP_PROBE_"))
        .collect();
    for (key, _) in &faults {
        // SAFETY: nextest runs each test in its own process, and this runs on the
        // test's own thread while no load of its own is running.
        unsafe { std::env::remove_var(key) };
    }
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
    for (key, value) in faults {
        // SAFETY: as above.
        unsafe { std::env::set_var(key, value) };
    }
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

/// What `Session::add_plugin_blocking` was: the entry, loaded and in a node.
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

impl LoadPlugin for Session {
    fn load_plugin_blocking(
        &mut self,
        main: MainThreadMarker,
        spec: Spec,
    ) -> Result<Key<PluginNode>, LoadError> {
        let (catalog, id, _dir) = catalog_of(&spec.plugin);
        self.load_entry_blocking(main, &catalog, &id, spec.hosting, spec.state)
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
    ) -> Result<Key<PluginNode>, LoadError>;
}

impl LoadEntry for Session {
    fn load_entry_blocking(
        &mut self,
        main: MainThreadMarker,
        catalog: &Catalog,
        id: &PluginId,
        hosting: Hosting,
        state: Option<PluginState>,
    ) -> Result<Key<PluginNode>, LoadError> {
        let key = KEYS.fetch_add(1, Ordering::Relaxed);
        // An update on the main thread first: an in-process load's main stage
        // waits for one.
        let _ = self.main(main).update();
        let mut declared = self.plugin_entry(key, id, catalog).hosting(hosting);
        if let Some(state) = state {
            declared = declared
                .initial_state(state)
                .expect("a state of the plugin");
        }
        self.pool(declared).expect("a pool the session takes");
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
