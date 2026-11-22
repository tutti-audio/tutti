//! A served plugin's two control surfaces, against the real CLAP probe in a
//! real `plugin-server`.
//!
//! [`IpcController`] answers a call twice over: through [`Control`] (with the
//! main-thread token the trait takes) and through [`AnyThread`] (with none).
//! The two exist because a handle's off-thread calls have no token to give,
//! and each call has **one** body, in
//! `AnyThread`, which `Control` forwards to. This suite is what holds the
//! forwards to it: a `Control` call that answered a default instead of
//! forwarding would pass every test that reads only one surface.

use tutti_fixtures::{machine_lock, main_thread};
use tutti_plugin_ipc::{Ipc, IpcConfig, Launched};
use tutti_plugin_types::{AnyThread, Control, Health, RenderMode, SampleRate, StateUse};
use tutti_units::ParamId;

use super::real_stall_tests::plugin_server_path;

/// The probe's applied gain (`GAIN_PARAM_ID` in the plugin).
const GAIN: ParamId = ParamId::new(77);

/// **Every call both surfaces carry answers alike**, each against the same
/// real plugin: the parameter catalog, a value, its text, the value parsed from
/// that text, the state, and the
/// render mode. (A parameter write is queued for the audio path and lands
/// only when blocks run, so it is not read back here; its forward is one line
/// onto the same bridge call.) The text and the parse are `Some` for this plugin, so a forward
/// that answered the default (`None`) differs from `AnyThread`'s answer.
///
/// `Control::set_automation_state` returns nothing, so the advisory is pinned
/// on the `AnyThread` side alone (it is delivered).
#[test]
fn the_served_control_and_any_thread_answer_alike() {
    let _lock = machine_lock();
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", plugin_server_path());
    let Launched { ipc, .. } = Ipc::launch(
        IpcConfig::new(),
        &tutti_plugin_ipc::LoadRequest {
            origin: &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
            format: tutti_plugin_types::PluginFormat::Clap,
            rate: SampleRate::SR_48K,
            restore: None,
        },
        Some(tutti_plugin_ipc::EditorLoader::new(
            crate::format::gui::load_gui_instance,
            tutti_fixtures::detached_work(),
        )),
        tutti_fixtures::executor(),
    )
    .expect("the probe launches in a real plugin-server");
    let main = main_thread();
    let mut control = ipc.controller();
    let any = ipc.controller();

    let catalog = futures_lite::future::block_on(AnyThread::parameter_descriptors(&any))
        .expect("answered")
        .expect("the probe lists its parameters");
    assert!(!catalog.is_empty());
    assert_eq!(
        format!("{:?}", Control::parameters(&control, main)),
        format!("{catalog:?}"),
        "the catalog"
    );

    let value = futures_lite::future::block_on(AnyThread::parameter_value(&any, GAIN))
        .flatten()
        .expect("the probe answers a value");
    assert_eq!(
        Control::parameter(&control, main, GAIN),
        Some(value),
        "the value"
    );

    let text = AnyThread::parameter_text(&any, GAIN, value).expect("the probe gives a text");
    assert_eq!(
        Control::parameter_text(&control, main, GAIN, value),
        Some(text.clone()),
        "the text"
    );

    let parsed = AnyThread::parameter_value_from_text(&any, GAIN, &text)
        .expect("the probe parses its own text");
    assert_eq!(
        Control::parameter_value_from_text(&control, main, GAIN, &text),
        Some(parsed),
        "the parse"
    );

    let state = AnyThread::save_state(&any, tutti_plugin_types::StateUse::Project)
        .expect("the probe saves");
    assert_eq!(
        Control::save_state(&mut control, main, tutti_plugin_types::StateUse::Project)
            .expect("the probe saves"),
        state,
        "the state"
    );

    assert!(AnyThread::set_render_mode(&any, RenderMode::Offline));
    assert!(Control::set_render_mode(
        &mut control,
        main,
        RenderMode::Realtime
    ));
    assert!(
        AnyThread::set_automation_mode(&any, tutti_plugin_types::AutomationMode::Off).is_ok(),
        "the advisory is delivered"
    );
    assert!(!Health::is_crashed(&any));
}

/// **A live served load carries what the state is for**: the probe takes a
/// state only saved for and loaded in `CLAP_STATE_CONTEXT_FOR_DUPLICATE` (2),
/// so a duplicate's blob loads through `load_state(Duplicate)` and is
/// refused by `load_state`, which is a project's (3).
#[test]
fn a_live_served_load_reaches_the_plugin_in_the_context_it_names() {
    let _lock = machine_lock();
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", plugin_server_path());
    env.set("TUTTI_CLAP_PROBE_LOAD_ONLY_CONTEXT", "2");
    let Launched { ipc, .. } = Ipc::launch(
        IpcConfig::new(),
        &tutti_plugin_ipc::LoadRequest {
            origin: &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
            format: tutti_plugin_types::PluginFormat::Clap,
            rate: SampleRate::SR_48K,
            restore: None,
        },
        Some(tutti_plugin_ipc::EditorLoader::new(
            crate::format::gui::load_gui_instance,
            tutti_fixtures::detached_work(),
        )),
        tutti_fixtures::executor(),
    )
    .expect("the probe launches in a real plugin-server");
    let main = main_thread();
    let mut control = ipc.controller();

    let copy = Control::save_state(&mut control, main, StateUse::Duplicate)
        .expect("the probe saves for a duplicate");
    Control::load_state(&mut control, main, &copy, StateUse::Duplicate)
        .expect("a duplicate's state loads in the duplicate context");
    Control::load_state(
        &mut control,
        main,
        &copy,
        tutti_plugin_types::StateUse::Project,
    )
    .expect_err("a project load is not the duplicate context the probe accepts");
}

