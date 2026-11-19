//! `check-canonical-paths`: engine types are imported from a crate's root,
//! never through its modules.
//!
//! Neither the compiler nor clippy has a lint for a redundant path, and doc
//! comments in ```text / ```ignore blocks are never compiled, so this scan is
//! the only enforcement. It searches `*.md` as well as `*.rs` for that reason.
//!
//! It matches tutti-to-tutti chains only; a third-party
//! re-export and is left alone.
//!
//! Usage (`cargo xtask check-paths [<crate> <module>...]`): with no arguments
//! the chained-path gate; with a crate and at least one module, also the
//! per-module gate (`tutti_rt cell vec` looks for `tutti_rt::cell::` and
//! `tutti_rt::vec::`). A lone argument is ignored, as the script ignored it.
//!
//! The patterns were `grep -E` ones; each is written out below to the same
//! language:
//! - chained: `tutti_[a-z0-9_]+::tutti_[a-z0-9_]+::`, anywhere in a line;
//! - module: `<crate>::(<mod>|<mod>...)::`, the crate and modules literal.

use super::{Report, Tree, under_skipped_dir};

// `_archive/` is excluded by design: those crates are kept as SPEC for
// rebuilds and reference APIs that no longer exist. They are hidden from cargo
// (Cargo.toml -> .txt) and must not gate a build.
const EXCLUDED_DIRS: &[&str] = &["_archive", "target", ".git"];

fn excluded(dir: &str) -> bool {
    EXCLUDED_DIRS.contains(&dir)
}

pub(super) fn check(tree: &dyn Tree, args: &[String]) -> Report {
    let mut r = Report::default();

    // A moved root must fail here: a scan over a missing directory finds
    // nothing and the gate would pass having read nothing. (`find`, so no
    // directory is excluded.)
    for root in ["crates", "testing"] {
        if !tree
            .files(root, &|_| false)
            .iter()
            .any(|f| f.ends_with(".rs"))
        {
            r.fail(format!(
                "FAIL: {root}/ holds no .rs file; was the layout changed?"
            ));
            return r;
        }
    }

    let scanned: Vec<String> = ["crates", "testing"]
        .into_iter()
        .flat_map(|root| tree.files(root, &excluded))
        .filter(|f| f.ends_with(".rs") || f.ends_with(".md"))
        .filter(|f| !under_skipped_dir(f, &excluded))
        .collect();

    r.say("==> chained tutti_X::tutti_Y:: paths");
    // A tutti crate reached THROUGH another tutti crate. The trailing `::` is
    // required so a bare `pub use tutti_rt;` (the declaration being
    // removed, which lives in exactly one place per crate) is not itself
    // reported here.
    let hits = grep(tree, &scanned, chained);
    if hits.is_empty() {
        r.say("ok");
    } else {
        let n = hits.len();
        for h in hits {
            r.fail(h);
        }
        r.fail(format!(
            "FAIL: {n} chained path(s). Import from the owning crate's root instead."
        ));
    }

    // Per-module gate.
    if let [krate, mods @ ..] = args
        && !mods.is_empty()
    {
        r.say(format!("==> redundant {krate}::<module>:: paths"));
        let needles: Vec<String> = mods.iter().map(|m| format!("{krate}::{m}::")).collect();
        let hits = grep(tree, &scanned, |l| needles.iter().any(|n| l.contains(n)));
        if hits.is_empty() {
            r.say("ok");
        } else {
            let n = hits.len();
            for h in hits {
                r.fail(h);
            }
            r.fail(format!(
                "FAIL: {n} path(s) through a privatized module. Use {krate}::<Item>."
            ));
        }
    }
    r
}

/// `grep -n`'s lines, as `path:line:text`.
fn grep(tree: &dyn Tree, files: &[String], hit: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    for f in files {
        let Some(text) = tree.read(f) else { continue };
        for (i, l) in text.split('\n').enumerate() {
            if hit(l) {
                out.push(format!("{f}:{}:{l}", i + 1));
            }
        }
    }
    out
}

fn class(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
}

/// The end of the `[a-z0-9_]` run starting at `at`.
fn run_end(b: &[u8], at: usize) -> usize {
    at + b[at..].iter().take_while(|&&c| class(c)).count()
}

/// `tutti_[a-z0-9_]+::` at `at`; the index after the `::`.
fn crate_then_colons(b: &[u8], at: usize) -> Option<usize> {
    if !b[at..].starts_with(b"tutti_") {
        return None;
    }
    let k = run_end(b, at + 6);
    (k > at + 6 && b[k..].starts_with(b"::")).then_some(k + 2)
}

