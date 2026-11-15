//! A hosted plugin with a bypass of its own is bypassed by the host writing it:
//! `Graph::set_mode` on its node writes the plugin's bypass, `1` for
//! `Bypassed` and `0` for any other mode, once per change. The shared test
//! plugin (CLAP, in a real `plugin-server`) lists its bypass as a flagged
//! parameter and records what it was told; its transcript is the witness.
//!
//! Its own test binary: the shared plugin's CLAP entry and the reference
//! plugin's cannot be linked into one (`tests/plugin`).
//!
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`); the
//! tests fail without it, by design.

use std::time::{Duration, Instant};

use tutti_buffers::InterleavedMut;
use tutti_plugin::catalog::PluginRef;
use tutti_session::{NodeMode, PluginLoadRequest, Session};
use tutti_shared_plugin::core::BYPASS;
use tutti_shared_plugin::transcript::{Kind, Transcript, find_blob};
use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};

const BLOCK: usize = 512;

fn session_with_plugin() -> (Session, tutti_graph::NodeKey) {
    // SAFETY: nextest runs each test in its own process, and this is set from
    // the test's own thread before the first server is spawned.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let mut s = Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples(BLOCK)))
        .build()
        .expect("an offline session builds");
    let real = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let file = tutti_fixtures::publish_with_extension(&real, "clap");
    let plugin = s
        .add_plugin_blocking(
            tutti_fixtures::main_thread(),
            PluginLoadRequest::new(PluginRef::file(file).expect("a plugin file")),
        )
        .expect("the shared plugin loads");
    let _ = s.update();
    (s, plugin.erase())
}

/// The value of the last bypass point the plugin recorded, if any.
fn told(s: &Session, node: tutti_graph::NodeKey) -> Option<f64> {
    let state = s
        .plugin(node)
        .expect("hosted")
        .state()
        .save()
        .expect("the plugin saves")
        .bytes()
        .to_vec();
    Transcript::parse(&find_blob(&state))
        .events
        .iter()
        .rev()
        .find(|e| e.kind == Kind::Param && e.id == BYPASS)
        .map(|e| e.value)
}

/// Renders blocks until the plugin's last bypass point is `want`.
fn until_told(s: &mut Session, node: tutti_graph::NodeKey, want: f64) -> bool {
    let mut block = vec![0.0f32; BLOCK * 2];
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        let _ = s.update();
        if told(s, node) == Some(want) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// **Bypassed tells the plugin its bypass is on, and Active tells it off.**
/// Nothing is told before the first change.
#[test]
fn setting_bypassed_writes_the_plugins_own_bypass_and_active_clears_it() {
    let (mut s, node) = session_with_plugin();
    assert_eq!(told(&s, node), None, "nothing is told before a change");
    s.graph().set_mode(node, NodeMode::Bypassed).unwrap();
    assert!(until_told(&mut s, node, 1.0), "bypassed was not told");
    s.graph().set_mode(node, NodeMode::Active).unwrap();
    assert!(until_told(&mut s, node, 0.0), "processing was not told");
}
