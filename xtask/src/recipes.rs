//! The recipes: each one a plan of steps, the justfile's lines in the
//! justfile's order. Planning is pure (it reads nothing but the arguments and
//! the `Ctx`), so `--dry-run` and the tests see exactly what a run would do.

use crate::cmd::Cmd;
use crate::gates::{GATES, Gate};
use crate::release::Version;
use std::collections::HashSet;
use std::path::PathBuf;

/// Where the plan runs: the repo root and the cargo target directory.
#[derive(Debug, Clone)]
pub(crate) struct Ctx {
    pub(crate) root: PathBuf,
    pub(crate) target: PathBuf,
}

/// One thing a recipe does, stopping the recipe if it fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Run(Cmd),
    /// Run the command; a failure is remembered and the recipe goes on, then fails
    /// at its end (`rtsan`: one run lists every finding).
    RunAll(Cmd),
    /// Write a file (`check-windows`'s stub header), creating its directory.
    WriteFile {
        path: PathBuf,
        contents: String,
    },
    /// A gate over the repo's files (`gates.rs`), run in this process.
    Gate(Gate),
    /// `conformance-inventory <format>`: rewrite an inventory from its spec source.
    ConformanceInventory(String),
    /// `prune-building`: delete the building baseline's stale rows.
    PruneBuilding,
    /// `write-public-items`: rewrite `xtask/public-items/` from the tree.
    WritePublicItems,
    /// `conformance`: the dashboard and badges from per-OS JUnit files
    /// (`gates/conformance_site.rs`).
    Conformance {
        results: PathBuf,
        out: PathBuf,
    },
    /// Run the command with the environment `cargo llvm-cov show-env` prints,
    /// so it builds into the target directory the instrumented tests run from.
    InLlvmCovEnv(Cmd),
    /// `coverage`'s probe: fails with the install hint when cargo-llvm-cov is missing.
    RequireLlvmCov,
    /// `deny`'s probe: fails with the install hint when cargo-deny is missing.
    RequireCargoDeny,
    /// `test-wasm`'s probe: fails when `wasm-bindgen-test-runner` is not at the
    /// version `Cargo.lock` pins for `wasm-bindgen`.
    RequireWasmRunner,
    /// The release edits (`release.rs`): the workspace version, the path
    /// dependency pins and the changelog section.
    Release(Version),
    /// `umbrella-api`: `public-api` text of each crate the umbrella exports from,
    /// read for a signature type the umbrella does not name (`gates/umbrella_api.rs`).
    /// With the flag, the lines of each crate's text are also held to its row of
    /// `xtask/public-api-counts.txt` (`gates/public_api_counts.rs`), reusing the texts.
    UmbrellaApi(Option<String>, bool),
    /// `affected <base>`: print the packages the diff from `<base>` can reach as
    /// `-p` flags, or `all` (`gates/affected.rs`).
    Affected(String),
    /// `cargo package --no-verify` over every crate under `crates/`, listed
    /// when the step runs.
    Package,
    /// `fuzz` with no target: the targets, one per line.
    FuzzList,
    /// `fuzz <target>` (or every target for `None`) for `secs` seconds each,
    /// under cargo-fuzz; the targets are the corpus directories, read when the
    /// step runs.
    Fuzz {
        target: Option<String>,
        secs: u32,
    },
}

impl Step {
    /// The step as a line a shell could read (for `--dry-run` and the echo).
    pub(crate) fn display(&self) -> String {
        match self {
            Step::Run(c) | Step::RunAll(c) => c.display(),
            Step::WriteFile { path, .. } => format!("write {}", path.display()),
            Step::Gate(g) => g.display(),
            Step::ConformanceInventory(f) => format!("conformance-inventory {f}"),
            Step::PruneBuilding => "prune-building".to_owned(),
            Step::WritePublicItems => "write-public-items".to_owned(),
            Step::Conformance { results, out } => format!(
                "conformance: join {}/<os>.xml to the inventories, write {}/index.html, ../index.html and ../badges/*.json",
                results.display(),
                out.display()
            ),
            Step::InLlvmCovEnv(c) => format!("(cargo llvm-cov show-env) {}", c.display()),
            Step::RequireLlvmCov => {
                "cargo llvm-cov --version || fail (cargo install cargo-llvm-cov --locked)"
                    .to_owned()
            }
            Step::RequireCargoDeny => {
                "cargo deny --version || fail (cargo install cargo-deny --version 0.20.2 --locked)"
                    .to_owned()
            }
            Step::RequireWasmRunner => {
                "wasm-bindgen-test-runner --version == Cargo.lock's wasm-bindgen, or fail"
                    .to_owned()
            }
            Step::UmbrellaApi(dir, counts) => format!(
                "umbrella-api: cargo public-api --package <crate> --all-features -s for each crate crates/tutti/src/lib.rs exports from and each crate their signatures name{}, then the types those signatures name that the umbrella does not, and the seeds ranked by how many each pulls in{}",
                dir.as_ref().map_or(String::new(), |d| format!(
                    " (the texts of {d}/<package>.txt instead)"
                )),
                if *counts {
                    "; then the lines of each crate under crates/ against xtask/public-api-counts.txt"
                } else {
                    ""
                }
            ),
            Step::Affected(base) => format!(
                "affected: git diff --name-only {base}...HEAD, then the packages it reaches as -p flags (or all)"
            ),
            Step::Package => {
                "cargo package --locked --no-verify -p <each crate under crates/>".to_owned()
            }
            Step::FuzzList => format!("list the directories of {FUZZ_CORPUS}"),
            Step::Fuzz {
                target: Some(t),
                secs,
            } => format!("fuzz {t}: {}", fuzz_cmd(t, *secs).display()),
            Step::Fuzz { target: None, secs } => format!(
                "fuzz all: {} for each directory of {FUZZ_CORPUS}",
                fuzz_cmd("<target>", *secs).display()
            ),
            Step::Release(v) => format!(
                "release {}: set the workspace version and path pins, write the changelog section",
                v.display()
            ),
        }
    }
}

