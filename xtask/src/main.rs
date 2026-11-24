//! `cargo xtask <recipe> [args]`: the repo's development commands, one
//! subcommand per recipe the justfile had, with the same names, commands,
//! environment and order. `cargo xtask help` lists them; `--dry-run` (before
//! the recipe) prints what would run.
//!
//! Std only, and no workspace dependency: the tool that builds and gates the
//! workspace does not depend on what it builds (`cargo xtask check-layout`).

#![forbid(unsafe_code)]

mod cmd;
mod gates;
mod recipes;
mod release;

use cmd::Cmd;
use recipes::{Ctx, RECIPES, Step};
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Help,
    Run {
        dry_run: bool,
        recipe: String,
        args: Vec<String>,
    },
}

/// `--dry-run` is read only before the recipe: after it, every word is the
/// recipe's own (`bench -- --test`).
fn parse(argv: &[String]) -> Result<Action, String> {
    let mut dry_run = false;
    let mut it = argv.iter().peekable();
    while let Some(a) = it.peek() {
        match a.as_str() {
            "--dry-run" | "-n" => {
                dry_run = true;
                it.next();
            }
            _ => break,
        }
    }
    match it.next().map(String::as_str) {
        None | Some("help" | "-h" | "--help") => Ok(Action::Help),
        Some(r) if r.starts_with('-') => Err(format!("unknown option `{r}`")),
        Some(r) => Ok(Action::Run {
            dry_run,
            recipe: r.to_owned(),
            args: it.cloned().collect(),
        }),
    }
}

fn help() -> String {
    let mut s = String::from(
        "usage: cargo xtask [--dry-run] <recipe> [args]\n\n  --dry-run   print the commands a recipe would run, run none\n\nrecipes:\n",
    );
    let heads: Vec<String> = RECIPES
        .iter()
        .map(|(name, args, _)| {
            if args.is_empty() {
                (*name).to_owned()
            } else {
                format!("{name} {args}")
            }
        })
        .collect();
    let width = heads.iter().map(String::len).max().unwrap_or(0);
    for (head, (_, _, doc)) in heads.iter().zip(RECIPES) {
        s.push_str(&format!("  {head:<width$}  {doc}\n"));
    }
    s
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (dry_run, recipe, args) = match parse(&argv) {
        Ok(Action::Help) => {
            print!("{}", help());
            return ExitCode::SUCCESS;
        }
        Ok(Action::Run {
            dry_run,
            recipe,
            args,
        }) => (dry_run, recipe, args),
        Err(e) => {
            eprintln!("xtask: {e}\n\n{}", help());
            return ExitCode::from(2);
        }
    };
    // The root is read at run time (`cargo run` and the `xtask` alias set
    // CARGO_MANIFEST_DIR), never only from `env!`: sccache's key does not hold
    // the path, so a binary it serves can carry another worktree's path, and
    // every gate would then read that tree. `env!` is the fallback for a binary
    // run by hand.
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from);
    let root = manifest_dir
        .parent()
        .expect("xtask sits directly under the repo root")
        .to_path_buf();
    let target = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(t) => root.join(t), // an absolute path replaces the root
        None => root.join("target"),
    };
    let ctx = Ctx { root, target };
    let steps = match recipes::plan(&recipe, &args, &ctx) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("xtask: {e}");
            return ExitCode::from(2);
        }
    };
    let mut deferred = None;
    for step in &steps {
        if dry_run {
            println!("{}", step.display());
            continue;
        }
        eprintln!("+ {}", step.display());
        if let Err(code) = execute(step, &ctx) {
            if matches!(step, Step::RunAll(_)) {
                deferred.get_or_insert(code);
            } else {
                return ExitCode::from(code);
            }
        }
    }
    deferred.map_or(ExitCode::SUCCESS, ExitCode::from)
}

