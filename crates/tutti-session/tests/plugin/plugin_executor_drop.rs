//! A served plugin's bridge lanes end with the session's executor: a lane that a
//! running render feeds without a break is stopped by the session's drop, which
//! completes (the drop joins the executor's threads), over the reference CLAP
//! plugin in a real `plugin-server`.
//!
//! **Build `plugin-server` first** (`cargo build -p tutti-plugin-server`).

use crate::loading::{LoadPlugin, Spec};
use crate::support::clap_probe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use tutti_buffers::ChannelLayout;
use tutti_engine::{ManualStreamDriver, OutputSpec};
use tutti_plugin_types::PluginRef;
use tutti_session::{MainOutput, OutputPort, Session};
use tutti_units::{SampleRate, Samples};

/// A hang guard: far past anything a drop takes, asserting completion and never speed.
const HANG_GUARD: Duration = Duration::from_secs(60);

/// **A session's drop ends a served plugin's lanes while a render feeds them.**
/// The render thread pulls block after block through the plugin, so the audio
/// lane is answering blocks when the session (which holds its executor) drops;
/// the drop must finish.
#[test]
fn a_session_drop_ends_the_lanes_of_a_plugin_a_render_feeds() {
    // SAFETY: nextest runs each test in its own process, and the server's path
    // is set before the first server is spawned, from the test's own thread.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let rendering = Arc::new(AtomicBool::new(true));
    let (dropped_tx, dropped_rx) = mpsc::channel();
    // A session is not `Send`: its whole life is on this thread, so the test
    // waits for the drop from outside, behind the hang guard.
    let session_thread = {
        let rendering = Arc::clone(&rendering);
        std::thread::spawn(move || {
            let (driver, stream) = ManualStreamDriver::new();
            let mut session = Session::builder()
                .device(tutti_cpal::DeviceSelector::Default)
                .build_device_on(
                    OutputSpec::new(
                        SampleRate::SR_48K,
                        ChannelLayout::STEREO,
                        tutti_buffers::SampleFormat::F32,
                    )
                    .with_quantum(tutti_engine::OutputQuantum::of(256)),
                    driver,
                )
                .expect("builds with no device");
            let node = session
                .load_plugin_blocking(
                    tutti_fixtures::main_thread(),
                    Spec::new(PluginRef::file(clap_probe())),
                )
                .expect("the reference CLAP plugin loads");
            session
                .graph()
                .set_sources(MainOutput::channel(0), [OutputPort::new(node, 0)])
                .unwrap();
            let _ = session.update();

            let rendered = Arc::new(AtomicU64::new(0));
            let renderer = {
                let (rendering, rendered) = (Arc::clone(&rendering), Arc::clone(&rendered));
                std::thread::spawn(move || {
                    while rendering.load(Ordering::Acquire) {
                        if stream.render_block(Samples::new(256)).is_none() {
                            break;
                        }
                        rendered.fetch_add(1, Ordering::Release);
                    }
                })
            };
            tutti_fixtures::wait_until("the render to run blocks", HANG_GUARD, || {
                rendered.load(Ordering::Acquire) > 16
            });
            drop(session);
            let _ = dropped_tx.send(());
            rendering.store(false, Ordering::Release);
            let _ = renderer.join();
        })
    };
    let dropped = dropped_rx.recv_timeout(HANG_GUARD);
    rendering.store(false, Ordering::Release);
    assert!(dropped.is_ok(), "the session's drop waits on a fed lane");
    session_thread.join().expect("the session thread ends");
}
