//! `check-units-bare`: `tutti-units` allocates nothing and shares no state.
//!
//! The crate is the root leaf of the unit vocabulary: `no_std` with no `alloc`
//! and no atomics, so it builds anywhere and holds no runtime structure. The
//! `check-no-std` build already refuses `alloc` (a `no_std` crate has none
//! without `extern crate alloc`) but accepts `core::sync::atomic`, so this scan
//! reads the crate's source for both. A finding is one of these words in code
//! outside `#[cfg(test)]` items, a `tests` directory and `tests.rs` files:
//!
//! - `alloc` (the crate, `extern crate alloc` or an `alloc::` path), `Vec`,
//!   `String`, `Box` and `Arc`;
//! - `atomic` (the `core::sync::atomic` module) and any name starting with
//!   "Atomic".
//!
//! Limits: a textual scan of code (comments and literals blanked), so a type
//! that merely shares one of the names is a finding too.

use super::lex::{blank, line_of, test_ranges};
use super::{Report, Tree};

/// The crate's source.
pub(super) const DIR: &str = "crates/tutti-units/src";

/// The scan reads at least this many files, or it is broken.
pub(super) const MIN_FILES: usize = 10;

const SKIPPED_DIRS: &[&str] = &["target", ".git"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

/// Whether `path` is a test file: `tests.rs`, or under a `tests` directory.
fn is_test_file(path: &str) -> bool {
    let segs: Vec<&str> = path.split('/').collect();
    segs.last() == Some(&"tests.rs") || segs[..segs.len() - 1].contains(&"tests")
}

fn banned(ident: &str) -> bool {
    matches!(ident, "alloc" | "Vec" | "String" | "Box" | "Arc" | "atomic")
        || ident.starts_with("Atomic")
}

/// `file:line: name` of each banned name in the non-test code of `text`.
fn hits(path: &str, text: &str) -> Vec<String> {
    let code = blank(text);
    let ranges = test_ranges(&code);
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let ident = &code[start..i];
            if banned(ident) && !ranges.iter().any(|&(s, e)| start >= s && start <= e) {
                out.push(format!("{path}:{}: `{ident}`", line_of(&code, start)));
            }
        } else {
            i += 1;
        }
    }
    out
}

pub(super) fn check(tree: &dyn Tree, min_files: usize) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let mut files = 0;
    for path in tree.files(DIR, &skipped) {
        if !path.ends_with(".rs") || is_test_file(&path) {
            continue;
        }
        let Some(text) = tree.read(&path) else {
            continue;
        };
        files += 1;
        for hit in hits(&path, &text) {
            bad.push(format!(
                "{hit}: tutti-units has no allocation and no atomics; the name belongs in a crate above it"
            ));
        }
    }
    if files < min_files {
        bad.push(format!(
            "only {files} source files found under {DIR}; the scan is broken"
        ));
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!("check-units-bare: ok ({files} source files)"));
    } else {
        r.fail("check-units-bare: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    fn run(files: &[(&str, &str)]) -> Report {
        check(&Mem::with(files), 1)
    }

    const LIB: &str = "crates/tutti-units/src/lib.rs";

    #[test]
    fn clean_code_passes() {
        let r = run(&[(LIB, "pub struct Hz(pub f32);\nfn f() -> u64 { 0 }\n")]);
        assert_eq!(r.out, ["check-units-bare: ok (1 source files)"]);
        assert!(r.passed());
    }

    #[test]
    fn every_banned_name_fails_with_its_line() {
        for (src, name) in [
            ("extern crate alloc;\n", "alloc"),
            ("use alloc::borrow::ToOwned;\n", "alloc"),
            ("fn f(_: Vec<u8>) {}\n", "Vec"),
            ("fn f(_: String) {}\n", "String"),
            ("fn f(_: Box<u8>) {}\n", "Box"),
            ("fn f(_: Arc<u8>) {}\n", "Arc"),
            ("use core::sync::atomic::Ordering;\n", "atomic"),
            ("struct C(core::sync::atomic::AtomicU64);\n", "AtomicU64"),
        ] {
            let r = run(&[(LIB, &format!("\n{src}"))]);
            assert!(
                r.findings
                    .iter()
                    .any(|l| l.contains(&format!("{LIB}:2: `{name}`"))),
                "{src}: {:?}",
                r.findings
            );
        }
    }

    #[test]
    fn prose_literals_tests_and_longer_names_pass() {
        let src = "/// A `Vec` of `Arc`s in prose.\n\
                   fn f() -> &'static str { \"String Box alloc atomic\" }\n\
                   struct Vector; struct Stringly; struct Allocation; struct Atomically;\n\
                   #[cfg(test)]\nmod tests { fn t(_: Vec<u8>) {} }\n";
        // A name that merely starts with "Atomic" is the one deliberate over-match.
        let r = run(&[(LIB, src)]);
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(r.findings[1].contains("`Atomically`"));
        let src = src.replace("struct Atomically;", "");
        let r = run(&[
            (LIB, &src),
            (
                "crates/tutti-units/src/units/tests.rs",
                "fn t(_: Vec<u8>) {}\n",
            ),
            (
                "crates/tutti-units/src/units/x/tests/a.rs",
                "fn t(_: Vec<u8>) {}\n",
            ),
            ("crates/tutti-other/src/lib.rs", "fn t(_: Vec<u8>) {}\n"),
        ]);
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_scan_that_reads_too_few_files_fails() {
        let r = check(&Mem::with(&[(LIB, "")]), 2);
        assert!(r.findings[1].contains("only 1 source files found under crates/tutti-units/src"));
    }
}
