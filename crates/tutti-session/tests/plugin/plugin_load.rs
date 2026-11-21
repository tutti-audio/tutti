//! A plugin as a load: `load_plugin(id, &catalog).run(main)` returns a handle,
//! `add_plugin(handle.node())` puts it in the graph, and the plugin closes with
//! the last clone of its handle. The reference CLAP bundle in a real
//! `plugin-server`; the in-process half is `tests/main_thread/plugin_load_main.rs`.
//!
//! Build the server and the probe first: `cargo build -p tutti-plugin-server
//! -p tutti-plugin-probe`.

use std::time::Duration;

use tutti_buffers::{ChannelLayout, InterleavedMut, MaxBlock};
use tutti_platform::Instant;
use tutti_plugin_db::{Catalog, Hosting};
use tutti_plugin_types::{PluginId, PluginRef};
use tutti_session::{
    AddPluginError, LoadError, Peer, PluginCause, Presence, Root, Session, SwapPluginError,
};
use tutti_units::Samples;

use crate::support::clap_probe;

const BLOCK: usize = 512;

fn first_id() -> PluginId {
    PluginId::Clap(
        tutti_clap_test_plugin::PLUGIN_ID
            .to_str()
            .expect("utf-8")
            .into(),
    )
}

fn second_id() -> PluginId {
    PluginId::Clap(
        tutti_clap_test_plugin::SECOND_PLUGIN_ID
            .to_str()
            .expect("utf-8")
            .into(),
    )
}

/// A catalog that scanned the reference bundle (both of its plugins), probed
/// before the plugin's variables are set.
fn catalog() -> (Catalog, tempfile::TempDir) {
    let (catalog, _id, dir) = crate::loading::catalog_of(&PluginRef::file(clap_probe()));
    (catalog, dir)
}

/// An offline session whose served plugins render their closed-form tags
/// (`TagOnly`: output channel `c` of port `p` holds `probe_tag(p, c)`).
///
/// SAFETY: nextest runs each test in its own process, and the variables are
/// set before the first server is spawned, from the test's own thread.
fn session() -> Session {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
        std::env::set_var("TUTTI_CLAP_PROBE_RENDER_MODE", "2");
    });
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .build()
        .expect("an offline session builds")
}