/// Runs one step; an error is the exit code to stop with.
fn execute(step: &Step, ctx: &Ctx) -> Result<(), u8> {
    match step {
        Step::Run(c) | Step::RunAll(c) => run(c, ctx),
        Step::WriteFile { path, contents } => {
            let dir = path.parent().expect("a file path has a parent");
            std::fs::create_dir_all(dir)
                .and_then(|()| std::fs::write(path, contents))
                .map_err(|e| {
                    eprintln!("xtask: cannot write {}: {e}", path.display());
                    1
                })
        }
        Step::Gate(g) => {
            let report = g.run(&ctx.root);
            for l in &report.out {
                println!("{l}");
            }
            if report.passed() { Ok(()) } else { Err(1) }
        }
        Step::Affected(base) => match gates::affected(&ctx.root, base) {
            Ok(selection) => {
                println!("{selection}");
                Ok(())
            }
            Err(e) => {
                eprintln!("xtask: affected: {e}");
                Err(1)
            }
        },
        Step::Package => package(ctx),
        Step::FuzzList => {
            for t in fuzz_targets(ctx)? {
                println!("{t}");
            }
            Ok(())
        }
        Step::Fuzz { target, secs } => fuzz(target.as_deref(), *secs, ctx),
        Step::UmbrellaApi(dir) => umbrella_api(ctx, dir.as_deref()),
        Step::UnusedPublic { crates, out, app } => unused_public(ctx, crates, out.as_deref(), *app),
        Step::Release(v) => release::run(&ctx.root, v).map_err(|e| {
            eprintln!("xtask: release: {e}");
            1
        }),
        Step::ConformanceInventory(format) => gates::conformance_inventory(&ctx.root, format)
            .map_err(|e| {
                eprintln!("xtask: {e}");
                1
            }),
        Step::PruneBuilding => gates::prune_building(&ctx.root).map_err(|e| {
            eprintln!("xtask: {e}");
            1
        }),
        Step::WritePublicItems => gates::write_public_items(&ctx.root).map_err(|e| {
            eprintln!("xtask: {e}");
            1
        }),
        Step::Conformance { results, out } => gates::conformance_site(&ctx.root, results, out)
            .map_err(|e| {
                eprintln!("xtask: {e}");
                1
            }),
        Step::InLlvmCovEnv(c) => {
            let shown = process::Command::new("cargo")
                .args(["llvm-cov", "show-env"])
                .current_dir(&ctx.root)
                .output()
                .map_err(|e| {
                    eprintln!("xtask: cannot run `cargo llvm-cov show-env`: {e}");
                    1
                })?;
            if !shown.status.success() {
                eprintln!("xtask: `cargo llvm-cov show-env` failed");
                return Err(1);
            }
            let mut c = c.clone();
            for line in String::from_utf8_lossy(&shown.stdout).lines() {
                if let Some((k, v)) = line.split_once('=') {
                    c = c.env(k.trim(), v.trim().trim_matches(['"', '\'']));
                }
            }
            run(&c, ctx)
        }
        Step::RequireLlvmCov => {
            let ok = process::Command::new("cargo")
                .args(["llvm-cov", "--version"])
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                Ok(())
            } else {
                eprintln!(
                    "cargo-llvm-cov is not installed; run: cargo install cargo-llvm-cov --locked"
                );
                Err(1)
            }
        }
        Step::RequireWasmRunner => {
            let lock = std::fs::read_to_string(ctx.root.join("Cargo.lock")).map_err(|e| {
                eprintln!("xtask: cannot read Cargo.lock: {e}");
                1
            })?;
            let out = process::Command::new("wasm-bindgen-test-runner")
                .arg("--version")
                .output()
                .map_err(|e| {
                    eprintln!(
                        "xtask: wasm-bindgen-test-runner is not installed ({e}); run: cargo install --locked wasm-bindgen-cli --version <the wasm-bindgen version in Cargo.lock>"
                    );
                    1
                })?;
            recipes::check_wasm_runner(&lock, &String::from_utf8_lossy(&out.stdout)).map_err(|m| {
                eprintln!("xtask: {m}");
                1
            })
        }
        Step::RequireCargoDeny => {
            let ok = process::Command::new("cargo")
                .args(["deny", "--version"])
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                Ok(())
            } else {
                eprintln!(
                    "cargo-deny is not installed; run: cargo install cargo-deny --version 0.20.2 --locked"
                );
                Err(1)
            }
        }
    }
}