/// `(name, arguments, what it does)`, in the justfile's order.
pub(crate) const RECIPES: &[(&str, &str, &str)] = &[
    (
        "test",
        "[nextest args]",
        "the workspace suite the way CI runs it (nextest; builds plugin-server and plugin-probe first)",
    ),
    (
        "test-doc",
        "[cargo test args]",
        "doctests (nextest does not run them)",
    ),
    (
        "test-release",
        "",
        "the release-profile suites of the lock-free crates",
    ),
    (
        "test-wasm",
        "",
        "run the web suite under node (needs node, `wasm-bindgen-cli` at Cargo.lock's version)",
    ),
    (
        "test-all",
        "",
        "test, test-features, test-doc, test-release, test-wasm",
    ),
    (
        "check-features",
        "",
        "clippy over every member with all its features (needs libjack headers)",
    ),
    (
        "test-features",
        "",
        "run the suites a feature gates, every feature on (see check-features)",
    ),
    ("miri", "", "miri over tutti-rt (needs nightly + miri)"),
    (
        "rtsan",
        "",
        "RealtimeSanitizer over the plugin-host, plugin, contract, engine and cpal suites, in target/rtsan (local only, never CI; Linux or macOS; needs curl, and network on the first build)",
    ),
    (
        "fuzz",
        "[<target>|all] [--secs N]",
        "list the fuzz targets, or run one (or all) under cargo-fuzz for N seconds (default 60; needs cargo-fuzz)",
    ),
    (
        "loom",
        "[<model>...]",
        "the loom models, as CI runs them (or only those named, e.g. handoff_loom)",
    ),
    (
        "pressure",
        "",
        "the out-of-process plugin bridge at real callback pacing",
    ),
    (
        "bench",
        "[criterion args]",
        "every criterion bench, one per line",
    ),
    (
        "bench-save",
        "[NAME=main]",
        "record a named baseline on this machine",
    ),
    (
        "bench-cmp",
        "[BASE=main]",
        "compare against a recorded baseline",
    ),
    (
        "bench-smoke",
        "",
        "run every bench exactly once, no measurement",
    ),
    (
        "profile-bench",
        "[BENCH=engine_render [PKG=tutti-nodes [SECS=10]]]",
        "samply a criterion bench",
    ),
    ("plugin-server", "", "build the plugin-server binary"),
    ("plugin-probe", "", "build the plugin-probe binary"),
    (
        "plugin-server-release",
        "",
        "build plugin-server in release",
    ),
    (
        "check-session",
        "",
        "tutti-session re-exports no crate whole",
    ),
    (
        "check-umbrella",
        "",
        "tutti re-exports no crate whole, offers each item at one path and each name for one crate, bar the rows of xtask/umbrella-collisions-baseline.txt",
    ),
    (
        "check-guards",
        "",
        "every plugin host callback runs under the panic guard",
    ),
    (
        "check-ffi",
        "",
        "unsafe lives only in the -ffi crates (plugin formats and MIDI), everything else forbids it",
    ),
    (
        "check-errors",
        "",
        "every error type derives thiserror::Error, and a public one is an Error",
    ),
    (
        "check-lints",
        "",
        "every workspace member takes the workspace lints and missing_debug_implementations denies",
    ),
    (
        "check-value-traits",
        "",
        "a fieldless public Eq enum derives Hash",
    ),
    (
        "check-layout",
        "",
        "the repo layout, the crate layering, the codec boundary, the banned names of tutti-units and the depth-free test paths",
    ),
    (
        "check-pointers",
        "",
        "no file:line pointer in code prose; one under docs/ names a line that exists",
    ),
    (
        "check-prose-names",
        "",
        "prose names crate paths and design-doc sections that exist, and a backticked item name in it is used by some code (a per-file ratchet)",
    ),
    (
        "check-passive-tests",
        "",
        "a #[test] asserts something; a per-file ratchet",
    ),
    (
        "check-comments",
        "",
        "no comment gains a citation of a design doc, audit, pull request, commit or CLAUDE.md",
    ),
    (
        "check-alloc-binaries",
        "",
        "no top-level test file installs AllocDisabler: allocation tests are modules of the area binary",
    ),
    (
        "check-platform-seam",
        "",
        "a platform cfg stands only on a mod declaration, bar the temporary rows",
    ),
    (
        "check-platform",
        "",
        "`Instant` and `SystemTime::now` come from `tutti_platform`: std's panic on wasm",
    ),
    (
        "check-buffers",
        "",
        "a public item names audio with a fixed sample format only as a row of xtask/buffers-fixed.txt, and a hand interleave loop or frame-times-width length in src only as a row of xtask/buffers-loops.txt",
    ),
    (
        "check-width",
        "",
        "a quantity has one width and data is generic over the graph's float: a crossing is a named place, bar the rows of xtask/width-baseline.txt",
    ),
    (
        "check-session-api",
        "",
        "tutti-session's public API has no `run` or `*_blocking`, and only the rows of xtask/session-api-allow.txt hold a MainThreadMarker",
    ),
    (
        "check-building",
        "",
        "a configured thing is built by the one shape, bar the rows of xtask/building-baseline.txt and the gate's allow-list",
    ),
    (
        "check-values",
        "",
        "a value is written with `set` on a target, bar the rows of xtask/values-baseline.txt",
    ),
    (
        "check-logging",
        "",
        "no `log` dependency, and no print macro outside a main, a test or an example",
    ),
    (
        "check-test-time",
        "",
        "a test asserts no wall-clock time and sleeps for no event; a per-file ratchet",
    ),
    (
        "check-conformance",
        "",
        "spec inventories are well formed, test tags name rows, the CLAP inventory matches clap-sys and the VST3 one the SDK",
    ),
    (
        "conformance",
        "--results <dir> [--out site/conformance]",
        "the spec conformance dashboard and badges (one table per inventory area) from per-OS nextest JUnit files (<dir>/linux.xml, macos.xml, windows.xml)",
    ),
    (
        "coverage",
        "[--out site/coverage] [--package a,b]",
        "line coverage of the workspace as HTML (needs cargo-llvm-cov)",
    ),
    (
        "conformance-inventory",
        "[clap|vst3|midi|sf2]",
        "regenerate an inventory's generated columns from the spec source (vst3: the vendored SDK; midi: TUTTI_MIDI_SPECS; sf2: TUTTI_SF2_SPEC)",
    ),
    (
        "prune-building",
        "",
        "delete the rows of xtask/building-baseline.txt that name no violation; never adds one",
    ),
    (
        "check-public-items",
        "",
        "a crate's public items are the rows of xtask/public-items/<crate>.txt: a new one needs its row, a row with no item is deleted",
    ),
    (
        "write-public-items",
        "",
        "rewrite xtask/public-items/<crate>.txt for every crate from the tree",
    ),
    (
        "check-range-types",
        "",
        "no plain derived Deserialize on a private tuple field",
    ),
    (
        "check-hosting",
        "",
        "every InProcess/OutOfProcess impl is listed in the plugin contract",
    ),
    (
        "check-paths",
        "[<crate> <module>...]",
        "engine types are imported from a crate's root, never through its modules (also searches *.md)",
    ),
    (
        "check-submodule-pins",
        "",
        "the VST3 SDK submodules are at the v3.8.0_build_66 SHAs",
    ),
    (
        "gates",
        "",
        "rustfmt, xtask's own tests and every source gate: no workspace build (the pre-push hook)",
    ),
    ("fmt", "", "cargo fmt --check"),
    ("lint", "", "cargo fmt --check and workspace clippy"),
    (
        "check-windows",
        "",
        "cross-lint for Windows (needs the msvc target, clang, llvm-ar)",
    ),
    (
        "doc",
        "",
        "rustdoc as CI does, per feature, the docs.rs build on a dated nightly, umbrella-api and each crate's public-api count against xtask/public-api-counts.txt (needs cargo-public-api)",
    ),
    ("check-loom", "", "clippy under --cfg loom"),
    (
        "check-wasm",
        "",
        "clippy for wasm32-unknown-unknown: tutti-platform, tutti-rt and tutti, lib only; then tutti with the web features",
    ),
    (
        "check-no-std",
        "",
        "build each no_std crate for a bare-metal target (needs `rustup target add aarch64-unknown-none`)",
    ),
    (
        "verify-audio",
        "[OUT=/tmp/tutti-verify]",
        "render the audio cases and verify them with the Python checks (needs uv)",
    ),
    ("deny", "", "RustSec advisories (needs cargo-deny)"),
    (
        "public-api",
        "<crate> [<base>]",
        "a crate's public API with all its features on the docs nightly, or its diff from <base> \
         to HEAD (needs cargo-public-api; the diff checks each commit out in place, refuses a \
         dirty tree and restores the checkout)",
    ),
    (
        "umbrella-api",
        "[<dir>]",
        "the types in the signatures of what tutti exports that tutti does not name (needs cargo-public-api; `doc` and `ci` run it, `gates` does not)",
    ),
    (
        "book",
        "[OUT]",
        "render the book's sounds into book/src/sounds, then build it with mdbook into book/build, or into OUT",
    ),
    (
        "affected",
        "<base>",
        "the packages the diff <base>...HEAD can reach (changed crates and everything that depends on them, dev dependencies included) as `-p` flags, or `all`; CI's test selection",
    ),
    (
        "package",
        "",
        "package every crate under crates/ the way crates.io takes it, without building",
    ),
    (
        "release",
        "<version>",
        "a release pull request's edits: the shared version, the path pins and the changelog section",
    ),
    ("ci", "", "everything CI runs"),
];

