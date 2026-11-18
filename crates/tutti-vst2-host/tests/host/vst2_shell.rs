//! **A VST2 shell file is every plugin it names.** The reference shell
//! (`tutti-vst2-shell-plugin`) answers `audioMasterCurrentId` with what the host says: with
//! `0` it is the shell (`kPlugCategShell`) and walks its two sub-plugins at
//! `effShellGetNextPlugin`; with a sub-plugin's `uniqueID` it is that plugin.
//!
//! The oracle is the spec's protocol and the ids and names `tutti-vst2-shell-plugin` declares,
//! spelt here: the crate is a dev-dependency to be built, not linked, because its entry point
//! would clash with the reference plugin's in this binary.

use std::path::PathBuf;
use tutti_fixtures::main_thread;
use tutti_plugin_types::{Describe, InProcess, Load, PluginRole};
use tutti_vst2_host::Vst2Loaded;

/// The shell's own `uniqueID`: the plugin the host gets when it names none.
const SHELL_ID: i32 = i32::from_be_bytes(*b"SHEL");
/// The first sub-plugin, an effect.
const ALPHA_ID: i32 = i32::from_be_bytes(*b"SHAL");
/// The second sub-plugin, an instrument.
const BETA_ID: i32 = i32::from_be_bytes(*b"SHBE");
const SHELL_NAME: &str = "Tutti VST2 Shell";
const ALPHA_NAME: &str = "Shell Alpha";
const BETA_NAME: &str = "Shell Beta";

fn shell() -> PathBuf {
    tutti_fixtures::cdylib("tutti_vst2_shell_plugin", "tutti-vst2-shell-plugin")
}

fn id(unique: i32) -> String {
    format!("vst2.{unique}")
}

/// A listing of the shell is each sub-plugin in the shell's order, described as that plugin
/// and not as the shell: its own category and vendor.
#[test]
fn describing_a_shell_lists_each_sub_plugin_as_that_plugin() {
    let plugins = Vst2Loaded::describe(main_thread(), &shell()).expect("describes");

    let ids: Vec<_> = plugins.iter().map(|p| p.id.clone()).collect();
    assert_eq!(ids, [id(ALPHA_ID), id(BETA_ID)], "{plugins:?}");
    assert_eq!(plugins[0].class.role(), PluginRole::Effect);
    assert_eq!(plugins[1].class.role(), PluginRole::Instrument);
    assert!(
        plugins.iter().all(|p| p.vendor == "Tutti"),
        "each is described from its own load: {plugins:?}"
    );
}

/// A load that names no plugin is the shell itself (the host answered 0), and a load that
/// names a sub-plugin is that plugin, with the choice kept for a reopen.
#[test]
fn a_load_by_id_is_that_sub_plugin() {
    let path = shell();
    let main = main_thread();

    let first =
        Vst2Loaded::load_in_process(main, &tutti_fixtures::detached_work(), &path, &Load::new())
            .expect("the shell");
    assert_eq!(first.metadata().id, id(SHELL_ID));
    assert_eq!(first.metadata().name, SHELL_NAME);
    assert_eq!(first.origin().load().chosen(), None);
    drop(first);

    for (unique, name) in [(BETA_ID, BETA_NAME), (ALPHA_ID, ALPHA_NAME)] {
        let load = Load::new().plugin(id(unique));
        let loaded =
            Vst2Loaded::load_in_process(main, &tutti_fixtures::detached_work(), &path, &load)
                .expect("a sub-plugin");
        assert_eq!(loaded.metadata().id, id(unique));
        assert_eq!(loaded.metadata().name, name);
        assert_eq!(
            loaded.origin().load(),
            &load,
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

    let unknown = Load::new().plugin(id(0x1234_5678));
    let refused =
        Vst2Loaded::load_in_process(main, &tutti_fixtures::detached_work(), &path, &unknown);
    assert!(
        refused.is_err_and(|e| e.to_string().contains("no plugin")),
        "the shell's default is not the plugin asked for"
    );
    let malformed = Load::new().plugin("clap.some.plugin");
    assert!(
        Vst2Loaded::load_in_process(main, &tutti_fixtures::detached_work(), &path, &malformed)
            .is_err()
    );
}