/// **A client launched with no editor loader asks the real server for its
/// editor**: the open crosses the wire to a real
/// `plugin-server`, which opens the editor on the served instance through the
/// format's own call, and the plugin's refusal comes back as the editor's error.
/// The probe presents no `clap.gui` in a server process (its shape is a switch
/// of the process that loaded it, which a test cannot reach across the
/// boundary), so the open is refused by the plugin: the reason is the server's,
/// where a client that loaded an editor itself would have said it has no
/// loader. What the server does with an editor that opens is the server's
/// suite (`event_loop`).
#[test]
fn a_client_with_no_editor_loader_asks_the_real_server_for_its_editor() {
    use tutti_plugin_types::EditorWindow;
    let _lock = machine_lock();
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", plugin_server_path());
    let Launched { mut ipc, .. } = Ipc::launch(
        IpcConfig::new(),
        &tutti_plugin_ipc::LoadRequest {
            origin: &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
            format: tutti_plugin_types::PluginFormat::Clap,
            rate: SampleRate::SR_48K,
            restore: None,
        },
        None,
        tutti_fixtures::executor(),
    )
    .expect("the probe launches in a real plugin-server with no editor loader");
    let main = main_thread();

    let refused = ipc
        .open_floating_editor(main)
        .expect_err("the probe has no gui in the server");
    let reason = refused.to_string();
    assert!(
        reason.contains("no gui extension"),
        "the refusal is the CLAP host's, in the server: {reason}"
    );
    // The server's answer was an answer, not a crash: the plugin is still served.
    assert!(
        futures_lite::future::block_on(AnyThread::parameter_descriptors(&ipc.controller()))
            .is_some_and(|listed| listed.is_some()),
        "the refused open left the server serving"
    );
    let resize = ipc
        .set_editor_size(main, dpi::PhysicalSize::new(100, 100).into())
        .expect_err("no editor is open");
    assert!(resize.to_string().contains("not open"), "{resize}");
}

/// **A served write of the plugin's own bypass says whether it reached the
/// plugin.** On a live link the command is queued and the write is `Sent`; once
/// the link is dead (the server ended) nothing is queued and it is `Lost`, so
/// the session writes it again rather than assuming it applied.
#[test]
fn a_served_bypass_write_is_lost_when_the_link_is_dead() {
    let _lock = machine_lock();
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", plugin_server_path());
    let Launched { ipc, .. } = Ipc::launch(
        IpcConfig::new(),
        &tutti_plugin_ipc::LoadRequest {
            origin: &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
            format: tutti_plugin_types::PluginFormat::Clap,
            rate: SampleRate::SR_48K,
            restore: None,
        },
        Some(tutti_plugin_ipc::EditorLoader::new(
            crate::format::gui::load_gui_instance,
            tutti_fixtures::detached_work(),
        )),
        tutti_fixtures::executor(),
    )
    .expect("the probe launches in a real plugin-server");
    let any = ipc.controller();
    let site = tutti_plugin_types::OwnBypass::Parameter(GAIN);
    assert_eq!(
        AnyThread::set_bypassed(&any, site, true),
        tutti_plugin_types::BypassWrite::Sent,
        "a live link takes the write"
    );
    Health::terminate(
        &any,
        tutti_plugin_types::TerminationReason::Hung {
            blocks: 1,
            held: std::time::Duration::from_secs(1),
        },
    );
    assert!(Health::is_crashed(&any), "the server is ended");
    assert_eq!(
        AnyThread::set_bypassed(&any, site, true),
        tutti_plugin_types::BypassWrite::Lost,
        "a dead link takes nothing"
    );
}

/// **Ending a served plugin the normal way does not read as its death.** The
/// guard kills the server as the plugin is dropped, and the bridge, which is
/// still attached, reads the EOF that follows; an offline fork's wait holds the
/// watch beyond the teardown, so a death declared by it would fail a good
/// render.
#[test]
fn a_normal_teardown_leaves_the_forks_watch_alive() {
    let _lock = machine_lock();
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", plugin_server_path());
    let Launched { mut ipc, .. } = Ipc::launch(
        IpcConfig::new(),
        &tutti_plugin_ipc::LoadRequest {
            origin: &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
            format: tutti_plugin_types::PluginFormat::Clap,
            rate: SampleRate::SR_48K,
            restore: None,
        },
        None,
        tutti_fixtures::executor(),
    )
    .expect("the probe launches in a real plugin-server");
    let wait = ipc.watch();
    assert!(!wait.server_died(), "a launched server is alive");
    drop(ipc);
    assert!(
        !wait.server_died(),
        "the teardown declared a death: {:?}",
        wait.death()
    );
    assert!(wait.death().is_none());
}