/// `--key value` pairs, each key one of `allowed` and given once.
fn options(
    recipe: &str,
    args: &[String],
    allowed: &[&str],
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut it = args.iter();
    while let Some(key) = it.next() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "`{recipe}` takes {}, got `{key}`",
                allowed.join(", ")
            ));
        }
        let value = it
            .next()
            .ok_or_else(|| format!("`{recipe}`: `{key}` needs a value"))?;
        if out.insert(key.clone(), value.clone()).is_some() {
            return Err(format!("`{recipe}`: `{key}` is given twice"));
        }
    }
    Ok(out)
}

/// Plans `name` with `args`; an error is a usage error (exit 2).
pub(crate) fn plan(name: &str, args: &[String], ctx: &Ctx) -> Result<Vec<Step>, String> {
    let mut p = Planner {
        ctx,
        done: HashSet::new(),
        out: Vec::new(),
    };
    p.done.insert(name.to_owned());
    p.recipe(name, args)?;
    Ok(p.out)
}

struct Planner<'a> {
    ctx: &'a Ctx,
    /// Recipes already planned in this invocation: a dependency runs once.
    done: HashSet<String>,
    out: Vec<Step>,
}

const TARGET: &str = "x86_64-pc-windows-msvc";

/// The dated nightly CI pins for the docs.rs build; `public-api` reads rustdoc
/// JSON from the same one.
const DOCS_NIGHTLY: &str = "nightly-2026-10-14";

