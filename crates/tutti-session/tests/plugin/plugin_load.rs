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
        std::thread::yield_now();
    }
}

/// **A load gives a node whose handle observes it, and the node renders.** The
/// handle reads the plugin the id names, served and live; the node over it
/// renders the plugin's tag (`probe_tag(0, 0)`, 1) on every frame once the
/// server's first blocks arrive.
#[test]
fn a_load_gives_a_node_whose_handle_observes_it_and_renders() {
    let (catalog, _dir) = catalog();
    let mut s = session();
    let opened = s
        .load_plugin(&second_id(), &catalog)
        .run(tutti_fixtures::main_thread())
        .expect("the reference plugin loads");
    let plugin = opened.handle();
    assert_eq!(
        crate::loading::clap_id(plugin.info().plugin()),
        tutti_clap_test_plugin::SECOND_PLUGIN_ID.to_str().unwrap(),
        "the plugin the id names, not the file's first"
    );
    assert_eq!(plugin.info().hosting(), Hosting::OutOfProcess);
    assert!(matches!(plugin.presence(), Presence::Live));
    assert!(!plugin.changed(), "the file is as it was loaded");

    let node = s.add_plugin(opened).expect("the plugin enters");
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

/// **A plugin loaded by one session is refused by another and handed back**,
/// by `add_plugin` and by `swap_plugin`: the other session's rate and collector
/// of main-thread work are not the plugin's. The node then enters the session
/// that loaded it.
#[test]
fn a_plugin_of_another_session_is_refused_by_name_and_handed_back() {
    let (catalog, _dir) = catalog();
    let a = session();
    let mut a = a;
    let mut b = session();
    let main = tutti_fixtures::main_thread();
    let opened = a.load_plugin(&first_id(), &catalog).run(main).unwrap();
    let Err(AddPluginError::OtherSession(opened)) = b.add_plugin(opened) else {
        panic!("add to another session");
    };
    let hosted = b.load_plugin(&first_id(), &catalog).run(main).unwrap();
    let at = b.add_plugin(hosted).unwrap();
    let Err(SwapPluginError::OtherSession(opened)) = b.swap_plugin(at, opened, main) else {
        panic!("swap in another session");
    };
    assert!(
        a.add_plugin(opened).is_ok(),
        "the session that loaded it takes it"
    );
}

/// The cases that name the server by its process id.
#[cfg(target_os = "linux")]
mod process {
    use super::*;

    fn alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    /// **Dropping an unadded node reaps the server**, though its handle lives:
    /// the handle holds readings, not the plugin, and reads `Closed`.
    #[test]
    fn dropping_an_unadded_node_reaps_the_server() {
        let (catalog, _dir) = catalog();
        let mut s = session();
        let opened = s
            .load_plugin(&first_id(), &catalog)
            .run(tutti_fixtures::main_thread())
            .unwrap();
        let plugin = opened.handle();
        let pid = plugin.info().server_pid().expect("a served plugin");
        assert!(alive(pid));
        let clone = plugin.clone();
        drop(plugin);
        let _ = s.update();
        assert!(alive(pid), "the node holds the plugin");
        drop(opened);
        render_until(&mut s, "the server's end", |_| !alive(pid));
        assert!(matches!(clone.presence(), Presence::Closed));
    }

    /// **A node keeps its plugin alive after the app drops its handle**, and the
    /// node's removal is what releases it, the handle reading `Closed`.
    #[test]
    fn a_node_keeps_its_plugin_alive_after_the_app_drops_its_handle() {
        let (catalog, _dir) = catalog();
        let mut s = session();
        let opened = s
            .load_plugin(&first_id(), &catalog)
            .run(tutti_fixtures::main_thread())
            .unwrap();
        let watch = opened.handle();
        let pid = watch.info().server_pid().expect("a served plugin");
        let node = s.add_plugin(opened).unwrap();
        s.graph().connect(node.out(0), Root::output(0)).unwrap();
        let kept = watch.clone();
        drop(watch);
        render_until(&mut s, "the plugin's tag", |left| {
            left.iter().all(|&x| x == 1.0)
        });
        assert!(alive(pid), "the node holds the plugin");
        assert!(matches!(kept.presence(), Presence::Live));

        s.remove(node).unwrap();
        render_until(&mut s, "the server's end", |_| !alive(pid));
        assert!(matches!(kept.presence(), Presence::Closed));
    }

    /// **A plugin that died before it had a node is lost, and refuses one**: the
    /// handle reads `Lost`, and neither a node nor a swap takes it.
    #[test]
    fn a_plugin_dead_before_its_node_is_lost_and_refuses_one() {
        let (catalog, _dir) = catalog();
        let mut s = session();
        let main = tutti_fixtures::main_thread();
        let opened = s.load_plugin(&first_id(), &catalog).run(main).unwrap();
        let plugin = opened.handle();
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
        let host = s.load_plugin(&first_id(), &catalog).run(main).unwrap();
        let node = s.add_plugin(host).unwrap();
        let Err(SwapPluginError::Lost(dead)) = s.swap_plugin(node, opened, main) else {
            panic!("a lost plugin is refused by a swap");
        };
        assert!(matches!(s.add_plugin(dead), Err(AddPluginError::Lost)));
    }

    /// **`changed()` reads the plugin's module, not its bundle directory.** A
    /// bundle's directory keeps its modification time when the module inside is
    /// rewritten; the handle stamps the module, so setting the module's time
    /// moves it.
    #[test]
    fn changed_reads_the_module_inside_a_bundle() {
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("plugin-load-bundle.{}", std::process::id()));
        let arch = tutti_plugin_types::bundle::native_arch_subdirs()[0];
        let module_dir = dir.join("Probe.clap/Contents").join(arch);
        std::fs::create_dir_all(&module_dir).unwrap();
        let module = module_dir.join("Probe.so");
        std::fs::copy(clap_probe(), &module).unwrap();
        let (catalog, id, _scratch) =
            crate::loading::catalog_of(&PluginRef::file(dir.join("Probe.clap")));
        let s = session();
        let opened = s
            .load_plugin(&id, &catalog)
            .run(tutti_fixtures::main_thread())
            .expect("the bundle loads");
        let plugin = opened.handle();
        assert_eq!(plugin.asset().name(), module.display().to_string());
        assert!(!plugin.changed(), "as loaded");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&module)
            .unwrap();
        file.set_modified(tutti_platform::SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        assert!(plugin.changed(), "the module was rewritten");
    }
}