/// `tutti_[a-z0-9_]+::tutti_[a-z0-9_]+::`. The class has no `:`, so each run
/// ends where the `::` must start and no backtracking is ever needed.
fn chained(line: &str) -> bool {
    let b = line.as_bytes();
    line.match_indices("tutti_")
        .any(|(i, _)| crate_then_colons(b, i).is_some_and(|m| crate_then_colons(b, m).is_some()))
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    /// The smallest tree that passes: each root holds one `.rs` file.
    fn base() -> Mem {
        Mem::with(&[
            ("crates/a/src/lib.rs", "pub fn a() {}\n"),
            ("testing/b/src/lib.rs", "pub fn b() {}\n"),
        ])
    }

    fn run(m: &Mem, args: &[&str]) -> Report {
        let a: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        check(m, &a)
    }

    #[test]
    fn a_clean_tree_passes() {
        let r = run(&base(), &[]);
        assert_eq!(r.findings, Vec::<String>::new());
        assert_eq!(r.out, ["==> chained tutti_X::tutti_Y:: paths", "ok"]);
    }

    #[test]
    fn a_chained_path_is_reported_with_its_place_and_a_count() {
        let mut m = base();
        m.files.insert(
            "crates/a/src/x.rs".into(),
            "use ok::Thing;\nuse tutti_a::tutti_b::Thing;\n".into(),
        );
        m.files.insert(
            "testing/b/README.md".into(),
            "x tutti_q::tutti_r::Y\n".into(),
        );
        let r = run(&m, &[]);
        assert_eq!(
            r.findings,
            [
                "crates/a/src/x.rs:2:use tutti_a::tutti_b::Thing;",
                "testing/b/README.md:1:x tutti_q::tutti_r::Y",
                "FAIL: 2 chained path(s). Import from the owning crate's root instead.",
            ]
        );
    }

    #[test]
    fn the_chained_pattern_is_the_greps() {
        for hit in [
            "tutti_a::tutti_b::",
            "xtutti_a1::tutti_b_2::C",
            "tutti_tutti_a::tutti_b::",
        ] {
            assert!(chained(hit), "{hit}");
        }
        for miss in [
            "tutti_a::tutti_b",
            "tutti_::tutti_b::",
            "tutti_a::tutti_::",
            "tutti_A::tutti_b::",
            "tutti_a:: tutti_b::",
            "tutti::tutti_b::",
            "pub use tutti_rt;",
        ] {
            assert!(!chained(miss), "{miss}");
        }
    }

    #[test]
    fn only_rs_and_md_are_scanned_and_three_directories_are_excluded() {
        let mut m = base();
        let bad = "tutti_a::tutti_b::X\n";
        for p in [
            "crates/a/notes.txt",
            "crates/_archive/old.rs",
            "crates/a/target/gen.rs",
            "crates/a/.git/hook.md",
            "crates/a/src/_archive/deep.md",
        ] {
            m.files.insert(p.into(), bad.into());
        }
        let r = run(&m, &[]);
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_root_without_rs_files_fails_at_once() {
        let mut m = base();
        m.files.remove("testing/b/src/lib.rs");
        m.files.insert("testing/b/notes.md".into(), "x".into());
        let r = run(&m, &[]);
        assert_eq!(
            r.out,
            ["FAIL: testing/ holds no .rs file; was the layout changed?"]
        );
        assert_eq!(r.findings, r.out);
        let r = run(&Mem::default(), &[]);
        assert_eq!(
            r.findings,
            ["FAIL: crates/ holds no .rs file; was the layout changed?"]
        );
    }

    #[test]
    fn the_root_check_counts_excluded_directories_as_find_does() {
        let mut m = Mem::with(&[("crates/target/x.rs", ""), ("testing/_archive/y.rs", "")]);
        m.files.insert("crates/a/lib.md".into(), "ok".into());
        assert!(run(&m, &[]).passed());
    }

    #[test]
    fn the_per_module_gate_takes_a_crate_and_its_modules() {
        let mut m = base();
        m.files.insert(
            "crates/a/src/x.rs".into(),
            "use tutti_rt::cell::Cell;\nuse tutti_rt::vec::RtVec;\nuse tutti_rt::RtVec;\nuse tutti_dsp::cell::C;\n"
                .into(),
        );
        let r = run(&m, &["tutti_rt", "cell", "vec"]);
        assert_eq!(
            r.out[2..],
            [
                "==> redundant tutti_rt::<module>:: paths",
                "crates/a/src/x.rs:1:use tutti_rt::cell::Cell;",
                "crates/a/src/x.rs:2:use tutti_rt::vec::RtVec;",
                "FAIL: 2 path(s) through a privatized module. Use tutti_rt::<Item>.",
            ]
        );
        assert_eq!(r.findings, &r.out[3..]);
        let r = run(&m, &["tutti_rt", "spin"]);
        assert_eq!(
            r.out[2..],
            ["==> redundant tutti_rt::<module>:: paths", "ok"]
        );
    }

    #[test]
    fn a_lone_argument_is_ignored() {
        let mut m = base();
        m.files
            .insert("crates/a/src/x.rs".into(), "tutti_rt::cell::C\n".into());
        let r = run(&m, &["tutti_rt"]);
        assert!(r.passed());
        assert_eq!(r.out.len(), 2);
    }
}