/// The fuzz package's corpus directory: one subdirectory of seeds per target.
pub(crate) const FUZZ_CORPUS: &str = "testing/tutti-fuzz/corpus";

/// Where cargo-fuzz keeps the inputs a run finds, one subdirectory per target;
/// ignored by git.
pub(crate) const FUZZ_WORK: &str = "testing/tutti-fuzz/fuzz/corpus";

/// The seconds `fuzz` runs a target for when `--secs` is absent.
const FUZZ_SECS: u32 = 60;

/// The cargo-fuzz run of `target`: new inputs go to the ignored
/// `fuzz/corpus/<target>`, the committed seeds are read beside them, and a crash
/// lands in `fuzz/artifacts/<target>`.
pub(crate) fn fuzz_cmd(target: &str, secs: u32) -> Cmd {
    Cmd::sh(&format!(
        "cargo +{DOCS_NIGHTLY} fuzz run --fuzz-dir testing/tutti-fuzz/fuzz {target} \
         {FUZZ_WORK}/{target} {FUZZ_CORPUS}/{target} -- -max_total_time={secs}"
    ))
}

/// The command that lists `package`'s public API with all its features on, for
/// the `public-api` and `umbrella-api` recipes.
pub(crate) fn public_api_cmd(package: &str) -> String {
    // `-s` omits blanket impls only: a lost auto-trait impl is a break.
    format!("cargo +{DOCS_NIGHTLY} public-api --package {package} --all-features -s")
}

/// The `tutti-book` examples that write a sound the book plays, each run with
/// `book/src/sounds` as its one argument.
const BOOK_SOUNDS: &[&str] = &["getting_started"];

