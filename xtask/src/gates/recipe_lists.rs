//! `check-layout`'s rows for the recipes' hand lists: each row names something
//! the workspace has, and no list leaves out what it must hold.
//!
//! - **Every package a recipe command names** (`-p`, `--package`, `--exclude`) is a
//!   workspace member: the `-p` lists of `check-features`, `test-features`, `rtsan`,
//!   `loom`, `check-loom`, `check-wasm`, `bench` and the `--exclude`s of `check-windows`.
//! - **A `--test` named under `-p` is a test target of that package** (`tests/<name>.rs`
//!   or `tests/<name>/main.rs`), and **every `*_loom` test target of a member is a
//!   `--test` some command runs**: a loom model that no recipe runs never runs.
//! - **A `--bench` named under `-p` is a bench target of that package.** The list of
//!   benches is a choice, not a sweep, so an unlisted bench is not a finding.
//! - **The `no_std` list is the set of crates whose `lib.rs` is `no_std`**, and every
//!   row names a member.
//! - **Every book sound names an example of `tutti-book`**, and every example of it is
//!   a sound.

use super::layout::{Pkg, packages};
use super::lex::blank;
use super::{Report, Tree};
use std::collections::BTreeSet;

/// The recipes' hand lists, as `recipes::listed` reads them out.
pub(crate) struct Listed {
    /// The crates `check-no-std` builds without `std`.
    pub(crate) no_std: Vec<&'static str>,
    /// Every command line (or argument line) the lists hold.
    pub(crate) commands: Vec<&'static str>,
    /// The `tutti-book` examples `book` renders.
    pub(crate) book_sounds: &'static [&'static str],
}

const BOOK: &str = "testing/tutti-book";

/// The packages a command line names, and each `--test` and `--bench` target with
/// the packages named before it.
struct Words {
    packages: Vec<String>,
    tests: Vec<String>,
    benches: Vec<String>,
}

fn words(command: &str) -> Words {
    let mut w = Words {
        packages: Vec::new(),
        tests: Vec::new(),
        benches: Vec::new(),
    };
    let mut it = command.split_whitespace();
    while let Some(t) = it.next() {
        let into = match t {
            "-p" | "--package" | "--exclude" => &mut w.packages,
            "--test" => &mut w.tests,
            "--bench" => &mut w.benches,
            _ => continue,
        };
        if let Some(v) = it.next() {
            into.push(v.to_owned());
        }
    }
    w
}

fn has_target(tree: &dyn Tree, p: &Pkg, kind: &str, name: &str) -> bool {
    let (file, main) = (
        format!("{}/{kind}/{name}.rs", p.dir),
        format!("{}/{kind}/{name}/main.rs", p.dir),
    );
    tree.read(&file).is_some() || tree.read(&main).is_some()
}

