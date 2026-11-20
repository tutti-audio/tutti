//! The host's automation mode, set on an in-process plugin's handle, reaches the
//! plugin through its format's channel (`ControlBackend::set_automation_mode`):
//! the shared plugin's transcript, saved through the same handle, holds the raw
//! state the spec names for the mode. The format's own call is the contract's
//! `the_automation_mode_reaches_the_plugin`; this is the handle's routing to it.

use tutti_fixtures::main_thread;
use tutti_plugin_contract::spec::{self, Surface};
use tutti_plugin_contract::{Transcript, find_blob};
use tutti_plugin_host::load_client;
use tutti_plugin_types::{AutomationMode, InProcess, MainWork, PluginRef, StateUse};
use tutti_units::SampleRate;

/// Sets each mode through the handle of the shared plugin at `path`, loaded as
/// `L`, and reads the last raw state the plugin was told from its saved state.
fn told_through_the_handle<L>(path: &std::path::Path, surface: Surface)
where
    L: InProcess + 'static,
    L::Processor: 'static,
{
    let (collector, work) = MainWork::new();
    let _pump = tutti_fixtures::pump(collector);
    let (_client, handle) = load_client::<L, f32>(
        main_thread(),
        &work,
        &PluginRef::file(path),
        SampleRate::SR_48K,
    )
    .unwrap_or_else(|e| panic!("in-process load of {path:?}: {e:?}"));
    for (mode, raw) in surface.automation {
        assert_eq!(handle.set_automation_mode(*mode), Ok(()), "{mode:?}");
        let state = handle
            .save_state(StateUse::Project)
            .expect("the plugin saves");
        let told = Transcript::parse(&find_blob(&state));
        assert_eq!(
            told.automation_last, *raw,
            "{mode:?} never reached the plugin through the handle"
        );
    }
    // The plugin is leaked, not dropped: a drop off the main thread posts its close to the
    // pump's thread, and a VST3 plugin ends on the thread that started it.
    std::mem::forget(handle);
    std::mem::forget(_client);
    assert!(
        matches!(surface.automation.first(), Some((AutomationMode::Off, _))),
        "the spec row starts at off"
    );
}

#[test]
fn a_clap_handle_tells_the_plugin_each_mode() {
    told_through_the_handle::<tutti_clap_host::ClapLoaded>(
        &tutti_plugin_contract::clap_path(),
        spec::CLAP,
    );
}

#[test]
fn a_vst3_handle_tells_the_plugin_each_mode() {
    told_through_the_handle::<tutti_vst3_host::Vst3Loaded>(
        &tutti_plugin_contract::vst3_module(),
        spec::VST3,
    );
}

#[test]
fn a_vst3_cpp_handle_tells_the_plugin_each_mode() {
    told_through_the_handle::<tutti_vst3_host::Vst3Loaded>(
        &tutti_plugin_contract::vst3_cpp_module(),
        spec::VST3_CPP,
    );
}
