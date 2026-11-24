//! The reference CLAP plugin and the `plugin-server` that hosts it, for the
//! plugin suite and the §5.2 example (`crates/tutti/examples`, which includes
//! this file by path).
//!
//! The plugin is a dev-dependency, so cargo builds its cdylib alongside the
//! test. The server is not (it would be a dependency cycle through
//! `tutti-plugin`), so it must be built first: `cargo build -p
//! tutti-plugin-server`. Both are looked up under the running binary's profile
//! directory (`tutti_fixtures::profile_dir`).

use std::path::PathBuf;

/// The reference CLAP cdylib, published under a `.clap` name: the server
/// picks the loader by extension, and reads a bare `.so` as VST2.
pub(crate) fn clap_probe() -> PathBuf {
    let real = tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
    tutti_fixtures::publish_with_extension(&real, "clap")
}
