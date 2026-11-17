//! Fuzz targets over the public decoders and parsers.
//!
//! A target is `pub fn <name>(data: &[u8])` in its own module. It calls public
//! APIs only and never panics or aborts on any input: a panic is a finding.
//! [`TARGETS`] is the one list of them; the corpus replay in the test suite and
//! the libFuzzer harness under `fuzz/` both read it.
//!
//! # Adding a target
//!
//! 1. A module `src/<name>.rs` with `pub fn <name>(data: &[u8])`, declared in
//!    this file.
//! 2. A row `("<name>", <name>::<name>)` in [`TARGETS`].
//! 3. Seeds in `corpus/<name>/`: a few small files, valid and truncated. The
//!    replay test fails for a target with no seed.
//! 4. A one-liner in `fuzz/fuzz_targets/<name>.rs` calling [`run`], and a
//!    `[[bin]]` row for it in `fuzz/Cargo.toml`.
//!
//! # Running
//!
//! `cargo xtask fuzz` lists the targets, `cargo xtask fuzz <name> [--secs N]`
//! fuzzes one (60 s by default) and `cargo xtask fuzz all [--secs N]` each in
//! turn. The run needs `cargo-fuzz` and the pinned nightly toolchain. New
//! inputs go to `fuzz/corpus/<name>/`; a crash writes its input under
//! `fuzz/artifacts/<name>/` and the recipe exits non-zero.
//!
//! The fuzz project's lockfile is not committed, so a local fuzz build takes the
//! latest compatible `libfuzzer-sys`.
//!
//! Once a crash is fixed, its input is copied into `corpus/<name>/` as a
//! regression seed, and the replay test keeps it fixed.

mod wav_decode;

/// Every target by name.
pub const TARGETS: &[(&str, fn(&[u8]))] = &[("wav_decode", wav_decode::wav_decode)];

/// Runs the target `name` over `data`.
///
/// # Panics
///
/// If no target is registered under `name`, or the target finds a bug.
pub fn run(name: &str, data: &[u8]) {
    let (_, target) = TARGETS
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no fuzz target `{name}`"));
    target(data);
}
