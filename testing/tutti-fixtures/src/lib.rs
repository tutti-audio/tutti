//! Where the test suites find things: the repo's `assets/`, the reference plugin
//! cdylibs and the `plugin-server` and `plugin-probe` binaries;
//! [`check_range_contract`] and [`check_range_serde`], the checks a range-checked
//! type keeps its contract and its serde form by, and [`check_round_trip`], a
//! value's JSON spelling pinned both ways; [`noise`], a seeded stimulus, and
//! [`Fnv`], a bit hash; [`wait_until`] and its [`HANG_GUARD`], a bounded wait
//! for another thread, [`for_each_row`], a table test's row runner, and
//! [`unique_name`], a name for what the OS shares; [`macos_major`], the macOS
//! release a test runs on; [`fixed_blocks`] and [`cycle_blocks`], a total cut into
//! the blocks a test renders under; and [`sf2`], a SoundFont built in memory.
//!
//! A test finds a repo file through [`repo_root`] and [`asset`], never through
//! a relative `..` path, so moving a crate never changes what a test reads.
//!
//! Cargo builds a test or bench binary, and a dev-dependency's cdylib, in its
//! package's build directory, `<profile>/build/<package>/<hash>/out/`. An
//! example lands in `<profile>/examples/`, and a workspace binary in `<profile>/`
//! itself, under `<target>` or `<target>/<triple>`. So the running executable
//! names the profile directory ([`profile_dir`]) and nothing else is configured:
//! no build script, no environment variable, and the answer is right under
//! `CARGO_TARGET_DIR` and `--target`, because a test binary is built for the
//! target it runs on.
//!
//! This crate is a dev-dependency only; it is never linked into a shipped
//! binary.
//!
//! ## The two rules, and what they cost when broken
//!
//! **Absence is a hard failure, never a skip.** The plugins are built by the
//! same `cargo test` invocation that runs the tests, so a missing one is a build
//! failure rather than a property of the machine. A skip would leave the suite
//! one typo away from reporting success having executed nothing.
//!
//! **The newest candidate wins, not the first.** A package has one build
//! directory per feature set and profile settings (`<hash>`), so the same
//! plugin can sit in several of them, and a binary's uplifted copy in
//! `<profile>/` can be older than the one beside it. A stale probe makes a
//! suite run green against behaviour the tests no longer set, and can silently
//! reverse a test result.

// Test-only `unsafe` the suites need lives here, each behind a safe call with
// its argument at the site (`ScopedEnv`, `StandInWindow`, the counting
// allocator, the main-thread and AU helpers); `deny` so each site carries its
// own named `allow`. A reference plugin's probe symbols
// are its own crate's (`tutti_clap_test_plugin::ClapProbe`,
// `tutti_vst2_test_plugin::Vst2Probe`): depending on a plugin here would link
// its entry point into every suite.
#![deny(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![allow(
    clippy::disallowed_methods,
    reason = "test support: it starts the threads its tests race"
)]

mod alloc_count;
#[cfg(feature = "au")]
#[cfg_attr(target_os = "macos", path = "au/macos.rs")]
#[cfg_attr(not(target_os = "macos"), path = "au/other.rs")]
mod au;
mod block_on;
mod blocks;
mod digest;
mod env;
#[cfg(feature = "graph")]
mod event_rig;
// A loom build leaves the executor out (see `tutti-platform`).
#[cfg(not(loom))]
mod executor;
pub mod hrir;
mod links;
mod machine_lock;
mod macos;
mod main_thread;
pub mod midi;
mod names;
#[cfg(feature = "plugin")]
mod plugin;
mod range_contract;
mod record;
mod round_trip;
mod rows;
mod rt_alloc_check;
pub mod sf2;
mod shared;
pub mod signal;
pub mod tpt;
mod unique;
#[cfg(feature = "vst3-cpp")]
mod vst3_cpp;
#[cfg(feature = "vst3-sdk")]
mod vst3_sdk;
mod wait;