impl Planner<'_> {
    fn run(&mut self, c: Cmd) {
        self.out.push(Step::Run(c));
    }

    fn lines<S: AsRef<str>>(&mut self, lines: &[S]) {
        for l in lines {
            self.run(Cmd::sh(l.as_ref()));
        }
    }

    /// Each crate of `NO_STD_CRATES` built for `NO_STD_TARGET` without its default
    /// features, and once more with each feature it lists: a `std` use in the crate
    /// has no such target to link against, so the build fails.
    fn check_no_std(&mut self) {
        for (krate, features) in NO_STD_CRATES {
            let build = format!(
                "cargo build --locked -p {krate} --no-default-features --target {NO_STD_TARGET}"
            );
            self.run(Cmd::sh(&build));
            for f in *features {
                self.run(Cmd::sh(&format!("{build} --features {f}")));
            }
        }
    }

    /// The suites that reach code `RtScope`'s own checks cannot see (a plugin's C, a
    /// dependency), built and run with `RTSAN_ENABLE` into `<target>/rtsan`, so the
    /// normal build never rebuilds. The two plugin binaries are built there too, since a
    /// test finds them beside its own binary.
    fn rtsan(&mut self) {
        let dir = self.ctx.target.join("rtsan").display().to_string();
        for (i, line) in RTSAN.iter().enumerate() {
            let cmd = Cmd::sh(line)
                .env("RTSAN_ENABLE", "1")
                .env("CARGO_TARGET_DIR", dir.clone());
            // The builds stop the recipe; the suites each run and report.
            self.out.push(if i < 2 {
                Step::Run(cmd)
            } else {
                Step::RunAll(cmd)
            });
        }
    }

    /// A dependency: planned once per invocation, with no arguments.
    fn dep(&mut self, name: &str) -> Result<(), String> {
        if self.done.insert(name.to_owned()) {
            self.recipe(name, &[])?;
        }
        Ok(())
    }

    /// Exactly `N` positional parameters, each with its default.
    fn params<const N: usize>(
        name: &str,
        args: &[String],
        defaults: [&str; N],
    ) -> Result<[String; N], String> {
        if args.len() > N {
            return Err(format!(
                "`{name}` takes at most {N} argument(s), got {}",
                args.len()
            ));
        }
        Ok(std::array::from_fn(|i| {
            args.get(i)
                .cloned()
                .unwrap_or_else(|| defaults[i].to_owned())
        }))
    }

    fn recipe(&mut self, name: &str, args: &[String]) -> Result<(), String> {
        if !RECIPES.iter().any(|r| r.0 == name) {
            return Err(format!("no recipe `{name}`; `cargo xtask help` lists them"));
        }
        // Only a recipe with an argument column takes free-form arguments or named parameters.
        let takes_args = RECIPES.iter().any(|r| r.0 == name && !r.1.is_empty());
        if !takes_args && !args.is_empty() {
            return Err(format!(
                "`{name}` takes no arguments, got {}",
                args.join(" ")
            ));
        }
        if let Some(gate) = Gate::of(name, args) {
            self.out.push(Step::Gate(gate));
            return Ok(());
        }
        match name {
            "test" => {
                self.dep("plugin-server")?;
                self.dep("plugin-probe")?;
                self.run(Cmd::sh("cargo nextest run --locked --workspace").args(args));
            }
            "test-doc" => {
                self.run(Cmd::sh("cargo test --locked --doc --workspace").args(args));
                self.run(Cmd::sh("cargo test --locked --doc -p tutti-session --all-features").args(args));
            }
            "test-release" => self.lines(&[
                "cargo nextest run --locked --release -p tutti-units -p tutti-rt -p tutti-graph",
            ]),
            "test-all" => {
                for d in ["test", "test-features", "test-doc", "test-release", "test-wasm"] {
                    self.dep(d)?;
                }
            }
            "test-wasm" => {
                self.out.push(Step::RequireWasmRunner);
                self.lines(&[
                    "cargo test --locked --target wasm32-unknown-unknown -p tutti-web-test",
                ]);
            }
            "check-features" => self.lines(CHECK_FEATURES),
            "test-features" => self.lines(TEST_FEATURES),
            "miri" => self.lines(MIRI),
            "rtsan" => {
                if cfg!(windows) {
                    return Err("`rtsan` runs on Linux and macOS only: RealtimeSanitizer has no Windows runtime".into());
                }
                self.rtsan();
            }
            "fuzz" => {
                let (target, rest) = match args.split_first() {
                    Some((t, rest)) if !t.starts_with("--") => (Some(t.as_str()), rest),
                    _ => (None, args),
                };
                let o = options(name, rest, &["--secs"])?;
                let secs = match o.get("--secs") {
                    Some(n) => n
                        .parse::<u32>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("`fuzz`: `--secs` is a positive whole number, got `{n}`"))?,
                    None => FUZZ_SECS,
                };
                match target {
                    None if o.is_empty() => self.out.push(Step::FuzzList),
                    None => return Err("`fuzz --secs` needs a target or `all`".into()),
                    Some(t) if !t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') => {
                        return Err(format!("`fuzz`: `{t}` is not a target name"));
                    }
                    Some("all") => self.out.push(Step::Fuzz { target: None, secs }),
                    Some(t) => self.out.push(Step::Fuzz { target: Some(t.to_owned()), secs }),
                }
            }
            "loom" if args.is_empty() => self.lines(LOOM),
            "loom" => {
                // The whole name: `--test pos_ring_loom` names no `pos_ring`.
                let runs = |l: &str, a: &str| {
                    l.contains(&format!("--test {a} ")) || l.ends_with(&format!("--test {a}"))
                };
                if let Some(unknown) = args.iter().find(|a| !LOOM.iter().any(|l| runs(l, a))) {
                    return Err(format!("`loom` runs no model `{unknown}`"));
                }
                let chosen: Vec<&str> = LOOM
                    .iter()
                    .copied()
                    .filter(|l| args.iter().any(|a| runs(l, a)))
                    .collect();
                self.lines(&chosen);
            }
            "pressure" => self.lines(&[
                "cargo build --locked -p tutti-plugin-server",
                "cargo nextest run --locked -p tutti-plugin --features clap --test real_plugin_pressure --no-capture",
            ]),
            "bench" => self.bench(args),
            "bench-save" => {
                let [n] = Self::params(name, args, ["main"])?;
                self.bench(&["--".into(), "--save-baseline".into(), n]);
            }
            "bench-cmp" => {
                let [b] = Self::params(name, args, ["main"])?;
                self.bench(&["--".into(), "--baseline".into(), b]);
            }
            "bench-smoke" => {
                self.bench(&["--".into(), "--test".into()]);
            }
            "profile-bench" => {
                let [bench, pkg, secs] =
                    Self::params(name, args, ["engine_render", "tutti-nodes", "10"])?;
                self.run(
                    Cmd::sh("cargo bench --locked -p")
                        .arg(&pkg)
                        .args(["--bench", &bench, "--profile", "profiling", "--no-run"]),
                );
                let mut samply = Cmd::sh("samply record")
                    .arg(format!("target/profiling/build/{pkg}/*/out/{bench}-*"))
                    .args(["--bench", "--profile-time", &secs]);
                samply.glob = true;
                self.run(samply);
            }
            "plugin-server" => self.lines(&["cargo build --locked -p tutti-plugin-server --bin plugin-server"]),
            "plugin-probe" => self.lines(&["cargo build --locked -p tutti-plugin-probe --bin plugin-probe"]),
            "plugin-server-release" => {
                self.lines(&["cargo build --locked --release -p tutti-plugin-server --bin plugin-server"]);
            }
            "conformance" => {
                let o = options(name, args, &["--results", "--out"])?;
                let results = o.get("--results").map(String::as_str).ok_or("`conformance` needs `--results <dir>`")?;
                let out = o.get("--out").map_or("site/conformance", String::as_str);
                self.out.push(Step::Conformance {
                    results: self.ctx.root.join(results),
                    out: self.ctx.root.join(out),
                });
            }
            "coverage" => {
                let o = options(name, args, &["--out", "--package"])?;
                let out = o.get("--out").map_or("site/coverage", String::as_str);
                self.out.push(Step::RequireLlvmCov);
                // The fixtures look for the plugin binaries beside the running
                // test binary, which llvm-cov builds under its own target
                // directory: build them there.
                for bin in ["plugin-server", "plugin-probe"] {
                    self.out.push(Step::InLlvmCovEnv(Cmd::sh(&format!(
                        "cargo build --locked -p tutti-{bin} --bin {bin}"
                    ))));
                }
                let mut run = Cmd::sh("cargo llvm-cov nextest --locked --ignore-run-fail");
                match o.get("--package") {
                    Some(list) => {
                        for p in list.split(',') {
                            run = run.arg("-p").arg(p);
                        }
                    }
                    None => run = run.arg("--workspace"),
                }
                self.run(run.arg("--html").arg("--output-dir").arg(out));
            }
            "conformance-inventory" => {
                let [f] = Self::params(name, args, ["clap"])?;
                self.out.push(Step::ConformanceInventory(f));
            }
            "prune-building" => self.out.push(Step::PruneBuilding),
            "write-public-items" => self.out.push(Step::WritePublicItems),
            "fmt" => self.lines(&["cargo fmt --all --check"]),
            "lint" => {
                self.dep("fmt")?;
                self.lines(&["cargo clippy --locked --workspace --all-targets -- -D warnings"]);
            }
            "check-windows" => self.check_windows(),
            "doc" => {
                self.lines(&[
                    r#"RUSTDOCFLAGS="-D warnings --document-private-items" cargo doc --locked --workspace --no-deps --all-features"#,
                ]);
                self.out.push(Step::UmbrellaApi(None, true));
            }
            "check-no-std" => self.check_no_std(),
            "check-loom" => self.lines(CHECK_LOOM),
            "check-wasm" => self.lines(CHECK_WASM),
            "book" => {
                let [out] = Self::params(name, args, [""])?;
                for example in BOOK_SOUNDS {
                    self.lines(&[format!(
                        "cargo run --locked --release -p tutti-book --example {example} -- book/src/sounds"
                    )]);
                }
                if out.is_empty() {
                    self.lines(&["mdbook build book"]);
                } else {
                    self.lines(&[format!("mdbook build book --dest-dir {out}")]);
                }
            }
            "verify-audio" => {
                let [out] = Self::params(name, args, ["/tmp/tutti-verify"])?;
                self.lines(&[
                    format!("cargo run --locked --release -p tutti-verify --bin render_sampler_cases -- {out}/sampler"),
                    format!("cargo run --locked --release -p tutti-verify --bin render_export_cases -- {out}/export"),
                    format!("cargo run --locked --release -p tutti-verify --bin render_analysis_cases -- {out}/analysis"),
                    format!("cargo run --locked --release -p tutti-verify --bin render_synth_cases -- {out}/synth"),
                    format!("cargo run --locked --release -p tutti-verify --bin render_dsp_cases -- {out}/dsp"),
                    "uv sync --quiet".to_owned(),
                    format!("uv run python testing/tutti-verify/py/verify_sampler.py {out}/sampler"),
                    format!("uv run python testing/tutti-verify/py/verify_export.py {out}/export"),
                    format!("uv run python testing/tutti-verify/py/verify_analysis.py {out}/analysis"),
                    format!("uv run python testing/tutti-verify/py/verify_synth.py {out}/synth"),
                    format!("uv run python testing/tutti-verify/py/verify_dsp.py {out}/dsp"),
                ]);
            }
            "release" => {
                let [version] = Self::params(name, args, [""])?;
                let v = Version::parse(&version).ok_or_else(|| {
                    format!("`release` takes a version MAJOR.MINOR.PATCH, got `{version}`")
                })?;
                self.out.push(Step::Release(v));
            }
            "package" => self.out.push(Step::Package),
            "affected" => {
                let [base] = Self::params(name, args, [""])?;
                if base.is_empty() {
                    return Err("`affected` takes a base: `cargo xtask affected <base>`".to_owned());
                }
                self.out.push(Step::Affected(base));
            }
            "umbrella-api" => {
                let [dir] = Self::params(name, args, [""])?;
                self.out.push(Step::UmbrellaApi((!dir.is_empty()).then_some(dir), false));
            }
            "public-api" => {
                let [krate, base] = Self::params(name, args, ["", ""])?;
                if krate.is_empty() {
                    return Err(
                        "`public-api` takes a crate: `cargo xtask public-api <crate> [<base>]`"
                            .to_owned(),
                    );
                }
                let list = public_api_cmd(&krate);
                if base.is_empty() {
                    self.lines(&[list]);
                } else {
                    self.lines(&[format!("{list} diff {base}..HEAD")]);
                }
            }
            "deny" => {
                self.out.push(Step::RequireCargoDeny);
                self.lines(&["cargo deny --locked check advisories"]);
            }
            "gates" => {
                self.dep("fmt")?;
                self.lines(&["cargo test --locked -p xtask"]);
                for g in GATES.iter().filter(|g| g.local) {
                    self.dep(g.recipe)?;
                }
            }
            "ci" => {
                for d in [
                    "gates", "lint", "deny", "check-conformance",
                    "check-features", "package",
                    "check-no-std", "check-loom", "check-wasm", "test", "test-features", "test-doc",
                    "test-release", "test-wasm", "doc",
                ] {
                    self.dep(d)?;
                }
            }
            _ => unreachable!("RECIPES and this match list the same names"),
        }
        Ok(())
    }

    /// Every bench, one per line; `args` follow each.
    fn bench(&mut self, args: &[String]) {
        for l in BENCH {
            self.run(Cmd::sh(l).args(args));
        }
    }

    /// The justfile wrote the stub header to `${TMPDIR:-/tmp}/tutti-winstub`;
    /// it is under the target directory now, so a run touches nothing outside
    /// the repo's build output.
    fn check_windows(&mut self) {
        let stub = self.ctx.target.join("tutti-winstub");
        self.out.push(Step::WriteFile {
            path: stub.join("malloc.h"),
            contents: "#include <stddef.h>\nvoid *_alloca(size_t);\n".to_owned(),
        });
        let cflags = format!("-I{}", stub.display());
        let mut win = |args: &str| {
            self.out.push(Step::Run(
                Cmd::sh(&format!(
                    "cargo clippy --locked --target {TARGET} {args} -- -D warnings"
                ))
                .env("CC_x86_64_pc_windows_msvc", "clang")
                .env("AR_x86_64_pc_windows_msvc", "llvm-ar")
                .env("CFLAGS_x86_64_pc_windows_msvc", cflags.clone()),
            ));
        };
        for a in WINDOWS {
            win(a);
        }
    }
}

