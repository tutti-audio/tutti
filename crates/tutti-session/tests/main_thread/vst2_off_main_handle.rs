//! **An in-process VST2 handle answers a pool thread while the main thread runs
//! `Session::update`.**
//!
//! The snapshot and parameter-read jobs the session hands out run on a pool
//! thread and call `AnyThread::save_state` and `AnyThread::parameter_value` on
//! the handle; the backend posts them to the main thread, and the update on the
//! main thread runs them (the session's `MainWork`). The target runs on the
//! process's main thread, where the handle's own calls run at once.

use crate::param_reads::ParamReads as _;
use crate::pooled::{LoadPlugin, Spec};
use std::time::Duration;
use tutti_buffers::ChannelLayout;

use tutti_plugin::catalog::PluginRef;
use tutti_session::Session;
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::SampleRate;

/// **A pool thread asks the handle for its state and a parameter while the
/// main thread runs the session's update.** The calls wait for the update
/// rather than being refused, and the answers are real: the state equals one
/// saved on the main thread, and the parameter is the value written first.
fn a_pool_thread_is_answered_while_the_main_thread_updates_the_session() {
    let main = tutti_fixtures::main_thread();
    let (mut session, mut background) = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::SR_48K)
        .expect("a detached session builds");
    background
        .spawn_threads(std::num::NonZeroUsize::MIN)
        .expect("a background thread starts");
    let node = session
        .load_plugin_blocking(
            main,
            Spec::new(PluginRef::file(tutti_fixtures::vst2_probe())),
        )
        .expect("the in-process VST2 loads");
    let handle = session.plugin_view(node).expect("hosted").handle().clone();
    let id = ParamId::new(1);
    handle
        .send_parameter(id, Normalized::of(0.75))
        .expect("a declared parameter");
    let expected = handle
        .save_state(tutti_plugin_types::StateUse::Project)
        .expect("saved on the main thread");

    let pool = std::thread::spawn(move || {
        (
            handle.save_state(tutti_plugin_types::StateUse::Project),
            handle.parameter_value(id),
        )
    });
    // The load gave the session a main thread, so the calls wait for an update
    // rather than answering "unavailable" at once.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !pool.is_finished(),
        "the pool thread was answered before any update ran its work"
    );
    while !pool.is_finished() {
        let _ = session.update();
        std::thread::sleep(Duration::from_millis(1));
    }
    let (state, value) = pool.join().expect("the pool thread returns");

    assert_eq!(state.expect("a real state, not the deadline"), expected);
    assert_eq!(value.map(Normalized::get), Some(0.75));
}

/// The cases of this module, run by `main`.
pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["vst2_off_main_handle";
a_pool_thread_is_answered_while_the_main_thread_updates_the_session,
];