pub use alloc_count::{ThreadCounting, allocations};
#[cfg(feature = "au")]
#[allow(
    unused_imports,
    unreachable_pub,
    reason = "the Audio Unit fixtures exist on macOS; `au` is empty elsewhere"
)]
pub use au::*;
pub use block_on::{block_on, block_on_within};
pub use blocks::{cycle_blocks, fixed_blocks};
pub use digest::{Fnv, Noise, noise, uniform, uniform_noise};
pub use env::{ScopedEnv, cases, remove_env, set_env};
#[cfg(feature = "graph")]
pub use event_rig::{BlockClock, EventRig, Output, Seen, Sink, heard};
#[cfg(not(loom))]
#[cfg(not(loom))]
pub use executor::executor;
pub use machine_lock::{MachineLock, machine_lock};
pub use macos::macos_major;
pub use main_thread::{main_thread, main_thread_cases};
#[cfg(feature = "plugin")]
pub use plugin::{
    Activation, Active, Pump, StandInWindow, Whole, detached_work, pump, pumped_work, tempo,
    vst3_module,
};
pub use range_contract::{RangeContract, check_range_contract, check_range_serde};
pub use record::{BlockLog, Record};
pub use round_trip::check_round_trip;
pub use rows::for_each_row;
pub use rt_alloc_check::RtAllocCheck;
pub use shared::Shared;
pub use unique::unique_name;
#[cfg(feature = "vst3-cpp")]
pub use vst3_cpp::{bundle_of, vst3_cpp_module};
#[cfg(feature = "vst3-sdk")]
pub use vst3_sdk::{SdkRecord, sdk_zeroed};
pub use wait::{HANG_GUARD, wait_until};

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The first ancestor of `start` (itself included) that holds a `Cargo.lock`.
///
/// Pure over the filesystem it is given, so the walk is testable on a temp tree.
pub fn find_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("Cargo.lock").is_file())
        .map(Path::to_path_buf)
}

/// The repository root: the first ancestor of this crate's manifest directory
/// that holds a `Cargo.lock`.
///
/// # Panics
///
/// If there is none, naming the start.
pub fn repo_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let own = Path::new(env!("CARGO_MANIFEST_DIR"));
        find_root(own).unwrap_or_else(|| {
            panic!(
                "no Cargo.lock above {}; the repo root cannot be found",
                own.display()
            )
        })
    })
}

/// A file under the repo's `assets/`, e.g. `asset("audio/stereo.flac")`.
///
/// # Panics
///
/// If the file is absent, naming the full path (and, for a soundfont, how to
/// fetch it). A missing asset fails the test; it never skips it.
pub fn asset(rel: &str) -> PathBuf {
    // One component at a time, so the path and the panic are native on every
    // platform (`rel` always uses `/`).
    let path = rel
        .split('/')
        .fold(repo_root().join("assets"), |dir, part| dir.join(part));
    assert!(
        path.is_file(),
        "test asset missing at {}.{}",
        path.display(),
        if rel.starts_with("soundfonts/") {
            " Fetch it with `assets/soundfonts/download-timgm6mb.sh` (see \
             `assets/soundfonts/README.md`)."
        } else {
            ""
        }
    );
    path
}

/// The profile directory a binary at `exe` belongs to: the directory above
/// `build/<package>/<hash>/out/` for a test or bench, above `examples/` for an
/// example, and its own directory for a workspace binary.
///
/// Pure over the path, so the layouts are testable without cargo.
///
/// # Panics
///
/// Panics when `exe` has no parent directory.
pub fn profile_dir_of(exe: &Path) -> PathBuf {
    let dir = exe.parent().expect("an executable path has a parent");
    let named =
        |p: Option<&Path>, name: &str| p.and_then(Path::file_name).is_some_and(|n| n == name);
    let mut up = dir.ancestors();
    if named(Some(dir), "out") && named(up.nth(3), "build") {
        return up.next().unwrap_or(dir).to_path_buf();
    }
    if named(Some(dir), "examples") {
        return dir.parent().unwrap_or(dir).to_path_buf();
    }
    dir.to_path_buf()
}

/// The profile directory of the running test, bench or example binary.
///
/// # Panics
///
/// If the OS cannot name the executable.
pub fn profile_dir() -> PathBuf {
    profile_dir_of(&std::env::current_exe().expect("the running executable has a path"))
}