pub(super) fn check(tree: &dyn Tree, lists: &Listed) -> Report {
    let mut bad = Vec::new();
    let pkgs = packages(tree, &mut Vec::new());
    let member = |name: &str| pkgs.iter().find(|p| p.name == name);
    let mut run_tests: BTreeSet<(String, String)> = BTreeSet::new();
    for command in &lists.commands {
        let w = words(command);
        for name in &w.packages {
            if member(name).is_none() {
                bad.push(format!(
                    "a recipe command names `{name}`, which is not a workspace member: {command}"
                ));
            }
        }
        for (kind, targets) in [("tests", &w.tests), ("benches", &w.benches)] {
            for t in targets {
                let owners: Vec<&Pkg> = w.packages.iter().filter_map(|n| member(n)).collect();
                if !owners.iter().any(|p| has_target(tree, p, kind, t)) {
                    bad.push(format!(
                        "a recipe command names {kind} target `{t}`, which none of its packages has: {command}"
                    ));
                }
                if kind == "tests" {
                    run_tests.extend(owners.iter().map(|p| (p.name.clone(), t.clone())));
                }
            }
        }
    }
    for p in &pkgs {
        for path in tree.files(&format!("{}/tests", p.dir), &|_| false) {
            let rel = &path[p.dir.len() + "/tests/".len()..];
            let stem = rel.strip_suffix(".rs").and_then(|s| {
                let s = s.strip_suffix("/main").unwrap_or(s);
                (!s.contains('/')).then_some(s)
            });
            if let Some(stem) = stem.filter(|s| s.ends_with("_loom"))
                && !run_tests.contains(&(p.name.clone(), stem.to_owned()))
            {
                bad.push(format!(
                    "{path} is a loom model no recipe runs; add `--test {stem}` under `-p {}` to `loom`",
                    p.name
                ));
            }
        }
    }

    let no_std: BTreeSet<&str> = pkgs
        .iter()
        .filter(|p| {
            tree.read(&format!("{}/src/lib.rs", p.dir))
                .is_some_and(|t| blank(&t).contains("no_std"))
        })
        .map(|p| p.name.as_str())
        .collect();
    let listed: BTreeSet<&str> = lists.no_std.iter().copied().collect();
    for name in listed.difference(&no_std) {
        bad.push(format!(
            "NO_STD_CRATES lists `{name}`, whose lib.rs is not no_std (or which is not a member); remove the row"
        ));
    }
    for name in no_std.difference(&listed) {
        bad.push(format!(
            "`{name}` is no_std but is not a row of NO_STD_CRATES; `check-no-std` would never build it for {}",
            "a target with no std"
        ));
    }

    let examples: BTreeSet<String> = tree
        .files(&format!("{BOOK}/examples"), &|_| false)
        .into_iter()
        .filter_map(|p| {
            p.strip_prefix(&format!("{BOOK}/examples/"))?
                .strip_suffix(".rs")
                .map(str::to_owned)
        })
        .collect();
    for s in lists.book_sounds {
        if !examples.contains(*s) {
            bad.push(format!(
                "BOOK_SOUNDS lists `{s}`, which is no example of {BOOK}; remove the row"
            ));
        }
    }
    for e in &examples {
        if !lists.book_sounds.contains(&e.as_str()) {
            bad.push(format!(
                "{BOOK}/examples/{e}.rs is no row of BOOK_SOUNDS; `book` would never render it"
            ));
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!(
            "check-layout: recipe lists ok ({} commands, {} no_std crates, {} book sounds)",
            lists.commands.len(),
            lists.no_std.len(),
            lists.book_sounds.len()
        ));
    } else {
        r.fail("check-layout: FAIL (recipe lists)");
        bad.into_iter().for_each(|b| r.fail(format!("  {b}")));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    fn lists(commands: &[&'static str]) -> Listed {
        Listed {
            no_std: vec!["a"],
            commands: commands.to_vec(),
            book_sounds: &["one"],
        }
    }

    /// Members `a` (no_std, a loom model, a bench) and `b`, and the book's one example.
    fn tree() -> Mem {
        Mem::with(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n",
            ),
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
            ("crates/b/Cargo.toml", "[package]\nname = \"b\"\n"),
            ("crates/a/src/lib.rs", "#![cfg_attr(not(test), no_std)]\n"),
            ("crates/b/src/lib.rs", "// no_std in a comment only\n"),
            ("crates/a/tests/ring_loom.rs", ""),
            ("crates/b/benches/speed/main.rs", ""),
            ("testing/tutti-book/examples/one.rs", ""),
        ])
    }

    const OK: &[&str] = &[
        "cargo test -p a --test ring_loom",
        "cargo bench -p b --bench speed",
        "cargo clippy --workspace --exclude b",
    ];

    fn findings(tree: &Mem, commands: &[&'static str]) -> Vec<String> {
        check(tree, &lists(commands)).findings
    }

    #[test]
    fn lists_that_name_real_members_targets_and_examples_pass() {
        let f = findings(&tree(), OK);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn a_package_a_command_names_must_be_a_member() {
        let f = findings(
            &tree(),
            &["cargo build -p gone", "cargo clippy --exclude gone2"],
        );
        assert!(
            f[1].contains("`gone`, which is not a workspace member"),
            "{f:?}"
        );
        assert!(f[2].contains("`gone2`"), "{f:?}");
    }

    #[test]
    fn a_test_or_bench_target_must_belong_to_a_named_package() {
        let f = findings(&tree(), &["cargo test -p a --test ring_loom --test nope"]);
        assert!(f.iter().any(|l| l.contains("tests target `nope`")), "{f:?}");
        let f = findings(&tree(), &["cargo bench -p a --bench speed"]);
        assert!(
            f.iter().any(|l| l.contains("benches target `speed`")),
            "{f:?}"
        );
    }

    #[test]
    fn a_loom_model_no_command_runs_is_a_finding_and_an_unlisted_bench_is_not() {
        let f = findings(&tree(), &[]);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[1].contains("crates/a/tests/ring_loom.rs is a loom model no recipe runs"));
        let mut t = tree();
        t.files
            .insert("crates/b/benches/other.rs".into(), String::new());
        assert!(findings(&t, OK).is_empty());
    }

    #[test]
    fn the_no_std_list_is_the_no_std_crates() {
        let mut t = tree();
        t.files
            .insert("crates/b/src/lib.rs".into(), "#![no_std]\n".into());
        let f = findings(&t, OK);
        assert!(f[1].contains("`b` is no_std but is not a row"), "{f:?}");
        let mut l = lists(OK);
        l.no_std = vec!["a", "gone"];
        let f = check(&tree(), &l).findings;
        assert!(f[1].contains("NO_STD_CRATES lists `gone`"), "{f:?}");
    }

    #[test]
    fn the_book_sounds_are_the_book_examples() {
        let mut t = tree();
        t.files
            .insert("testing/tutti-book/examples/two.rs".into(), String::new());
        let f = findings(&t, OK);
        assert!(
            f[1].contains("examples/two.rs is no row of BOOK_SOUNDS"),
            "{f:?}"
        );
        let mut l = lists(OK);
        l.book_sounds = &["one", "three"];
        let f = check(&tree(), &l).findings;
        assert!(f[1].contains("BOOK_SOUNDS lists `three`"), "{f:?}");
    }
}
