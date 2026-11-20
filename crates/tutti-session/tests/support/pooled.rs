//! Loading a plugin file through the pool, as a test does where an app declares
//! the entry: a catalog probes the file, the entry is declared under a key of
//! its own, the session updates until it lands, and the node is added.
//!
//! The probe is the repo's `plugin-probe`, built beside the test binaries
//! (`cargo build -p tutti-plugin-probe`).

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::Mutex;
use std::time::Duration;
use tutti_platform::Instant;
use tutti_plugin_db::{Catalog, CatalogConfig, Hosting};
use tutti_plugin_db::{PluginDb, ScanPoll};
use tutti_plugin_types::MainThreadMarker;
use tutti_plugin_types::{Locator, PluginId, PluginRef};
use tutti_rt::LockExt;
use tutti_session::{AnyPlugin, Key, LoadError, PeerState, PoolEvent, Session, SessionEvent};

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
    ) -> Result<Key<AnyPlugin>, LoadError> {
        self.load_plugin_keyed(main, spec).map(|(_, node)| node)
    }

    /// As [`load_plugin_blocking`](Self::load_plugin_blocking), with the pool
    /// key the entry is declared under: what the session's jobs take.
    ///
    /// # Errors
    ///
    /// Why the load failed.
    fn load_plugin_keyed(
        &mut self,
        main: MainThreadMarker,
        spec: Spec,
    ) -> Result<(u64, Key<AnyPlugin>), LoadError>;
}

static KEYS: AtomicU64 = AtomicU64::new(0);

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
    fn load_plugin_keyed(
        &mut self,
        main: MainThreadMarker,
        spec: Spec,
    ) -> Result<(u64, Key<AnyPlugin>), LoadError> {
        prime(&spec.plugin);
        let key = {
            let primed = PRIMED.lock_recover();
            let file = tutti_plugin_db::canonical_path(spec.plugin.path());
            let id = spec.plugin.id().cloned();
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
        self.await_entry(main, key).map(|node| (key, node))
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
        state: Option<PeerState>,
    ) -> Result<Key<AnyPlugin>, LoadError> {
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
        state: Option<PeerState>,
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
    ) -> Result<Key<AnyPlugin>, LoadError>;
}

impl<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>> LoadEntry for Session<K, C> {
    fn declare_entry(
        &mut self,
        main: MainThreadMarker,
        catalog: &Catalog,
        id: &PluginId,
        hosting: Hosting,
        state: Option<PeerState>,
    ) -> u64 {
        let key = KEYS.fetch_add(1, Ordering::Relaxed);
        let _ = self.update_on(main);
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
    ) -> Result<Key<AnyPlugin>, LoadError> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            for event in self.update_on(main) {
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

/// A job that lands in updates on the main thread.
pub trait LandOn<J: tutti_session::JobKind> {
    /// Updates `s` on the main thread until the job lands, and returns its
    /// result: a test thread is not the OS's main thread, so
    /// [`Job::wait`](tutti_session::Job::wait) cannot run a job that waits for
    /// it. The deadline only guards a hang.
    fn land_on<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
        self,
        s: &mut Session<K, C>,
    ) -> Result<J::Output, J::Error>;
}

impl<J: tutti_session::JobKind> LandOn<J> for tutti_session::Job<J> {
    fn land_on<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
        self,
        s: &mut Session<K, C>,
    ) -> Result<J::Output, J::Error> {
        let main = tutti_fixtures::main_thread();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            for event in s.update_on(main) {
                if let SessionEvent::JobDone(mut output) = event
                    && let Some(result) = output.take(&self)
                {
                    return result;
                }
            }
            assert!(Instant::now() < deadline, "the job never landed");
            std::thread::yield_now();
        }
    }
}
