//! A scanned link to a differently named target loads by its catalog key (feature
//! `clap`; `cargo build -p tutti-plugin-server -p tutti-plugin-probe` first).

#![cfg(all(feature = "clap", unix))]

use tutti_plugin::catalog::Plugin;
use tutti_plugin_db::{Catalog, CatalogConfig, Hosting};
use tutti_plugin_db::{PluginDb, ScanPoll};
use tutti_plugin_types::PluginFormat;
use tutti_plugin_types::{Locator, PluginId};

/// **A scanned link to a differently named target loads by its catalog
/// key**: the row is CLAP though its file is `libfoo.so`, so the load that
/// resolves the plugin's id opens the target as CLAP, in a server, and the
/// plugin that comes back is the row's.
#[test]
fn a_scanned_link_to_a_differently_named_target_loads_by_its_catalog_key() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let target = dir.path().join("libfoo.so");
    std::fs::copy(tutti_fixtures::clap_probe(), &target).expect("a plugin");
    let scanned = dir.path().join("plugins");
    std::fs::create_dir(&scanned).expect("the scanned directory");
    std::os::unix::fs::symlink(&target, scanned.join("Foo.clap")).expect("a link");
    let config = CatalogConfig::new(dir.path())
        .scan_dirs(vec![scanned])
        .probe_locator(Locator::at(tutti_fixtures::plugin_probe()));
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));
    let mut task = catalog.scan().expect("no task is running");
    let mut result = None;
    tutti_fixtures::wait_until(
        "the scan to end",
        std::time::Duration::from_secs(120),
        || match catalog.try_apply(&mut task) {
            ScanPoll::Running => false,
            ScanPoll::Done(done) => {
                result = Some(done);
                true
            }
            ScanPoll::Died => panic!("the scan's worker died"),
        },
    );
    let result = result.expect("the wait ended on a result");
    assert!(result.failures().is_empty(), "{:?}", result.failures());
    let (listed, _) = catalog.iter().next().expect("the link is one row");
    assert_eq!(listed.path(), tutti_plugin_db::canonical_path(&target));
    let id = listed.id().cloned().expect("the scan read the plugin's id");
    assert!(matches!(id, PluginId::Clap(_)), "{id:?}");

    let loadable = catalog
        .resolve(&id)
        .bind()
        .expect("the scanned id is in the catalog")
        .hosting(Hosting::OutOfProcess);
    assert_eq!(loadable.format(), Ok(PluginFormat::Clap));
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let plugin = Plugin::open_served(
        &tutti_plugin_ipc::AudioConfig::default(),
        &tutti_fixtures::detached_work(),
        &loadable,
        tutti_units::SampleRate::SR_48K,
    )
    .expect("the target loads as CLAP, though its extension names VST2");

    assert_eq!(plugin.plugin().id(), Some(&id));
    assert_eq!(plugin.plugin().format(), Ok(PluginFormat::Clap));
    assert_eq!(plugin.plugin().path(), listed.path());
    assert!(plugin.handle().server_pid().is_some());
}