/// The cdylib filename for the target being built, e.g.
/// `libtutti_clap_test_plugin.so`.
///
/// `lib_stem` is the crate name with `-` replaced by `_`, as cargo spells it.
pub fn lib_filename(lib_stem: &str) -> String {
    format!("{}{lib_stem}.{}", names::LIB_PREFIX, names::LIB_EXT)
}

/// Where cargo may have put `package`'s artifact `name` under `profile`:
/// `<profile>/<name>`, then `<profile>/build/<package>/<hash>/out/<name>` for
/// every build directory of the package.
pub fn candidates_in(profile: &Path, package: &str, name: &str) -> Vec<PathBuf> {
    let builds = std::fs::read_dir(profile.join("build").join(package))
        .into_iter()
        .flatten()
        .flatten()
        .map(|hash| hash.path().join("out").join(name));
    std::iter::once(profile.join(name)).chain(builds).collect()
}

/// The newest existing regular file among `candidates`, or `None`.
///
/// **Newest, not first**: see the module docs. Directories are rejected, since
/// only a regular file can be `dlopen`ed.
pub fn newest_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .filter(|c| c.is_file())
        .filter_map(|c| {
            let mtime = std::fs::metadata(c).and_then(|m| m.modified()).ok()?;
            Some((mtime, c))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, c)| c.clone())
}

fn searched(candidates: &[PathBuf]) -> String {
    candidates
        .iter()
        .map(|c| format!("\n  - {}", c.display()))
        .collect()
}

/// The reference plugin cdylib for `lib_stem` (a crate name with `_`), the
/// newest copy under the running binary's profile directory.
///
/// `package` is the plugin's package, whose build directory holds the cdylib.
///
/// # Panics
///
/// If it does not exist, naming every path tried. The plugin is a
/// dev-dependency built by the same `cargo test`, so this is a build failure,
/// not a missing install.
pub fn cdylib(lib_stem: &str, package: &str) -> PathBuf {
    let tried = candidates_in(&profile_dir(), package, &lib_filename(lib_stem));
    newest_existing(&tried).unwrap_or_else(|| {
        panic!(
            "reference plugin `{package}` not found.\nSearched:{}\n\
             It is a dev-dependency built by this same `cargo test`, so this is \
             a build failure, not a missing install.",
            searched(&tried)
        )
    })
}

/// The `plugin-server` binary under the running binary's profile directory.
///
/// # Panics
///
/// If it has not been built. It is not a dev-dependency of the crates whose
/// tests spawn it (`tutti-plugin-server` depends on `tutti-plugin`, so the edge
/// would be a cycle), so running tests does not build it.
pub fn plugin_server() -> PathBuf {
    workspace_binary("plugin-server", "tutti-plugin-server")
}

/// The `plugin-probe` binary under the running binary's profile directory.
///
/// # Panics
///
/// If it has not been built. A dev-dependency builds a crate's library, not its
/// binary, and `tutti-plugin` spawns this one, so running tests does not build
/// it.
pub fn plugin_probe() -> PathBuf {
    workspace_binary("plugin-probe", "tutti-plugin-probe")
}

/// The newest `bin` under the profile directory; `package` names what builds it.
fn workspace_binary(bin: &str, package: &str) -> PathBuf {
    let name = format!("{bin}{}", std::env::consts::EXE_SUFFIX);
    let tried = candidates_in(&profile_dir(), package, &name);
    newest_existing(&tried).unwrap_or_else(|| {
        panic!(
            "`{bin}` not found.\nSearched:{}\n\
             It is not a dev-dependency (that would be a cycle through \
             `tutti-plugin`), so running tests does not build it. Build it \
             first, then re-run:\n\n  cargo build -p {package}\n",
            searched(&tried)
        )
    })
}

/// The CLAP test plugin (`tutti-clap-test-plugin`), published under a `.clap`
/// extension ([`publish_with_extension`]): `PluginRef::file` takes a format
/// from the file's extension, and cargo's `libtutti_clap_test_plugin.so` reads
/// as VST2 there.
///
/// # Panics
///
/// If the cdylib has not been built ([`cdylib`]).
pub fn clap_probe() -> &'static Path {
    static PUBLISHED: OnceLock<PathBuf> = OnceLock::new();
    PUBLISHED.get_or_init(|| {
        let real = cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin");
        publish_with_extension(&real, "clap")
    })
}