/// The arguments of each `cargo clippy --target {TARGET}` of `check-windows`: the workspace
/// minus what Windows cannot build, then the crates built with their features.
const WINDOWS: &[&str] = &[
    "--workspace --all-targets --exclude tutti-codec --exclude tutti-export --exclude tutti --exclude tutti-vst3-host --exclude tutti-plugin-server --exclude tutti-plugin-probe --exclude tutti-shared-plugin-vst3-cpp --exclude tutti-plugin-contract --exclude tutti-verify",
    "-p tutti-shared-plugin --all-targets",
    "-p tutti-export --lib",
    "-p tutti-codec --features encode-wav,encode-aiff,encode-flac,decode-wav,decode-aiff,decode-flac,decode-mp3,decode-ogg --all-targets",
    "-p tutti-plugin -p tutti-plugin-types --features tutti-plugin/clap,tutti-plugin/vst3,tutti-plugin/vst2,tutti-plugin/serde --all-targets",
    "-p tutti-plugin-db --features serde --all-targets",
    "-p tutti-vst2-host --all-targets",
    "-p tutti-vst3-host -p tutti-plugin-server -p tutti-plugin-probe --lib --bins",
    "-p tutti-winmm-ffi -p tutti-midi-hardware --features tutti-winmm-ffi/synth-tests,tutti-midi-hardware/synth-tests --all-targets",
];

