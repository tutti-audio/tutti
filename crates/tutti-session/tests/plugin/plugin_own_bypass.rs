//! A hosted plugin with a bypass of its own is bypassed by the host writing it:
//! `Graph::set_mode` on its node writes the plugin's bypass, `1` for
//! `Bypassed` and `0` for any other mode, once per change. The shared test
//! plugin (CLAP, in a real `plugin-server`) lists its bypass as a flagged
//! parameter; the value the plugin reads back is the witness.
//!
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`); the
//! tests fail without it, by design.

use crate::param_reads::ParamReads as _;

use crate::loading::Perform as _;
use crate::loading::{LoadPlugin, Spec};
use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_plugin::PluginClient;
use tutti_plugin_db::Loadable;
use tutti_plugin_types::PluginRef;
use tutti_session::{ModeTarget, NodeMode, Replacement, Session};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{SampleRate, Samples};

const BLOCK: usize = 512;

fn plugin_file() -> std::path::PathBuf {
    let real = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    tutti_fixtures::publish_with_extension(&real, "clap")
}

fn session_with_plugin() -> (Session, tutti_session::Key<tutti_session::AnyPlugin>) {
    tutti_fixtures::set_env("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let mut s = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .build()
        .expect("an offline session builds");
    let file = plugin_file();
    let plugin = s
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(PluginRef::file(file)),
        )
        .expect("the shared plugin loads");
    let _ = s.update();
    (s, plugin)
}

/// The shared plugin's bypass parameter (`core.rs`, `BYPASS`), `0` processing
/// and `1` bypassed. The test reads it back from the plugin, which holds the
/// last value it was told: it links none of the plugin's code, so its CLAP
/// entry does not collide with the reference plugin's in this binary.
const BYPASS: ParamId = ParamId::new(5);

/// The bypass the plugin holds now.
fn told(s: &Session, node: tutti_graph::NodeKey) -> Option<f64> {
    s.plugin_view(node)
        .expect("hosted")
        .handle()
        .parameter_value(BYPASS)
        .map(Normalized::get)
}

/// Renders blocks until the plugin's last bypass point is `want`, for at most
/// [`HANG_GUARD`](tutti_fixtures::HANG_GUARD).
fn until_told(s: &mut Session, node: tutti_graph::NodeKey, want: f64, what: &str) {
    let mut block = vec![0.0f32; BLOCK * 2];
    tutti_fixtures::wait_until(what, tutti_fixtures::HANG_GUARD, || {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        let _ = s.update();
        told(s, node) == Some(want)
    });
}

/// **Bypassed tells the plugin its bypass is on, and Active tells it off.**
/// The plugin starts processing.
#[test]
fn setting_bypassed_writes_the_plugins_own_bypass_and_active_clears_it() {
    let (mut s, key) = session_with_plugin();
    let node = key.erase();
    assert_eq!(told(&s, node), Some(0.0), "the plugin starts processing");
    s.set(ModeTarget::of(node).to(NodeMode::Bypassed)).unwrap();
    until_told(&mut s, node, 1.0, "bypassed was not told");
    s.set(ModeTarget::of(node).to(NodeMode::Active)).unwrap();
    until_told(&mut s, node, 0.0, "processing was not told");
}

/// **A plugin swapped in under a `Bypassed` node is bypassed.** The node was
/// set `Bypassed` before the crossfade; the incoming plugin is told as it
/// enters, with no further change of mode (a reload after a crash takes the same
/// path), and the mode the plugin plays under is the node's.
#[test]
fn a_plugin_replacing_another_under_a_bypassed_node_is_bypassed_as_it_enters() {
    let (mut s, key) = session_with_plugin();
    let node = key.erase();
    s.set(ModeTarget::of(node).to(NodeMode::Bypassed)).unwrap();
    until_told(&mut s, node, 1.0, "the first plugin is bypassed");

    let outgoing_pid = s
        .plugin_view(node)
        .expect("hosted")
        .diagnostics()
        .server_pid();
    let incoming = PluginClient::new(
        tutti_plugin_ipc::IpcConfig::new(),
        tutti_fixtures::executor(),
        &tutti_fixtures::detached_work(),
        &Loadable::uncatalogued(PluginRef::file(plugin_file())),
        SampleRate::SR_48K,
    )
    .expect("a second instance loads");
    s.graph()
        .replace_plugin(node, Replacement::new(Box::new(incoming)))
        .expect("the crossfade is accepted");
    // The record now holds the incoming plugin's handle, which is what is read.
    let incoming_pid = s
        .plugin_view(node)
        .expect("hosted")
        .diagnostics()
        .server_pid();
    assert_ne!(
        incoming_pid, outgoing_pid,
        "the node's plugin is the new one"
    );
    until_told(&mut s, node, 1.0, "the incoming plugin was not bypassed");
}

/// **A bypass the plugin is moved off is made the node's mode again.** The
/// plugin's bypass is switched on behind the host's back (an editor click or
/// an automation lane would), the state is restored, and the next ticks read it
/// back, find it differs from the `Active` node, write the mode again and count
/// it.
#[test]
fn a_restore_that_leaves_the_bypass_on_under_an_active_node_is_reconciled() {
    let (mut s, key) = session_with_plugin();
    let node = key.erase();
    // Let the install's reconciliation finish, so the session is not already
    // comparing when the plugin is moved.
    let mut block = vec![0.0f32; BLOCK * 2];
    for _ in 0..8 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        let _ = s.update();
    }
    let handle = s.plugin_view(node).expect("hosted").handle().clone();
    handle
        .send_parameter(BYPASS, Normalized::of(1.0))
        .expect("a declared parameter");
    until_told(&mut s, node, 1.0, "the plugin's own bypass went on");
    let main = tutti_fixtures::main_thread();
    let state = s
        .save_state(&key, tutti_plugin_types::StateUse::Project)
        .expect("hosted")
        .perform(main)
        .expect("saves");
    let restored = s
        .restore_state(&key, state)
        .expect("its own plugin's")
        .perform(main)
        .expect("restores");
    s.apply(restored).expect("the node hosts the plugin");
    until_told(
        &mut s,
        node,
        0.0,
        "the session did not make the plugin's bypass the node's mode",
    );
    assert!(
        s.plugin_view(node)
            .expect("hosted")
            .bypass_writes_unapplied()
            >= 1,
        "the correction is counted"
    );
}
