//! A load asks the blacklist where it opens the plugin, and only a plugin that
//! came through a catalog is asked.

use tutti_plugin::catalog::Plugin;
use tutti_plugin_db::{Catalog, CatalogConfig, Loadable, PluginDb, Resolved};
use tutti_plugin_db::{FileFacts, FileRow, RowChange, canonical_path};
use tutti_plugin_types::Described;
use tutti_plugin_types::{IpcError, PluginClass, PluginDescriptor, PluginFormat, SampleRate};
use tutti_plugin_types::{PluginId, PluginRef};

/// A catalog whose one file `path` holds the plugin `id` of `format`, or, with no
/// `id`, is blacklisted for `reason`.
fn catalog_over(path: &std::path::Path, format: PluginFormat, id: Option<&str>) -> Catalog {
    std::fs::write(path, b"not a real plugin").unwrap();
    let facts = FileFacts::read(path);
    let row = match id {
        Some(id) => FileRow::probed(format, &facts)
            .plugins(vec![Described::new(
                PluginId::Clap(id.into()),
                PluginDescriptor::new(id, PluginClass::Unknown),
            )])
            .build()
            .expect("ids of the row's format"),
        None => FileRow::blacklisted("SIGSEGV during probe".to_string(), &facts),
    };
    let mut db = PluginDb::new();
    db.apply(RowChange::put(canonical_path(path), row));
    Catalog::new(db, CatalogConfig::new("/nonexistent"), |_| Ok(()))
}

/// A plugin no catalog resolved consults no blacklist: the load of a file the
/// host holds a [`PluginRef`] to, and no catalog, is not refused for what a
/// catalog recorded of it.
#[test]
fn an_uncatalogued_plugin_does_not_consult_a_blacklist() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("Crasher.vst3");
    let plugins = catalog_over(&path, PluginFormat::Vst3, None);
    assert!(plugins.is_blacklisted(&path), "fixture should be recorded");

    let err = Plugin::open(
        None,
        &tutti_platform::Executor::new(),
        &tutti_fixtures::detached_work(),
        &Loadable::uncatalogued(PluginRef::file(&path)),
        SampleRate::SR_48K,
    )
    .expect_err("the fixture is not a loadable plugin");
    assert!(
        !matches!(err, IpcError::Blacklisted { .. }),
        "an uncatalogued plugin has no blacklist to be refused by, got {err:?}"
    );
}

/// **A load refuses a plugin blacklisted after it was resolved**: the `Loadable`
/// asks the catalog's blacklist where the load opens the plugin, so a scan that
/// blacklists the file between the resolve and the load is not raced.
#[test]
fn a_plugin_blacklisted_after_its_resolve_is_refused_at_the_load() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("Late.clap");
    let mut plugins = catalog_over(&path, PluginFormat::Clap, Some("late"));
    let Resolved::Exact(loadable) = plugins.resolve(&PluginId::Clap("late".into())) else {
        panic!("the id is in the catalog");
    };
    assert!(loadable.check().is_ok(), "admitted when resolved");

    plugins.blacklist(&path, "crashed while scanning");

    let err = Plugin::open(
        None,
        &tutti_platform::Executor::new(),
        &tutti_fixtures::detached_work(),
        &loadable,
        SampleRate::SR_48K,
    )
    .expect_err("the plugin is blacklisted now");
    assert!(
        matches!(&err, IpcError::Blacklisted { reason, .. } if reason == "crashed while scanning"),
        "{err:?}"
    );
}