/// A target with no `std`: a crate that builds for it uses only `core` and `alloc`.
const NO_STD_TARGET: &str = "aarch64-unknown-none";

/// The `#![no_std]` crates in layer order, each with the features built on top of
/// the bare build. A crate that becomes `no_std` is one line here.
const NO_STD_CRATES: &[(&str, &[&str])] = &[
    ("tutti-units", &["serde"]),
    ("tutti-music", &["serde"]),
    ("tutti-ump", &["serde"]),
    ("tutti-midi-protocol", &["serde"]),
    ("tutti-curve", &[]),
    ("tutti-buffers", &["serde"]),
    ("tutti-tempo", &["serde"]),
    ("tutti-voicing", &[]),
];

const CHECK_LOOM: &[&str] = &[
    r#"RUSTFLAGS="--cfg loom" cargo clippy --locked -p tutti-rt -p tutti-plugin-ipc-ffi -p tutti-winmm-ffi --all-targets -- -D warnings"#,
];

const CHECK_WASM: &[&str] = &[
    "cargo clippy --locked --target wasm32-unknown-unknown -p tutti-platform -p tutti-rt -p tutti --lib -- -D warnings",
    "cargo clippy --locked --target wasm32-unknown-unknown -p tutti --lib --features sampler,soundfont,synth,analyze,decode-wav,decode-aiff,decode-flac,decode-mp3,decode-ogg,encode-wav,encode-flac,encode-aiff,export,record,midi,midi-network,serde -- -D warnings",
];

const CHECK_FEATURES: &[&str] = &[
    r#"cargo clippy --locked --workspace --all-features --all-targets -- -D warnings"#,
    // Each encoder alone: a codec test helper compiles only where a test uses it.
    r#"cargo clippy --locked -p tutti-codec --no-default-features --features encode-wav --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --no-default-features --features encode-aiff --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-codec --no-default-features --features encode-flac --all-targets -- -D warnings"#,
    // The session's lib alone, where no dev-dependency unifies a codec feature in.
    r#"cargo clippy --locked -p tutti-session --all-features --lib -- -D warnings"#,
    // A subsystem feature alone: its test binaries compile only what it enables.
    r#"cargo clippy --locked -p tutti-session --no-default-features --features clap --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --no-default-features --features vst3 --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --no-default-features --features plugin --all-targets -- -D warnings"#,
    r#"cargo clippy --locked -p tutti-session --no-default-features --features midi --all-targets -- -D warnings"#,
];