/// `umbrella-api [<dir>]`: the unnameable types of what the umbrella exports and
/// the seeds ranked by how many each pulls in. The texts come from `public-api`
/// for each crate the umbrella exports from and each crate their signatures name,
/// or from `<dir>/<package>.txt` when a directory is given. Without a directory, the lines of
/// every crate under `crates/` are then printed as a table, the texts already read reused.
fn umbrella_api(ctx: &Ctx, dir: Option<&str>) -> Result<(), u8> {
    let lib_path = ctx.root.join("crates/tutti/src/lib.rs");
    let lib = std::fs::read_to_string(&lib_path).map_err(|e| {
        eprintln!("xtask: cannot read {}: {e}", lib_path.display());
        1
    })?;
    let texts: std::cell::RefCell<std::collections::BTreeMap<String, String>> = Default::default();
    let fetch_text = |package: &str| -> Result<Option<String>, String> {
        if let Some(dir) = dir {
            return match std::fs::read_to_string(Path::new(dir).join(format!("{package}.txt"))) {
                Ok(t) => Ok(Some(t)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            };
        }
        let is_package = ["crates", "testing"]
            .iter()
            .any(|d| ctx.root.join(d).join(package).join("Cargo.toml").is_file());
        if !is_package {
            return Ok(None);
        }
        let c = Cmd::sh(&recipes::public_api_cmd(package));
        println!("+ {}", c.display());
        let out = command(&c, ctx)
            .output()
            .map_err(|e| format!("cannot run `{}`: {e}", c.display()))?;
        if !out.status.success() {
            return Err(format!(
                "`{}` failed ({}):\n{}",
                c.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim_end()
            ));
        }
        Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
    };
    // `None` is a crate with no text: not a package, or no file in `dir`.
    let fetch = |krate: &str| -> Result<Option<String>, String> {
        let package = krate.replace('_', "-");
        if let Some(t) = texts.borrow().get(&package) {
            return Ok(Some(t.clone()));
        }
        let text = fetch_text(&package)?;
        if let Some(t) = &text {
            texts.borrow_mut().insert(package, t.clone());
        }
        Ok(text)
    };
    let (_, analysis) = gates::collect(&lib, &fetch).map_err(|e| {
        eprintln!("xtask: umbrella-api: {e}");
        1
    })?;
    for krate in &analysis.skipped {
        let why = if dir.is_some() {
            "no public-api text"
        } else {
            "not a package"
        };
        println!("skipped {krate}: {why}");
    }
    println!(
        "seeds: unexported types pulled in, of them internal or behind an internal one, item:"
    );
    for s in &analysis.seeds {
        println!("  {:4}  {:4}  {}", s.pulled, s.internal_or_behind, s.item);
    }
    if !analysis.decisions.is_empty() {
        println!("re-exports covered (re-exporter -> covered path):");
        for d in &analysis.decisions {
            println!("  {d}");
        }
    }
    if !analysis.ambiguous.is_empty() {
        println!("re-exports covering nothing (ambiguous or unresolved):");
        for d in &analysis.ambiguous {
            println!("  {d}");
        }
    }
    if !analysis.leaks.is_empty() {
        println!("internal types the exported items name (findings for their owners):");
        for l in &analysis.leaks {
            println!("  {l}");
        }
    }
    if !analysis.stale.is_empty() {
        println!("rows that match no type the texts name (stale, or for a crate not reached):");
        for r in &analysis.stale {
            println!("  {r}");
        }
    }
    for m in &analysis.missing {
        println!("{m}");
    }
    println!(
        "umbrella-api: {} unnameable type(s)",
        analysis.missing.len()
    );
    if dir.is_none() {
        print_counts(ctx, &fetch)?;
    }
    if analysis.missing.is_empty() {
        Ok(())
    } else {
        Err(1)
    }
}

/// `unused-public`: indexes the workspace with `rust-analyzer scip`, reads `tutti`'s
/// public API, and prints (and optionally writes) the report. Exits 0 with any finding.
fn unused_public(ctx: &Ctx, crates: &[String], out: Option<&str>, app: bool) -> Result<(), u8> {
    let fail = |msg: String| {
        eprintln!("xtask: unused-public: {msg}");
        1
    };
    let crates: Vec<String> = if crates.is_empty() {
        let mut all: Vec<String> = std::fs::read_dir(ctx.root.join("crates"))
            .map_err(|e| fail(e.to_string()))?
            .flatten()
            .filter(|e| e.path().join("Cargo.toml").is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|c| !app || c != "tutti")
            .collect();
        all.sort();
        all
    } else {
        crates.to_vec()
    };
    for c in &crates {
        if !ctx.root.join("crates").join(c).join("Cargo.toml").is_file() {
            return Err(fail(format!("`{c}` is not a crate under crates/")));
        }
    }
    let index = ctx.target.join("unused-public.scip");
    // Every cargo feature on, so code behind a feature (a device layer) is indexed.
    let config = ctx.target.join("unused-public-ra.json");
    std::fs::write(&config, r#"{"cargo": {"features": "all"}}"#)
        .map_err(|e| fail(format!("{}: {e}", config.display())))?;
    let ra = Cmd::sh("rust-analyzer scip .")
        .arg("--config-path")
        .arg(config.display().to_string())
        .arg("--output")
        .arg(index.display().to_string());
    eprintln!("+ {}", ra.display());
    let status = command(&ra, ctx).status().map_err(|e| {
        fail(format!(
            "cannot run rust-analyzer ({e}); install it with `rustup component add rust-analyzer`"
        ))
    })?;
    if !status.success() {
        return Err(fail(format!(
            "`{}` failed ({status}); install it with `rustup component add rust-analyzer`",
            ra.display()
        )));
    }
    let api = Cmd::sh(&recipes::public_api_cmd("tutti"));
    eprintln!("+ {}", api.display());
    let umbrella = match command(&api, ctx).output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        other => {
            eprintln!(
                "xtask: unused-public: cargo public-api did not run ({}); umbrella reachability is not checked (cargo install cargo-public-api --locked)",
                other.map_or_else(|e| e.to_string(), |o| o.status.to_string())
            );
            None
        }
    };
    let bytes = std::fs::read(&index).map_err(|e| fail(format!("{}: {e}", index.display())))?;
    let tree = gates::RealTree {
        root: ctx.root.clone(),
    };
    let make = if app {
        gates::unused_public_app
    } else {
        gates::unused_public
    };
    let git = |args: &[&str]| {
        process::Command::new("git")
            .args(args)
            .current_dir(&ctx.root)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let mut built_at = git(&["rev-parse", "--short", "HEAD"]);
    if !git(&["status", "--porcelain"]).is_empty() {
        built_at.push_str(", with uncommitted changes");
    }
    let text = make(&tree, &bytes, &crates, umbrella.as_deref(), &built_at).map_err(fail)?;
    print!("{text}");
    if let Some(path) = out {
        if let Some(dir) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(path, &text).map_err(|e| fail(format!("{path}: {e}")))?;
    }
    Ok(())
}

/// The `public-api` line count of every crate under `crates/`, as a table.
fn print_counts(
    ctx: &Ctx,
    fetch: &dyn Fn(&str) -> Result<Option<String>, String>,
) -> Result<(), u8> {
    let fail = |e: String| {
        eprintln!("xtask: public-api: {e}");
        1
    };
    let mut actual = std::collections::BTreeMap::new();
    let dirs = std::fs::read_dir(ctx.root.join("crates")).map_err(|e| fail(e.to_string()))?;
    for entry in dirs.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().join("Cargo.toml").is_file() || gates::skipped_in_counts(&name) {
            continue;
        }
        let text = fetch(&name)
            .map_err(fail)?
            .ok_or_else(|| fail(format!("{name}: no public-api text")))?;
        actual.insert(name, gates::count_api(&text));
    }
    gates::count_table(&actual)
        .iter()
        .for_each(|line| println!("{line}"));
    Ok(())
}

/// `cargo package` over every crate under `crates/`: a directory there holding
/// a `Cargo.toml` is a crate of that name.
fn package(ctx: &Ctx) -> Result<(), u8> {
    let dir = ctx.root.join("crates");
    let mut names: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().join("Cargo.toml").is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(e) => {
            eprintln!("xtask: cannot read {}: {e}", dir.display());
            return Err(1);
        }
    };
    names.sort();
    let mut c = Cmd::sh("cargo package --locked --no-verify");
    for n in &names {
        c = c.arg("-p").arg(n.as_str());
    }
    println!("+ {}", c.display());
    match command(&c, ctx).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(1),
        Err(e) => Err(spawn_failed(&c, &e)),
    }
}

