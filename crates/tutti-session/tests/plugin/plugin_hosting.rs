//! What a load asks of the catalog and of the entry's `Hosting`, through the
//! session: the reference CLAP plugin (`clap_probe`), declared as a pool entry.

use crate::pooled::{LoadEntry, LoadPlugin, Spec, catalog_of, probe};
use crate::support::{clap_probe, plugin_server};
use tutti_plugin::catalog::{Hosting, PluginRef};
use tutti_plugin_types::BridgeError;
use tutti_session::{LoadError, Session};
use tutti_types::{MaxBlock, Samples};

fn session() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples(512)))
        .build()
        .expect("an offline session builds")
}

fn set_server(path: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: nextest runs each test in its own process, and this is set from
    // the test's own thread before anything spawns a server.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", path) };
}

/// **A blacklisted plugin is not in the catalog's answer, and is admitted once
/// the user unblacklists it**: the entry of a blacklisted file fails with
/// `Unresolved` before any server is looked for (none can be found here, which
/// would be `ServerNotFound`), and one built after the user unblacklists loads.
#[test]
fn a_blacklisted_plugin_does_not_resolve_and_loads_once_unblacklisted() {
    let path = clap_probe();
    let plugin = PluginRef::file(&path).expect("a plugin file");
    let (mut catalog, id, _dir) = catalog_of(&plugin);
    catalog.blacklist(&path, "SIGSEGV during probe");
    let mut s = session();
    let main = tutti_fixtures::main_thread();

    set_server("/nonexistent/plugin-server");
    match s.load_entry_blocking(main, &catalog, &id, Hosting::Auto, None) {
        Err(LoadError::Unresolved(_)) => {}
        other => panic!("expected an unresolved entry, got {other:?}"),
    }

    // The user unblacklists it: an entry built now admits it.
    set_server(plugin_server());
    assert!(catalog.unblacklist(&path));
    probe(&mut catalog, &path); // a blacklisted file keeps no descriptors
    s.load_entry_blocking(main, &catalog, &id, Hosting::Auto, None)
        .expect("an unblacklisted plugin loads");
}

/// **The blacklist is asked when the load runs, not when the entry is built**: a
/// plugin the catalog blacklists after the entry resolved it is refused, with
/// the recorded reason and before any server starts.
#[test]
fn a_plugin_blacklisted_after_the_entry_is_built_is_refused_when_it_loads() {
    let path = clap_probe();
    let plugin = PluginRef::file(&path).expect("a plugin file");
    let (mut catalog, id, _dir) = catalog_of(&plugin);
    let mut s = session();
    let main = tutti_fixtures::main_thread();

    set_server("/nonexistent/plugin-server");
    let declared = s.plugin_entry(0u64, &id, &catalog);
    catalog.blacklist(&path, "hung in its load");
    s.pool(declared).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let error = loop {
        let failed = s
            .main(main)
            .update()
            .into_iter()
            .find_map(|event| match event {
                tutti_session::SessionEvent::Pool {
                    event: tutti_session::PoolEvent::Failed(error),
                    ..
                } => Some(error),
                _ => None,
            });
        if let Some(error) = failed {
            break error;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the load never failed"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    };
    match error {
        LoadError::Plugin(error) => match &*error {
            BridgeError::Blacklisted { reason, .. } => assert_eq!(reason, "hung in its load"),
            other => panic!("expected a Blacklisted refusal, got {other:?}"),
        },
        other => panic!("expected a Blacklisted refusal at load, got {other:?}"),
    }
}

/// **A plugin unblacklisted before the load runs is admitted**: the entry asks
/// the catalog's blacklist when the load opens the plugin, so a plugin the
/// catalog blacklists and then clears between building the entry and the load
/// is not refused.
#[test]
fn a_plugin_unblacklisted_before_the_load_runs_is_admitted() {
    let path = clap_probe();
    let plugin = PluginRef::file(&path).expect("a plugin file");
    let (mut catalog, id, _dir) = catalog_of(&plugin);
    let mut s = session();
    let main = tutti_fixtures::main_thread();

    set_server(plugin_server());
    let declared = s.plugin_entry(0u64, &id, &catalog);
    catalog.blacklist(&path, "hung in its load");
    assert!(catalog.unblacklist(&path));
    s.pool(declared).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let events = s.main(main).update();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                tutti_session::SessionEvent::Pool {
                    event: tutti_session::PoolEvent::Failed(_),
                    ..
                }
            )),
            "an unblacklisted plugin was refused"
        );
        if events.iter().any(|event| {
            matches!(
                event,
                tutti_session::SessionEvent::Pool {
                    event: tutti_session::PoolEvent::Loaded,
                    ..
                }
            )
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the load never landed"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// **A hosting the format cannot have is refused at load**: VST2 asked to be
/// served is `NotOutOfProcess`, and the entry is not changed to in process.
#[cfg(feature = "vst2")]
#[test]
fn vst2_asked_for_out_of_process_is_refused_at_load() {
    let plugin = PluginRef::file(tutti_fixtures::vst2_probe()).expect("a plugin file");
    let (catalog, id, _dir) = catalog_of(&plugin);
    let mut s = session();
    match s.load_entry_blocking(
        tutti_fixtures::main_thread(),
        &catalog,
        &id,
        Hosting::OutOfProcess,
        None,
    ) {
        Err(LoadError::Plugin(error)) => match &*error {
            BridgeError::NotOutOfProcess { format, .. } => assert_eq!(*format, "VST2"),
            other => panic!("expected NotOutOfProcess, got {other:?}"),
        },
        other => panic!("expected NotOutOfProcess, got {other:?}"),
    }
}

/// **`Hosting::InProcess` makes the session host a CLAP plugin in its own
/// process**: no server is started (none can be found here), and the hosted
/// plugin is the one asked for.
#[test]
fn a_clap_plugin_asked_in_process_is_hosted_here() {
    set_server("/nonexistent/plugin-server");
    let plugin = PluginRef::file(clap_probe()).expect("a plugin file");
    let mut s = session();
    let node = s
        .load_plugin_blocking(
            tutti_fixtures::main_thread(),
            Spec::new(plugin).hosting(Hosting::InProcess),
        )
        .expect("the reference plugin loads in this process");
    let hosted = s.plugin_view(node).expect("it is hosted");
    assert_eq!(hosted.diagnostics().server_pid(), None);
    assert_eq!(
        hosted.descriptor().id,
        tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8")
    );
}
