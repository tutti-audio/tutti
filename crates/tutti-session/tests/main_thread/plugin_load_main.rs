//! A plugin hosted in process, as a load: the drop of what only the main thread
//! may drop follows the last clone of the handle. The reference CLAP plugin
//! (`Hosting::InProcess`) resolved through a catalog that probed it. The served
//! half is `tests/plugin/plugin_load.rs`.
//!
//! Build the probe first: `cargo build -p tutti-plugin-probe`.

use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use tutti_platform::Instant;

use tutti_clap_test_plugin::{ClapProbe, Site, ThreadAnswer};

use tutti_buffers::MaxBlock;
use tutti_plugin_db::{Catalog, CatalogConfig, Hosting};
use tutti_plugin_db::{PluginDb, ScanPoll};
use tutti_plugin_types::{Locator, PluginId};
use tutti_session::{Peer, Presence, Session};
use tutti_units::Samples;

use crate::loading::Hosted;
use crate::support::clap_probe;

fn probe() -> &'static ClapProbe {
    static PROBE: OnceLock<ClapProbe> = OnceLock::new();
    PROBE.get_or_init(|| {
        ClapProbe::open(
            &tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
            &clap_probe(),
        )
    })
}

/// Where the plugin's last `deactivate` ran: the in-process instance closes
/// there when its last owner drops.
fn deactivation() -> ThreadAnswer {
    probe().thread_capture().sites[Site::Deactivate as usize]
}

fn id() -> PluginId {
    PluginId::Clap(
        tutti_clap_test_plugin::PLUGIN_ID
            .to_str()
            .expect("utf-8")
            .into(),
    )
}

/// A catalog that probed the reference plugin.
fn catalog() -> (Catalog, tempfile::TempDir) {
    catalog_of(&clap_probe())
}

/// A catalog that probed the plugin file at `path`.
fn catalog_of(path: &std::path::Path) -> (Catalog, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let config =
        CatalogConfig::new(dir.path()).probe_locator(Locator::at(tutti_fixtures::plugin_probe()));
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));
    let mut task = catalog.register(path).expect("a plugin file");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match catalog.try_apply(&mut task) {
            ScanPoll::Running => {
                assert!(Instant::now() < deadline, "the probe never ended");
                thread::yield_now();
            }
            ScanPoll::Done(result) => {
                assert!(result.failures().is_empty(), "{:?}", result.failures());
                break;
            }
            ScanPoll::Died => panic!("the probe's worker died"),
        }
    }
    (catalog, dir)
}

fn session() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("an offline session builds")
}

fn load(s: &Session, catalog: &Catalog) -> tutti_session::PluginNode {
    s.load_in_process(&id(), catalog)
        .run(tutti_fixtures::main_thread())
        .expect("the reference plugin loads in process")
}

/// Updates and renders until the plugin has closed, a hang guard its only bound.
fn until_closed(s: &mut Session) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !deactivation().observed {
        assert!(Instant::now() < deadline, "the plugin never closed");
        let _ = s.update();
        let mut out = vec![0.0f32; 512 * 2];
        s.render(
            &mut tutti_buffers::InterleavedMut::new(&mut out, tutti_buffers::ChannelLayout::STEREO)
                .unwrap(),
        );
        thread::yield_now();
    }
}

/// **An in-process load stamps its module, and `changed()` reads it**: the
/// handle is unchanged as loaded, and reads changed once the module's
/// modification time moves.
fn an_in_process_load_stamps_its_module() {
    let scratch = tempfile::TempDir::new().expect("a scratch directory");
    let module = scratch.path().join("Probe.clap");
    std::fs::copy(clap_probe(), &module).unwrap();
    let (catalog, _dir) = catalog_of(&module);
    let s = session();
    let opened = load(&s, &catalog);
    let handle = opened.handle();
    assert_eq!(handle.asset().name(), module.display().to_string());
    assert!(!handle.changed(), "as loaded");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&module)
        .unwrap()
        .set_modified(tutti_platform::SystemTime::now() + Duration::from_secs(3600))
        .unwrap();
    assert!(handle.changed(), "the module was rewritten");
    drop(opened);
    s.close(tutti_fixtures::main_thread());
}

/// **An in-process load resolves in process from the catalog**: `load_plugin`
/// with `Hosting::InProcess` returns the in-process load, which the format's
/// default does not.
fn an_in_process_hosting_resolves_to_an_in_process_load() {
    let (catalog, _dir) = catalog();
    let s = session();
    assert!(matches!(
        s.load_plugin(&id(), &catalog, Hosting::InProcess),
        Ok(tutti_session::LoadPlugin::InProcess(_))
    ));
}

