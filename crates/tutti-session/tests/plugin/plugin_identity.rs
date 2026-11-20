//! Which plugin a load opens, and whose state a restore accepts: a plugin is
//! its file *and* its id in it (`PluginRef`), checked at the request and at
//! the hosted plugin. The reference CLAP bundle exports two plugins, in a real
//! `plugin-server`.

use crate::pooled::{LoadPlugin, Spec, catalog_of};
use crate::support::clap_probe;
use tutti_buffers::MaxBlock;
use tutti_plugin_db::PluginEntry;
use tutti_plugin_types::PluginFormat;
use tutti_plugin_types::{PluginClass, PluginDescriptor, StateUse};
use tutti_plugin_types::{PluginId, PluginRef};
use tutti_session::{PeerState, RestoreError, SavedPeer, Session};
use tutti_units::Samples;

/// The plugin a state names.
fn plugin_of(state: &PeerState) -> &PluginRef {
    match state.peer() {
        SavedPeer::Plugin(plugin) => plugin,
        other => panic!("a plugin's state names a plugin: {other:?}"),
    }
}

fn first_id() -> &'static str {
    tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8")
}

fn second_id() -> &'static str {
    tutti_clap_test_plugin::SECOND_PLUGIN_ID
        .to_str()
        .expect("utf-8")
}

fn session() -> Session {
    // SAFETY: nextest runs each test in its own process, and this is set before
    // the first server is spawned, from the test's own thread.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("an offline session builds")
}

/// What a scan reports for the bundle's plugin `id`.
fn entry_ref(path: &std::path::Path, id: &str) -> PluginRef {
    let id = PluginId::Clap(id.into());
    let descriptor = PluginDescriptor::new(id.to_string(), PluginClass::Unknown);
    PluginEntry {
        path,
        format: PluginFormat::Clap,
        id: Some(&id),
        descriptor: &descriptor,
    }
    .plugin_ref()
}

/// **A scanned plugin loads as itself, not as its file's first.** The bundle's
/// second plugin, picked by the id a scan reports, is the one that is hosted;
/// the file alone still opens the first, and its record carries the id the
/// loaded plugin reports.
#[test]
fn the_second_plugin_of_a_bundle_loads_by_its_entry() {
    let path = clap_probe();
    let mut s = session();
    let main = tutti_fixtures::main_thread();

    let second = s
        .load_plugin_blocking(main, Spec::new(entry_ref(&path, second_id())))
        .expect("the second plugin loads");
    let hosted = s.plugin_view(second).unwrap();
    assert_eq!(crate::pooled::clap_id(hosted.plugin()), second_id());
    assert_eq!(crate::pooled::clap_id(hosted.plugin()), second_id());

    let first = s
        .load_plugin_blocking(main, Spec::new(PluginRef::file(&path)))
        .expect("the file's first plugin loads");
    let hosted = s.plugin_view(first).unwrap();
    assert_eq!(crate::pooled::clap_id(hosted.plugin()), first_id());
    assert_eq!(
        crate::pooled::clap_id(hosted.plugin()),
        first_id(),
        "the record names the plugin the file's first turned out to be"
    );
}

/// **A state saved from one plugin is refused by an entry for the other, and
/// by the other's hosted plugin; its own takes it.** The refusal comes before
/// the plugin is asked: `OtherPeer` names both.
#[test]
fn a_state_is_refused_by_another_plugin() {
    let path = clap_probe();
    let first_ref = entry_ref(&path, first_id());
    let second_ref = entry_ref(&path, second_id());
    let mut s = session();
    let main = tutti_fixtures::main_thread();
    let first = s
        .load_plugin_blocking(main, Spec::new(first_ref.clone()))
        .expect("loads");
    let second = s
        .load_plugin_blocking(main, Spec::new(second_ref.clone()))
        .expect("loads");

    let from_first = s
        .plugin_view(first)
        .unwrap()
        .state()
        .save(StateUse::Project)
        .expect("saves");
    // The catalog names a plugin by its file's canonical path.
    assert_eq!(
        plugin_of(&from_first).id(),
        first_ref.id(),
        "the state names its owner"
    );
    assert_eq!(
        tutti_plugin_db::canonical_path(plugin_of(&from_first).path()),
        tutti_plugin_db::canonical_path(first_ref.path()),
        "in its own file"
    );
    let from_second = s
        .plugin_view(second)
        .unwrap()
        .state()
        .save(StateUse::Project)
        .expect("saves");

    // The entry's declaration. The key an entry is declared under is never
    // pooled here: the declaration is built and its state checked, nothing more.
    const UNPOOLED: u8 = 0;
    let (catalog, _id, _dir) = catalog_of(&PluginRef::file(&path));
    let id_of = |id: &str| PluginId::Clap(id.into());
    let refused = s
        .plugin_entry(UNPOOLED, &id_of(second_id()), &catalog)
        .initial_state(from_first.clone())
        .expect_err("the first plugin's state is not the second's");
    let SavedPeer::Plugin(expected) = refused.expected() else {
        panic!("a plugin: {:?}", refused.expected());
    };
    assert_eq!(crate::pooled::clap_id(expected), second_id());
    assert_eq!(refused.found(), from_first.peer());
    assert!(
        refused.to_string().contains(first_id()) && refused.to_string().contains(second_id()),
        "the text names both: {refused}"
    );
    assert!(
        s.plugin_entry(UNPOOLED, &id_of(first_id()), &catalog)
            .initial_state(from_first.clone())
            .is_ok(),
        "its own state is accepted"
    );

    // The hosted plugin.
    let hosted = s.plugin_view(second).unwrap();
    let refused = hosted
        .state()
        .restore(main, &from_first)
        .expect_err("a hosted plugin refuses another's state");
    assert!(matches!(refused, RestoreError::OtherPeer(_)), "{refused:?}");
    hosted
        .state()
        .restore(main, &from_second)
        .expect("and takes its own");
    assert_eq!(
        from_first.media_type().as_str(),
        "application/x-clap-state",
        "a CLAP plugin's state carries CLAP's media type"
    );
}
