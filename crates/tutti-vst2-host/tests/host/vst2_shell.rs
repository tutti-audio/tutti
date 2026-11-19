//! **A VST2 shell file is every plugin it names.** The reference shell
//! (`tutti-vst2-shell-plugin`) answers `audioMasterCurrentId` with what the host says: with
//! `0` it is the shell (`kPlugCategShell`) and walks its two sub-plugins at
//! `effShellGetNextPlugin`; with a sub-plugin's `uniqueID` it is that plugin.
//!
//! The oracle is the spec's protocol and the ids and names `tutti-vst2-author`'s `shell` module
//! holds. The shell plugin's own crate is a dev-dependency to be built, not linked, because its
//! entry point would clash with the reference plugin's in this binary.

use std::path::PathBuf;
use tutti_fixtures::main_thread;
use tutti_plugin_types::{Describe, InProc, PluginId, PluginRef, PluginRole};
use tutti_vst2_author::shell::{ALPHA_ID, ALPHA_NAME, BETA_ID, BETA_NAME, SHELL_ID, SHELL_NAME};
use tutti_vst2_host::Vst2Loaded;

fn shell() -> PathBuf {
    tutti_fixtures::cdylib("tutti_vst2_shell_plugin", "tutti-vst2-shell-plugin")
}

fn id(unique: i32) -> PluginId {
    PluginId::Vst2(unique as u32)
}

/// A listing of the shell is each sub-plugin in the shell's order, described as that plugin
/// and not as the shell: its own id, name, category and vendor.
#[test]
fn describing_a_shell_lists_each_sub_plugin_as_that_plugin() {
    let plugins = Vst2Loaded::describe(main_thread(), &shell()).expect("describes");

    let ids: Vec<_> = plugins.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(ids, [id(ALPHA_ID), id(BETA_ID)], "{plugins:?}");
    let names: Vec<_> = plugins.iter().map(|(_, d)| d.name.as_str()).collect();
    assert_eq!(names, [ALPHA_NAME, BETA_NAME]);
    assert_eq!(plugins[0].1.class.role(), PluginRole::Effect);
    assert_eq!(plugins[1].1.class.role(), PluginRole::Instrument);
    assert!(
        plugins.iter().all(|(_, d)| d.vendor == "Tutti"),
        "each is described from its own load: {plugins:?}"
    );
}

/// A load that names no plugin is the shell itself (the host answered 0), and a load that
/// names a sub-plugin is that plugin, with the choice kept for a reopen.
#[test]
fn a_load_by_id_is_that_sub_plugin() {
    let path = shell();
    let main = main_thread();

    let first = Vst2Loaded::load_in_process(
        main,
        &tutti_fixtures::detached_work(),
        &PluginRef::file(&path),
    )
    .expect("the shell");
    assert_eq!(first.metadata().id, SHELL_ID as u32);
    assert_eq!(first.metadata().name, SHELL_NAME);
    assert_eq!(
        first.origin().id(),
        Some(&id(SHELL_ID)),
        "the plugin the load opened, though it chose none"
    );
    drop(first);

    for (unique, name) in [(BETA_ID, BETA_NAME), (ALPHA_ID, ALPHA_NAME)] {
        let plugin = PluginRef::new(&path, id(unique));
        let loaded = Vst2Loaded::load_in_process(main, &tutti_fixtures::detached_work(), &plugin)
            .expect("a sub-plugin");
        assert_eq!(loaded.metadata().id, unique as u32);
        assert_eq!(loaded.metadata().name, name);
        assert_eq!(
            loaded.origin().id(),
            plugin.id(),
            "a reopen loads the same sub-plugin"
        );
    }
}

/// A shell that creates something else for an id (it does not know it) is refused: the
/// plugin that came up is not the one asked for. So is a choice that is no VST2 id.
#[test]
fn an_id_the_shell_does_not_have_is_refused() {
    let path = shell();
    let main = main_thread();

    let unknown = PluginRef::new(&path, id(0x1234_5678));
    let refused = Vst2Loaded::load_in_process(main, &tutti_fixtures::detached_work(), &unknown);
    assert!(
        refused.is_err_and(|e| e.to_string().contains("no plugin")),
        "the shell's default is not the plugin asked for"
    );
    // An id of another format is refused at the call, naming both.
    let other = PluginRef::new(&path, PluginId::Clap("some.plugin".into()));
    let refused = Vst2Loaded::load_in_process(main, &tutti_fixtures::detached_work(), &other);
    assert!(
        refused.is_err_and(|e| {
            let text = e.to_string();
            text.contains("clap.some.plugin") && text.contains("vst2")
        }),
        "a CLAP id is not a VST2 plugin"
    );
}