/// The fuzz targets: the sorted directory names under the corpus.
fn fuzz_targets(ctx: &Ctx) -> Result<Vec<String>, u8> {
    let dir = ctx.root.join(recipes::FUZZ_CORPUS);
    let entries = std::fs::read_dir(&dir).map_err(|e| {
        eprintln!("xtask: cannot read {}: {e}", dir.display());
        1
    })?;
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(names)
}

/// Runs cargo-fuzz over `target`, or over every target in turn for `None`; every
/// run is attempted and the recipe fails if any did (a crash included).
fn fuzz(target: Option<&str>, secs: u32, ctx: &Ctx) -> Result<(), u8> {
    let all = fuzz_targets(ctx)?;
    let chosen: Vec<String> = match target {
        None => all,
        Some(t) if all.iter().any(|a| a == t) => vec![t.to_owned()],
        Some(t) => {
            eprintln!(
                "xtask: no fuzz target `{t}`; the targets are: {}",
                all.join(", ")
            );
            return Err(2);
        }
    };
    let mut failed = Vec::new();
    for t in &chosen {
        let work = ctx.root.join(recipes::FUZZ_WORK).join(t);
        if let Err(e) = std::fs::create_dir_all(&work) {
            eprintln!("xtask: cannot create {}: {e}", work.display());
            return Err(1);
        }
        let c = recipes::fuzz_cmd(t, secs);
        eprintln!("+ {}", c.display());
        if run(&c, ctx).is_err() {
            failed.push(t.as_str());
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    eprintln!(
        "xtask: fuzz failed for {}; a crash input is under testing/tutti-fuzz/fuzz/artifacts/<target>/",
        failed.join(", ")
    );
    Err(1)
}

fn command(c: &Cmd, ctx: &Ctx) -> process::Command {
    let mut p = process::Command::new(&c.program);
    p.current_dir(&ctx.root);
    for (k, v) in &c.env {
        p.env(k, v);
    }
    if c.glob {
        p.args(c.args.iter().flat_map(|a| expand(a, &ctx.root)));
    } else {
        p.args(&c.args);
    }
    p
}

fn run(c: &Cmd, ctx: &Ctx) -> Result<(), u8> {
    let status = command(c, ctx).status().map_err(|e| spawn_failed(c, &e))?;
    if status.success() {
        return Ok(());
    }
    eprintln!("xtask: `{}` failed ({status})", c.display());
    Err(status
        .code()
        .and_then(|n| u8::try_from(n).ok())
        .unwrap_or(1))
}

fn spawn_failed(c: &Cmd, e: &std::io::Error) -> u8 {
    eprintln!("xtask: cannot run `{}`: {e}", c.program);
    1
}

/// The shell's glob over `/`-separated components, each a name or one
/// `prefix*suffix`: the sorted matches, or the pattern itself when nothing
/// matches (as `sh` leaves it).
fn expand(arg: &str, root: &Path) -> Vec<String> {
    if !arg.contains('*') {
        return vec![arg.to_owned()];
    }
    let mut paths = vec![String::new()];
    for part in arg.split('/') {
        paths = paths
            .iter()
            .flat_map(|at| {
                let join = |n: &str| {
                    if at.is_empty() {
                        n.to_owned()
                    } else {
                        format!("{at}/{n}")
                    }
                };
                let Some((prefix, suffix)) = part.split_once('*') else {
                    return vec![join(part)];
                };
                std::fs::read_dir(root.join(at))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| {
                        n.len() >= prefix.len() + suffix.len()
                            && n.starts_with(prefix)
                            && n.ends_with(suffix)
                            && !(n.starts_with('.') && !prefix.starts_with('.'))
                    })
                    .map(|n| join(&n))
                    .collect()
            })
            .collect();
    }
    let mut hits: Vec<String> = paths
        .into_iter()
        .filter(|p| root.join(p).exists())
        .collect();
    hits.sort();
    if hits.is_empty() {
        vec![arg.to_owned()]
    } else {
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn no_recipe_or_help_is_help() {
        assert_eq!(parse(&argv(&[])), Ok(Action::Help));
        assert_eq!(parse(&argv(&["help"])), Ok(Action::Help));
        assert_eq!(parse(&argv(&["--help"])), Ok(Action::Help));
    }

    #[test]
    fn dry_run_is_read_before_the_recipe_only() {
        assert_eq!(
            parse(&argv(&["--dry-run", "bench", "--", "--dry-run"])),
            Ok(Action::Run {
                dry_run: true,
                recipe: "bench".into(),
                args: argv(&["--", "--dry-run"]),
            })
        );
        assert_eq!(
            parse(&argv(&["bench", "--dry-run"])),
            Ok(Action::Run {
                dry_run: false,
                recipe: "bench".into(),
                args: argv(&["--dry-run"])
            })
        );
    }

    #[test]
    fn an_unknown_option_is_refused() {
        assert!(parse(&argv(&["--nope", "ci"])).is_err());
    }

    #[test]
    fn help_lists_every_recipe() {
        let h = help();
        for (name, ..) in RECIPES {
            assert!(h.contains(&format!("  {name} ")), "{name}");
        }
    }

    #[test]
    fn a_glob_without_matches_stays_literal() {
        let dir = std::env::temp_dir().join("xtask-glob-test-none");
        assert_eq!(expand("deps/engine-*", &dir), ["deps/engine-*"]);
    }

    /// A `*` matches within one component at any depth, so a bench under its
    /// package's build directories is found whatever the hash.
    #[test]
    fn a_glob_matches_in_every_component() {
        let dir = std::env::temp_dir().join(format!("xtask-glob-test-deep-{}", std::process::id()));
        for f in [
            "build/p/a1/out/eng-1f",
            "build/p/b2/out/eng-2e",
            "build/p/b2/out/other-3d",
            "build/q/c3/out/eng-4c",
        ] {
            let path = dir.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        assert_eq!(
            expand("build/p/*/out/eng-*", &dir),
            ["build/p/a1/out/eng-1f", "build/p/b2/out/eng-2e"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
