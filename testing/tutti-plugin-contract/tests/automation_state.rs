//! The host's automation mode, set on an in-process plugin's handle, reaches
//! a plugin that carries its format's automation-state channel and is refused
//! (`Unsupported`) by one that does not.
//!
//! The channel is CLAP's `param-indication`, VST3's `IAutomationState` and
//! VST2's `audioMasterGetAutomationState` (the VST2 case, which reads what the
//! plugin was answered, is `tutti-plugin-host`'s `vst2_in_process_render_mode`).
//! The shared plugin implements none of the first two, so it is the plugin
//! without one; the CLAP test plugin implements `param-indication`.

use tutti_fixtures::main_thread;
use tutti_plugin_host::{PluginHandle, load_client};
use tutti_plugin_types::{
    AutomationMode, AutomationModeError, Features, InProcess, MainWork, PluginRef,
};
use tutti_units::SampleRate;

/// `path` loaded in process as `L`, with a thread standing in for the main
/// thread's tick (the handle's calls hop to it from the test's thread).
fn loaded<L>(path: &std::path::Path) -> (PluginHandle, tutti_fixtures::Pump)
where
    L: InProcess + 'static,
    L::Processor: 'static,
{
    let (collector, work) = MainWork::new();
    let pump = tutti_fixtures::pump(collector);
    let (_client, handle) = load_client::<L, f32>(
        main_thread(),
        &work,
        &PluginRef::file(path),
        SampleRate::SR_48K,
    )
    .unwrap_or_else(|e| panic!("in-process load of {path:?}: {e:?}"));
    (handle, pump)
}

/// A plugin with no automation-state channel does not claim the feature, and the
/// handle's call names its format in the refusal.
fn assert_refused<L>(path: &std::path::Path, format: &str)
where
    L: InProcess + 'static,
    L::Processor: 'static,
{
    let (handle, _pump) = loaded::<L>(path);
    assert!(
        !handle
            .loaded()
            .features
            .contains(Features::AUTOMATION_STATE),
        "{format} claims a channel the plugin does not carry"
    );
    assert_eq!(
        handle.set_automation_mode(AutomationMode::Writing),
        Err(AutomationModeError::Unsupported {
            format: format.to_owned()
        })
    );
}

/// **A CLAP plugin with `param-indication` is told**: the feature is claimed
/// and the call is delivered.
#[test]
fn a_clap_plugin_with_param_indication_takes_the_automation_mode() {
    let (handle, _pump) = loaded::<tutti_clap_host::ClapLoaded>(tutti_fixtures::clap_probe());
    assert!(
        handle
            .loaded()
            .features
            .contains(Features::AUTOMATION_STATE)
    );
    assert_eq!(handle.set_automation_mode(AutomationMode::Writing), Ok(()));
}

/// **A CLAP plugin without `param-indication` is refused** with `Unsupported`.
#[test]
fn a_clap_plugin_without_param_indication_is_refused() {
    assert_refused::<tutti_clap_host::ClapLoaded>(&tutti_plugin_contract::clap_path(), "clap");
}

/// **A VST3 controller without `IAutomationState` is refused** with
/// `Unsupported`, for both the Rust and the C++ plugin.
#[test]
fn a_vst3_plugin_without_iautomationstate_is_refused() {
    assert_refused::<tutti_vst3_host::Vst3Loaded>(&tutti_plugin_contract::vst3_module(), "vst3");
    assert_refused::<tutti_vst3_host::Vst3Loaded>(
        &tutti_plugin_contract::vst3_cpp_module(),
        "vst3",
    );
}