/// The VST2 test plugin (`tutti-vst2-test-plugin`).
///
/// On macOS a host meets VST2 under a `.vst` name and cargo's `.dylib` reads as
/// no format (`PluginRef::file` refuses it), so the path is the `.vst` link
/// ([`publish_with_extension`]); elsewhere it is the cdylib, whose `.so` and
/// `.dll` already read as VST2.
///
/// # Panics
///
/// If the cdylib has not been built ([`cdylib`]).
pub fn vst2_probe() -> &'static Path {
    static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        let real = cdylib("tutti_vst2_test_plugin", "tutti-vst2-test-plugin");
        match names::VST2_LINK_EXT {
            Some(extension) => publish_with_extension(&real, extension),
            None => real,
        }
    })
}

/// Publish `real` under its own name with `extension` (`.clap`, say) beside
/// it, and return that path.
///
/// The extension is load-bearing for a host that dispatches on it: cargo's
/// `libtutti_clap_test_plugin.so` reads as VST2 there. The link path is shared
/// by every test process of every suite, so it is published by atomic rename from a per-process
/// staging name.
///
/// **On Unix a link that already points at `real` is left alone.** Only the
/// first process republishes, or one that finds the link naming something
/// else. Nothing goes stale by
/// skipping: a symlink is resolved at each open, so one naming `real` is as
/// fresh as `real`. A Windows copy can go stale, so it is still republished.
///
/// **A `real` that already has `extension` is returned as it is**: the link
/// path would be `real` itself.
///
/// # Panics
///
/// If the link cannot be staged, or is absent after a lost rename.
pub fn publish_with_extension(real: &Path, extension: &str) -> PathBuf {
    let link = real.with_extension(extension);
    // `real` already has the extension: it is its own link, and publishing
    // one would replace the file with a link to itself.
    if link == real {
        return link;
    }
    if links::is_published(&link, real) {
        return link;
    }

    // Stage under a name no other process can pick, then swap it in.
    let staging = link.with_extension(format!("{extension}.tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    links::stage(real, &staging);

    if let Err(e) = std::fs::rename(&staging, &link) {
        // Losing the swap is not a failure: whoever won published a link to
        // the same artifact. Only a missing result is fatal.
        let _ = std::fs::remove_file(&staging);
        assert!(
            link.exists(),
            "publish the reference plugin at {}: {e}",
            link.display()
        );
    }
    link
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule the module docs call load-bearing: newest wins, so a stale
    /// copy in another build directory cannot shadow a fresh one.
    #[test]
    fn newest_wins_over_first() {
        let dir = std::env::temp_dir().join(format!("tfr-newest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let old = dir.join("old.so");
        let new = dir.join("new.so");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&new, b"x").unwrap();
        // Set the mtimes outright: equal mtimes would let `max_by_key` return
        // either file, and the test would pass for the wrong reason.
        let now = tutti_platform::SystemTime::now();
        let age = |file: &Path, by: u64| {
            std::fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(now - std::time::Duration::from_secs(by))
                .unwrap();
        };
        age(&old, 100);
        age(&new, 10);

        let picked = newest_existing(&[old.clone(), new.clone()]).expect("one exists");
        assert_eq!(
            picked, new,
            "the older candidate was listed first and must not win"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory at a candidate path is not a plugin: `metadata` succeeds on
    /// a directory, `dlopen` does not.
    #[test]
    fn a_directory_is_not_a_candidate() {
        let dir = std::env::temp_dir().join(format!("tfr-dir-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(newest_existing(std::slice::from_ref(&dir)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A test or bench binary is in its package's `build/<package>/<hash>/out/`,
    /// an example in `examples/`, and a plain binary beside its profile; a
    /// target triple only adds a level above. An `out/` that is not under
    /// `build/` is a plain directory.
    #[test]
    fn the_profile_dir_is_derived_from_the_executable_path() {
        let p = |s: &str| profile_dir_of(Path::new(s));
        assert_eq!(
            p("/t/debug/build/tutti/9a6f/out/a-1f"),
            Path::new("/t/debug")
        );
        assert_eq!(p("/t/debug/examples/ex"), Path::new("/t/debug"));
        assert_eq!(p("/t/release/plugin-server"), Path::new("/t/release"));
        assert_eq!(p("/opt/app/x/y/out/tool"), Path::new("/opt/app/x/y/out"));
        assert_eq!(
            p("/t/x86_64-pc-windows-msvc/debug/build/tutti-rt/1f/out/a-1f.exe"),
            Path::new("/t/x86_64-pc-windows-msvc/debug")
        );
        assert_eq!(
            p("/t/aarch64-apple-darwin/release/examples/ex"),
            Path::new("/t/aarch64-apple-darwin/release")
        );
    }

    /// The root is the nearest ancestor with a `Cargo.lock`, not the farthest,
    /// and the start itself counts.
    #[test]
    fn the_root_is_the_nearest_ancestor_with_a_lockfile() {
        let base = std::env::temp_dir().join(format!("tfx-root-{}", std::process::id()));
        let inner = base.join("outer/inner/deep");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(base.join("outer/Cargo.lock"), "").unwrap();
        assert_eq!(find_root(&inner), Some(base.join("outer")));
        std::fs::write(base.join("outer/inner/Cargo.lock"), "").unwrap();
        assert_eq!(find_root(&inner), Some(base.join("outer/inner")));
        assert_eq!(
            find_root(&base.join("outer/inner")),
            Some(base.join("outer/inner")),
            "the start itself counts"
        );
        std::fs::remove_file(base.join("outer/Cargo.lock")).unwrap();
        std::fs::remove_file(base.join("outer/inner/Cargo.lock")).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The root found is the real repo root, with the assets under it.
    #[test]
    fn the_repo_root_holds_the_assets() {
        assert!(repo_root().join("Cargo.lock").is_file());
        assert!(asset("audio/stereo.flac").is_file());
    }

    /// A missing asset fails, naming the full path in the platform's own
    /// separators (the path is built component by component).
    #[test]
    fn a_missing_asset_panics_with_its_path() {
        let native = repo_root()
            .join("assets")
            .join("audio")
            .join("no_such_file.wav");
        let panic = std::panic::catch_unwind(|| asset("audio/no_such_file.wav"))
            .expect_err("a missing asset must panic");
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .expect("a string panic");
        assert!(
            message.contains(&native.display().to_string()),
            "{message:?} does not name {}",
            native.display()
        );
    }

    #[test]
    #[should_panic(expected = "download-timgm6mb.sh")]
    fn a_missing_soundfont_names_how_to_fetch_it() {
        asset("soundfonts/no_such_font.sf2");
    }

    /// The uplifted copy, then one candidate per build directory of the package
    /// and none from another package's.
    #[test]
    fn candidates_are_the_uplifted_copy_then_each_build_directory() {
        let profile = std::env::temp_dir().join(format!("tfx-cand-{}", std::process::id()));
        for dir in ["build/pkg/a1/out", "build/pkg/b2/out", "build/other/c3/out"] {
            std::fs::create_dir_all(profile.join(dir)).unwrap();
        }
        let mut found = candidates_in(&profile, "pkg", "x");
        found[1..].sort();
        assert_eq!(
            found,
            [
                profile.join("x"),
                profile.join("build/pkg/a1/out/x"),
                profile.join("build/pkg/b2/out/x"),
            ]
        );
        assert_eq!(candidates_in(&profile, "none", "x"), [profile.join("x")]);
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn lib_filename_follows_the_target_os() {
        // Assert the shape rather than one platform's answer, so this holds on
        // every runner.
        let name = lib_filename("some_plugin");
        assert!(
            name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".dll"),
            "unexpected cdylib name: {name}"
        );
        assert!(name.contains("some_plugin"));
    }

    /// A file that already has the extension is its own published path, and stays a file
    /// (D18: the Linux `.so` cdylib published as `.so` became a link to itself).
    #[test]
    fn a_file_with_the_extension_is_left_as_it_is() {
        let dir = std::env::temp_dir().join(format!("tfr-self-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let real = dir.join("libplugin.so");
        std::fs::write(&real, b"x").unwrap();
        assert_eq!(publish_with_extension(&real, "so"), real);
        assert!(std::fs::symlink_metadata(&real).unwrap().is_file());
        assert_eq!(std::fs::read(&real).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