/// **An in-process plugin loads on the main thread** and is a plugin hosted in
/// this process, live, whose node is hosted here.
fn an_in_process_load_is_a_plugin_hosted_in_this_process() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    let opened = load(&s, &catalog);
    let handle = opened.handle();
    assert_eq!(handle.info().hosting(), Hosting::InProcess);
    assert!(matches!(handle.presence(), Presence::Live));
    let node = s.add_plugin(opened).expect("the plugin enters");
    assert!(
        s.plugin_view(node)
            .unwrap()
            .diagnostics()
            .server_pid()
            .is_none(),
        "no server: the plugin is in this process"
    );
    s.close(tutti_fixtures::main_thread());
}

/// **An unadded node dropped off the main thread closes its in-process plugin
/// on the main thread, once the session's main work runs**: the drop leaves the
/// plugin open (its `deactivate` has not run), and the main thread's next
/// update closes it, the plugin saying it ran there. Its handle reads `Closed`.
fn an_unadded_node_dropped_off_the_main_thread_closes_on_the_main_thread() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let mut s = session();
    let opened = load(&s, &catalog);
    let handle = opened.handle();
    thread::spawn(move || drop(opened)).join().unwrap();
    assert!(
        !deactivation().observed,
        "dropped off the main thread, the plugin is still open"
    );
    assert!(matches!(handle.presence(), Presence::Closed));
    let _ = s.update();
    let closed = deactivation();
    assert!(closed.observed, "the main thread's update closed it");
    assert!(closed.is_main, "and the plugin saw the main thread");
    s.close(tutti_fixtures::main_thread());
}

/// **An unadded node dropped on the main thread closes its plugin there at
/// once**, and a clone of its handle holds no plugin.
fn an_unadded_node_dropped_on_the_main_thread_closes_in_place() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let s = session();
    let opened = load(&s, &catalog);
    let handle = opened.handle();
    let clone = handle.clone();
    drop(handle);
    drop(clone);
    assert!(!deactivation().observed, "the node holds the plugin");
    drop(opened);
    let closed = deactivation();
    assert!(closed.observed && closed.is_main, "{closed:?}");
    s.close(tutti_fixtures::main_thread());
}

/// **A node keeps its in-process plugin open after the app drops its handle**,
/// and its removal, on the main thread, is what closes it: the handle reads
/// `Closed` from the removal on.
fn a_node_keeps_an_in_process_plugin_open_after_its_handle_drops() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let mut s = session();
    let opened = load(&s, &catalog);
    let watch = opened.handle();
    let node = s.add_plugin(opened).unwrap();
    let _ = s.update();
    assert!(!deactivation().observed, "the node holds the plugin");
    assert!(matches!(watch.presence(), Presence::Live));
    s.remove(node).unwrap();
    // The removal commits at a render, and the retired plugin closes in the
    // update after.
    until_closed(&mut s);
    let closed = deactivation();
    assert!(closed.observed && closed.is_main, "{closed:?}");
    assert!(matches!(watch.presence(), Presence::Closed));
    s.close(tutti_fixtures::main_thread());
}

/// **A session on another thread adds an in-process plugin**: binding the
/// plugin to a graph node is not main-thread work, so `add_plugin` takes no
/// main marker.
fn a_session_on_another_thread_adds_an_in_process_plugin() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let s = session();
    let opened = load(&s, &catalog);
    let (s, node) = thread::spawn(move || {
        let mut s = s;
        let node = s.add_plugin(opened).expect("added off the main thread");
        (s, node)
    })
    .join()
    .unwrap();
    assert!(s.plugin_view(node).is_ok());
    assert!(!deactivation().observed);
    s.close(tutti_fixtures::main_thread());
}

/// **A plugin loaded by one session is refused by another, handed back, and
/// closes cleanly after the first session is gone.** The refusal names the
/// session; the node returns in the error, and dropping it on the main thread
/// closes the plugin there with nothing stranded.
fn a_plugin_of_another_session_is_refused_and_closes_cleanly() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let a = session();
    let mut b = session();
    let opened = load(&a, &catalog);
    let Err(tutti_session::AddPluginError::OtherSession(returned)) = b.add_plugin(opened) else {
        panic!("another session's plugin must be refused");
    };
    let stranded = tutti_plugin_types::MainWork::stranded();
    a.close(tutti_fixtures::main_thread());
    assert!(!deactivation().observed, "the node still holds it");
    drop(returned);
    let closed = deactivation();
    assert!(closed.observed && closed.is_main, "{closed:?}");
    assert_eq!(tutti_plugin_types::MainWork::stranded(), stranded);
    b.close(tutti_fixtures::main_thread());
}

pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["plugin_load_main";
an_in_process_load_is_a_plugin_hosted_in_this_process,
an_in_process_load_stamps_its_module,
an_in_process_hosting_resolves_to_an_in_process_load,
an_unadded_node_dropped_off_the_main_thread_closes_on_the_main_thread,
an_unadded_node_dropped_on_the_main_thread_closes_in_place,
a_node_keeps_an_in_process_plugin_open_after_its_handle_drops,
a_session_on_another_thread_adds_an_in_process_plugin,
a_plugin_of_another_session_is_refused_and_closes_cleanly,
];
