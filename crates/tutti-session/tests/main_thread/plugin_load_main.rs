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
use tutti_plugin_db::PluginDb;
use tutti_plugin_db::{Catalog, CatalogConfig, Hosting};
use tutti_plugin_types::{Locator, PluginId};
use tutti_session::{Peer, Presence, Session};
use tutti_units::Samples;

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
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let config =
        CatalogConfig::new(dir.path()).probe_locator(Locator::at(tutti_fixtures::plugin_probe()));
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));
    let task = catalog.register(clap_probe()).expect("a plugin file");
    let result = catalog
        .apply(task.run())
        .expect("the output of this catalog's own scan");
    assert!(result.failures().is_empty(), "{:?}", result.failures());
    (catalog, dir)
}

fn session() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("an offline session builds")
}

fn load(s: &Session, catalog: &Catalog) -> tutti_session::LoadedPlugin {
    s.load_plugin(&id(), catalog)
        .hosting(Hosting::InProcess)
        .run(tutti_fixtures::main_thread())
        .expect("the reference plugin loads in process")
}

/// **An in-process plugin loads on the main thread** and is a plugin hosted in
/// this process, live, whose node is hosted here.
fn an_in_process_load_is_a_plugin_hosted_in_this_process() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    let plugin = load(&s, &catalog);
    assert_eq!(plugin.info().hosting(), Hosting::InProcess);
    assert!(matches!(plugin.presence(), Presence::Live));
    let node = s.add_plugin(plugin.node()).expect("the plugin enters");
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

/// **A handle dropped off the main thread closes its in-process plugin on the
/// main thread**: the drop leaves the plugin open (its `deactivate` has not
/// run), and the main thread's next update closes it, the plugin saying it ran
/// there.
fn a_handle_dropped_off_the_main_thread_closes_its_plugin_on_the_main_thread() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let mut s = session();
    let plugin = load(&s, &catalog);
    thread::spawn(move || drop(plugin)).join().unwrap();
    assert!(
        !deactivation().observed,
        "dropped off the main thread, the plugin is still open"
    );
    let _ = s.update();
    let closed = deactivation();
    assert!(closed.observed, "the main thread's update closed it");
    assert!(closed.is_main, "and the plugin saw the main thread");
    s.close(tutti_fixtures::main_thread());
}

/// **Dropped on the main thread, the last clone closes its plugin there at
/// once**, and an earlier clone's drop closes nothing.
fn the_last_clone_dropped_on_the_main_thread_closes_in_place() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let s = session();
    let plugin = load(&s, &catalog);
    let clone = plugin.clone();
    drop(plugin);
    assert!(!deactivation().observed, "a clone holds the plugin");
    drop(clone);
    let closed = deactivation();
    assert!(closed.observed && closed.is_main, "{closed:?}");
    s.close(tutti_fixtures::main_thread());
}

/// **A node keeps its in-process plugin open after the app drops its handle**,
/// and its removal, on the main thread, is what closes it.
fn a_node_keeps_an_in_process_plugin_open_after_its_handle_drops() {
    probe().thread_reset();
    let (catalog, _dir) = catalog();
    let mut s = session();
    let plugin = load(&s, &catalog);
    let node = s.add_plugin(plugin.node()).unwrap();
    drop(plugin);
    let _ = s.update();
    assert!(!deactivation().observed, "the node holds the plugin");
    s.remove(node).unwrap();
    // The removal commits at a render, and the retired plugin closes in the
    // update after.
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
    let closed = deactivation();
    assert!(closed.observed && closed.is_main, "{closed:?}");
    s.close(tutti_fixtures::main_thread());
}

pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["plugin_load_main";
an_in_process_load_is_a_plugin_hosted_in_this_process,
a_handle_dropped_off_the_main_thread_closes_its_plugin_on_the_main_thread,
the_last_clone_dropped_on_the_main_thread_closes_in_place,
a_node_keeps_an_in_process_plugin_open_after_its_handle_drops,
];