/// Updates and renders a block at a time until `done` holds, the output's
/// channel 0 being its argument.
fn render_until(s: &mut Session, what: &str, mut done: impl FnMut(&[f32]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut block = vec![0.0f32; BLOCK * 2];
    loop {
        let _ = s.update();
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        let left: Vec<f32> = block.iter().step_by(2).copied().collect();
        if done(&left) {
            return;
        }
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// **A load gives a handle whose node renders.** The handle reads the plugin
/// the id names, served and live; the node over it renders the plugin's tag
/// (`probe_tag(0, 0)`, 1) on every frame once the server's first blocks arrive.
#[test]
fn a_load_gives_a_handle_whose_node_renders() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    let plugin = s
        .load_plugin(&second_id(), &catalog)
        .run(tutti_fixtures::main_thread())
        .expect("the reference plugin loads");
    assert_eq!(
        crate::loading::clap_id(plugin.info().plugin()),
        tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap(),
        "the plugin the id names, not the file's first"
    );
    assert_eq!(plugin.info().hosting(), Hosting::OutOfProcess);
    assert!(matches!(plugin.presence(), Presence::Live));
    assert!(!plugin.changed(), "the file is as it was loaded");

    let node = s.add_plugin(plugin.node()).expect("the plugin enters");
    assert!(
        s.plugin_view(node)
            .unwrap()
            .diagnostics()
            .server_pid()
            .is_some(),
        "an out-of-process plugin has its server"
    );
    s.graph().connect(node.out(0), Root::output(0)).unwrap();
    render_until(&mut s, "the plugin's tag", |left| {
        left.iter().all(|&x| x == 1.0)
    });
}

/// **A loaded plugin has one node**: a second node from the same handle is
/// refused, and so is one after the first is removed.
#[test]
fn a_second_node_from_one_handle_is_refused() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    let plugin = s
        .load_plugin(&first_id(), &catalog)
        .run(tutti_fixtures::main_thread())
        .unwrap();
    let node = s.add_plugin(plugin.node()).unwrap();
    assert!(matches!(
        s.add_plugin(plugin.node()),
        Err(AddPluginError::Installed)
    ));
    s.remove(node).unwrap();
    assert!(matches!(
        s.add_plugin(plugin.node()),
        Err(AddPluginError::Installed)
    ));
}

/// **A load that cannot run returns its error, named**: an id the catalog does
/// not hold is `Unresolved`, never a default plugin, and a state of another
/// plugin is refused where the load is built, before anything opens.
#[test]
fn a_failed_load_returns_its_error() {
    let (catalog, _dir) = catalog();
    let s = session();
    let absent = PluginId::Clap("com.example.absent".into());
    let error = s
        .load_plugin(&absent, &catalog)
        .run(tutti_fixtures::main_thread())
        .expect_err("no such plugin");
    assert!(matches!(error, LoadError::Unresolved(_)), "{error:?}");

    let entry = catalog
        .entries()
        .find(|entry| entry.id == Some(&first_id()))
        .expect("the plugin is listed");
    let other = tutti_session::PeerState::of_plugin(entry.plugin_ref(), Vec::new());
    assert!(
        s.load_plugin(&second_id(), &catalog)
            .initial_state(other.clone())
            .is_err()
    );
    assert!(
        s.load_plugin(&first_id(), &catalog)
            .initial_state(other)
            .is_ok()
    );
}

/// **Dropping every clone of a handle releases the plugin**: the server ends
/// with the last clone, not before.
#[cfg(target_os = "linux")]
#[test]
fn dropping_every_clone_reaps_the_server() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    let plugin = s
        .load_plugin(&first_id(), &catalog)
        .run(tutti_fixtures::main_thread())
        .unwrap();
    let pid = plugin.info().server_pid().expect("a served plugin");
    assert!(alive(pid));
    let clone = plugin.clone();
    drop(plugin);
    let _ = s.update();
    assert!(alive(pid), "a clone holds the plugin");
    drop(clone);
    render_until(&mut s, "the server's end", |_| !alive(pid));
}

/// **A node keeps its plugin alive after the app drops its handle**, and the
/// node's removal is what releases it.
#[cfg(target_os = "linux")]
#[test]
fn a_node_keeps_its_plugin_alive_after_the_app_drops_its_handle() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    let plugin = s
        .load_plugin(&first_id(), &catalog)
        .run(tutti_fixtures::main_thread())
        .unwrap();
    let pid = plugin.info().server_pid().expect("a served plugin");
    let node = s.add_plugin(plugin.node()).unwrap();
    s.graph().connect(node.out(0), Root::output(0)).unwrap();
    drop(plugin);
    render_until(&mut s, "the plugin's tag", |left| {
        left.iter().all(|&x| x == 1.0)
    });
    assert!(alive(pid), "the node holds the plugin");

    s.remove(node).unwrap();
    render_until(&mut s, "the server's end", |_| !alive(pid));
}

/// **A plugin that died before it had a node is lost, and refuses one**: the
/// handle reads `Lost`, and neither a node nor a swap takes it.
#[cfg(target_os = "linux")]
#[test]
fn a_plugin_dead_before_its_node_is_lost_and_refuses_one() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    let plugin = s
        .load_plugin(&first_id(), &catalog)
        .run(tutti_fixtures::main_thread())
        .unwrap();
    let pid = plugin.info().server_pid().expect("a served plugin");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    render_until(&mut s, "the loss", |_| !plugin.presence().is_live());
    assert!(matches!(
        plugin.presence(),
        Presence::Lost(PluginCause::Died(_))
    ));
    assert!(matches!(
        s.add_plugin(plugin.node()),
        Err(AddPluginError::Lost)
    ));
    let host = s
        .load_plugin(&first_id(), &catalog)
        .run(tutti_fixtures::main_thread())
        .unwrap();
    let node = s.add_plugin(host.node()).unwrap();
    assert!(matches!(
        s.swap_plugin(node, plugin.node(), tutti_fixtures::main_thread()),
        Err(SwapPluginError::Lost)
    ));
}