const TEST_FEATURES: &[&str] = &[
    r#"cargo build --locked -p tutti-plugin-server --no-default-features --features clap,vst3"#,
    r#"cargo build --locked -p tutti-plugin-probe"#,
    r#"cargo nextest run --locked --all-features -p tutti-cpal -p tutti-session -p tutti -p tutti-codec -p tutti-ump -p tutti-midi-protocol -p tutti-midi-ci -p tutti-midi-device -p tutti-tempo -p tutti-measure -p tutti-buffers -p tutti-plugin-types -p tutti-plugin-db -p tutti-plugin-contract"#,
    r#"cargo nextest run --locked -p tutti-midi-hardware --all-features"#,
    r#"cargo build --locked -p tutti-plugin-server"#,
    r#"cargo nextest run --locked -p tutti-plugin --features clap,vst2,serde"#,
];

const MIRI: &[&str] = &[
    r#"cargo +nightly miri test --locked -p tutti-rt --lib"#,
    r#"MIRIFLAGS="-Zmiri-many-seeds=0..16" cargo +nightly miri test --locked -p tutti-rt --lib ring"#,
];

const RTSAN: &[&str] = &[
    r#"cargo build --locked -p tutti-plugin-server --bin plugin-server"#,
    r#"cargo build --locked -p tutti-plugin-probe --bin plugin-probe"#,
    r#"cargo nextest run --locked --no-fail-fast --features tutti-rt/rtsan -p tutti-engine -p tutti-cpal -p tutti-plugin-host -p tutti-plugin-contract --all-features"#,
    r#"cargo nextest run --locked --no-fail-fast --features tutti-rt/rtsan,tutti-plugin/clap,tutti-plugin/vst2,tutti-plugin/serde -p tutti-plugin"#,
];

const LOOM: &[&str] = &[
    r#"LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test pos_ring_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test atomic_cell_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test seq_cell_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test cell_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test handoff_loom -- --nocapture"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-plugin-ipc-ffi --release --lib loom_model"#,
    // `LOOM_MAX_PREEMPTIONS=3` bounds this to under a second; unbounded it ran past 60 s.
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-winmm-ffi --release --lib loom_model"#,
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test split_loom"#,
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test job_gate_loom"#,
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test task_graph_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test parker_loom"#,
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test bells_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test level_cell_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test request_flags_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test dirty_set_loom"#,
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test ring_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test drain_clock_loom -- --nocapture"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test fault_latch_loom -- --nocapture"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test gauge_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test render_count_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test render_gate_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test claim_loom"#,
    r#"RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test last_out_loom"#,
    r#"LOOM_MAX_PREEMPTIONS=3 RUSTFLAGS="--cfg loom" cargo test --locked -p tutti-rt --release --test seq_slots_loom"#,
];

/// The hand lists the recipes hold, for the rows `check-layout` checks against the
/// workspace (`gates/recipe_lists.rs`).
pub(crate) fn listed() -> crate::gates::Listed {
    crate::gates::Listed {
        no_std: NO_STD_CRATES.iter().map(|(c, _)| *c).collect(),
        commands: [
            CHECK_FEATURES,
            TEST_FEATURES,
            MIRI,
            RTSAN,
            LOOM,
            BENCH,
            CHECK_LOOM,
            CHECK_WASM,
            WINDOWS,
        ]
        .concat(),
        book_sounds: BOOK_SOUNDS,
    }
}

/// Adding a bench means adding a line here: there is no sweep that would pick
/// one up silently (the justfile's contract, which CI mirrors).
const BENCH: &[&str] = &[
    r#"cargo bench --locked -p tutti-nodes     --bench engine_render"#,
    r#"cargo bench --locked -p tutti-graph     --bench graph_render"#,
    r#"cargo bench --locked -p tutti-graph     --bench tap_lanes"#,
    r#"cargo bench --locked -p tutti-cpal      --bench audio_callback"#,
    r#"cargo bench --locked -p tutti-export    --bench offline_render"#,
    r#"cargo bench --locked -p tutti-polysynth --bench polysynth"#,
    r#"cargo bench --locked -p tutti-ump       --bench codec"#,
    r#"cargo bench --locked -p tutti-midi-runtime --bench clip_node"#,
    r#"cargo bench --locked -p tutti-sampler   --bench voice_pool"#,
    r#"cargo bench --locked -p tutti-plugin-host --bench vst2_in_process"#,
];

#[cfg(test)]
mod tests;

/// The `version` of the package `name` in a `Cargo.lock` text, if it has one.
pub(crate) fn locked_version(lock: &str, name: &str) -> Option<String> {
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() == format!("name = \"{name}\"") {
            let version = lines.next()?.trim().strip_prefix("version = ")?;
            return Some(version.trim_matches('"').to_owned());
        }
    }
    None
}

/// Whether the runner's `--version` output (`wasm-bindgen-test-runner 0.2.130`) names
/// the `wasm-bindgen` version `Cargo.lock` holds; the error is the message to print.
pub(crate) fn check_wasm_runner(lock: &str, runner_output: &str) -> Result<(), String> {
    let locked = locked_version(lock, "wasm-bindgen")
        .ok_or("Cargo.lock has no wasm-bindgen package".to_owned())?;
    let found = runner_output.split_whitespace().last().unwrap_or_default();
    if found == locked {
        Ok(())
    } else {
        Err(format!(
            "wasm-bindgen-test-runner {found} does not match Cargo.lock's wasm-bindgen {locked}: install it with cargo install --locked wasm-bindgen-cli --version {locked}"
        ))
    }
}
