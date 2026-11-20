//! A plugin's own event sources, served by the main-thread view's `update` for
//! a plugin hosted in this process: the descriptors and timers it asked the
//! host to run (CLAP's `posix-fd-support` and `timer-support`) are looked at
//! without waiting, and what is ready is handed back, so an in-process host's
//! editors get them without the app knowing any format.
//!
//! The reference CLAP plugin, hosted in process (`Hosting::InProcess`), with an
//! embeddable editor open: an X11 window id is what its editor embeds into, so
//! the module is one `cfg` on its declaration.

use crate::pooled::{LoadPlugin, Spec};
use std::sync::OnceLock;
use std::time::Duration;

use tutti_buffers::MaxBlock;
use tutti_clap_test_plugin::{
    CMD_REGISTER_FD, CMD_REGISTER_TIMER, ClapProbe, ClapSwitch, GuiMode, Site,
};
use tutti_fixtures::StandInWindow;
use tutti_plugin_db::Hosting;
use tutti_plugin_types::ContentScale;
use tutti_plugin_types::PluginRef;
use tutti_session::{Editor, Session};
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

/// A session with the reference plugin hosted in this process, an editor open
/// on it, and the plugin's latched command run by the editor's `show`.
fn session_with_open_editor(command: u32) -> (Session, Editor) {
    let probe = probe();
    probe.set_gui_mode(GuiMode::Embeddable);
    probe.thread_reset();
    probe.thread_command(command);
    let (mut s, mut background) = Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("an offline session builds");
    background
        .spawn_threads(std::num::NonZeroUsize::MIN)
        .expect("a background thread starts");
    let plugin = PluginRef::file(clap_probe());
    let node = s
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(plugin).hosting(Hosting::InProcess),
        )
        .expect("the reference plugin loads in this process");
    let mut editor = s
        .editor(node, &tutti_fixtures::main_thread())
        .expect("it is hosted");
    editor
        .open(&StandInWindow, ContentScale::IDENTITY)
        .expect("the editor opens");
    (s, editor)
}

/// **A descriptor the plugin registered is served by the view's `update` the
/// moment it is readable**, and by no other call: the view looks at the
/// plugin's descriptors without waiting and hands back what is ready.
fn an_in_process_editors_descriptor_is_served_by_the_views_update() {
    let (mut s, _editor) = session_with_open_editor(CMD_REGISTER_FD);
    let probe = probe();
    assert!(
        probe.thread_capture().fd_registered,
        "the plugin registered a descriptor when its editor was shown"
    );

    let main = tutti_fixtures::main_thread();
    let _ = s.update_on(main);
    assert_eq!(
        probe.thread_capture().fd_fires,
        0,
        "nothing is readable, so nothing is called"
    );
    for fires in 1..=3 {
        assert!(probe.fd_poke(), "the plugin's pipe takes a byte");
        let _ = s.update_on(main);
        assert_eq!(
            probe.thread_capture().fd_fires,
            fires,
            "one update after the byte, one call of `on_fd`"
        );
    }
    let _ = s.update_on(main);
    assert_eq!(
        probe.thread_capture().fd_fires,
        3,
        "drained, so not called again"
    );
}

/// **A timer the plugin registered is served by the view's `update`**: the
/// view calls the plugin's `on_timer` once the timer is due. (When a timer is
/// due, and that its period restarts, is `take_due_timers`' rule, tested over
/// constructed instants in `tutti-clap-host`.)
fn an_in_process_editors_timer_is_served_by_the_views_update() {
    // The shortest period the host keeps.
    probe().set(ClapSwitch::TimerPeriod, 10);
    let (mut s, _editor) = session_with_open_editor(CMD_REGISTER_TIMER);
    let probe = probe();
    assert!(probe.thread_capture().timer_registered);
    let ticks = || probe.thread_capture().sites[Site::OnTimer as usize].visits;

    let main = tutti_fixtures::main_thread();
    // Updates until the timer is served: a hang guard, not a speed claim.
    let guard = tutti_platform::Instant::now() + Duration::from_secs(60);
    while ticks() == 0 {
        assert!(
            tutti_platform::Instant::now() < guard,
            "the view's update never served the timer"
        );
        let _ = s.update_on(main);
    }
    probe.set(ClapSwitch::TimerPeriod, 0);
}

/// **A bare `Session::update` on the OS main thread does the main thread's
/// share**: it services an open editor (the plugin's descriptor is served) and
/// drops a removed in-process plugin from the graveyard, with no marker passed.
fn a_bare_update_on_the_main_thread_services_editors_and_exhumes() {
    let (mut s, editor) = session_with_open_editor(CMD_REGISTER_FD);
    let probe = probe();
    assert!(probe.fd_poke(), "the plugin's pipe takes a byte");
    let _ = s.update();
    assert_eq!(
        probe.thread_capture().fd_fires,
        1,
        "the open editor was serviced"
    );

    s.remove(editor.node()).expect("the plugin is hosted");
    assert_eq!(
        s.retired_plugins(),
        1,
        "removal parks the in-process record"
    );
    let _ = s.update();
    assert_eq!(
        s.retired_plugins(),
        0,
        "the update on the main thread dropped it"
    );
    assert!(
        !editor.is_open(),
        "the removed plugin's editor reads closed"
    );
}

/// The cases of this module, run by `main`.
pub(crate) const CASES: &[(&str, fn())] = tutti_fixtures::cases!["plugin_event_sources";
an_in_process_editors_descriptor_is_served_by_the_views_update,
an_in_process_editors_timer_is_served_by_the_views_update,
a_bare_update_on_the_main_thread_services_editors_and_exhumes,
];
