//! `check-public-items`'s tests.

use super::super::fake::{Mem, run_gate};
use super::scan::scan;
use super::*;

const F: &str = "crates/a/src/lib.rs";

fn keys(path: &str, src: &str) -> Vec<String> {
    scan(path, src).into_iter().map(|i| i.key).collect()
}

fn list(rows: &[&str]) -> String {
    format!("# header\n{}\n", rows.join("\n"))
}

#[test]
fn every_kind_of_public_item_is_listed_by_kind_and_path() {
    let src = "pub fn f() {}\npub const fn g() {}\npub unsafe extern \"C\" fn h() {}\n\
               pub struct S;\npub enum E { A }\npub trait T {}\npub unsafe trait U {}\n\
               pub type Al = u8;\npub const C: u8 = 1;\npub static ST: u8 = 1;\n\
               pub static mut M: u8 = 1;\npub mod m {}\npub union Un { a: u8 }\n\
               #[macro_export]\nmacro_rules! mac { () => {} }\n";
    let mut got = keys(F, src);
    got.sort();
    let mut want = [
        "fn f",
        "fn g",
        "fn h",
        "struct S",
        "enum E",
        "trait T",
        "trait U",
        "type Al",
        "const C",
        "static ST",
        "static M",
        "mod m",
        "union Un",
        "macro mac",
    ]
    .map(str::to_owned)
    .to_vec();
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn a_field_a_variant_a_pub_use_and_a_restricted_item_are_not_listed() {
    let src = "pub struct S { pub x: u8, pub(crate) y: u8 }\npub struct T(pub u8, pub fn(u8));\n\
               pub use a::b;\npub(crate) fn c() {}\npub(super) struct D;\npub(in crate::x) fn e() {}\n\
               pub enum E { A, B(u8) }\nmacro_rules! private { () => {} }\n";
    assert_eq!(keys(F, src), ["struct S", "struct T", "enum E"]);
}

#[test]
fn an_item_behind_cfg_test_and_a_comment_or_literal_is_not_listed() {
    let src = "#[cfg(test)]\nmod tests { pub fn t() {} }\n#[cfg(test)]\npub fn u() {}\n\
               // pub fn c() {}\nconst S: &str = \"pub fn s() {}\";\npub fn real() {}\n";
    assert_eq!(keys(F, src), ["fn real"]);
}

#[test]
fn a_method_is_keyed_by_its_impl_type_without_generics_and_a_trait_impl_has_none() {
    let src = "pub struct Editor<T>(T);\nimpl<T: Clone> Editor<T> {\n    pub fn replace(&self) {}\n    \
               pub const N: u8 = 1;\n    fn private(&self) {}\n}\nimpl Clone for Editor<u8> { fn clone(&self) -> Self { todo!() } }\n";
    assert_eq!(
        keys("crates/a/src/edit.rs", src),
        [
            "struct edit::Editor",
            "fn edit::Editor::replace",
            "const edit::Editor::N"
        ]
    );
}

#[test]
fn the_module_path_is_the_file_and_the_inline_mods_around_the_item() {
    assert_eq!(keys("crates/a/src/lib.rs", "pub fn f() {}"), ["fn f"]);
    assert_eq!(keys("crates/a/src/x/mod.rs", "pub fn f() {}"), ["fn x::f"]);
    assert_eq!(
        keys("crates/a/src/x/y.rs", "pub struct S;"),
        ["struct x::y::S"]
    );
    assert_eq!(
        keys(F, "mod inner { pub mod deep { pub fn f() {} } }"),
        ["mod inner::deep", "fn inner::deep::f"]
    );
}

fn run(files: &[(&str, &str)]) -> Report {
    run_gate(|t| check(t, 0), files)
}

const SRC: &str = "pub struct Editor;\nimpl Editor { pub fn replace(&self) {} }\n";

#[test]
fn a_tree_with_every_row_passes() {
    let l = list(&["fn Editor::replace", "struct Editor"]);
    let r = run(&[(F, SRC), ("xtask/public-items/a.txt", &l)]);
    assert_eq!(r.findings, Vec::<String>::new());
}

#[test]
fn an_item_with_no_row_fails_naming_the_crate_the_key_and_the_place() {
    let l = list(&["struct Editor"]);
    let r = run(&[(F, SRC), ("xtask/public-items/a.txt", &l)]);
    assert_eq!(
        r.findings,
        [
            "check-public-items: FAIL: a: fn Editor::replace (crates/a/src/lib.rs:2): a new public item \
             needs its row in xtask/public-items/a.txt, added in the same change, or make it pub(crate); \
             say in the PR description what calls it from outside the crate (CONTRIBUTING.md, reviewer checklist, item 4)"
        ]
    );
}

#[test]
fn a_crate_with_no_list_file_has_no_rows() {
    let r = run(&[(F, "pub fn f() {}\n")]);
    assert_eq!(r.findings.len(), 1);
    assert!(r.findings[0].contains("a: fn f (crates/a/src/lib.rs:1)"));
}

#[test]
fn a_row_with_no_item_fails_and_says_to_delete_it() {
    let l = list(&["fn gone", "struct Editor", "fn Editor::replace"]);
    let r = run(&[(F, SRC), ("xtask/public-items/a.txt", &l)]);
    assert_eq!(
        r.findings,
        [
            "check-public-items: FAIL: a: fn gone (xtask/public-items/a.txt:2) has a row and no item: \
          delete the row: the list only shrinks on its own"
        ]
    );
}

#[test]
fn a_key_that_occurs_twice_needs_two_rows() {
    let twin = "#[cfg(unix)]\npub fn f() {}\n#[cfg(windows)]\npub fn f() {}\n";
    let one = list(&["fn f"]);
    let r = run(&[(F, twin), ("xtask/public-items/a.txt", &one)]);
    assert_eq!(r.findings.len(), 1);
    assert!(
        r.findings[0].contains("fn f (crates/a/src/lib.rs:4)"),
        "{:?}",
        r.findings
    );
    let two = list(&["fn f", "fn f"]);
    assert!(run(&[(F, twin), ("xtask/public-items/a.txt", &two)]).passed());
    let r = run(&[(F, "pub fn f() {}\n"), ("xtask/public-items/a.txt", &two)]);
    assert!(
        r.findings[0].contains("a.txt:3) has a row and no item"),
        "{:?}",
        r.findings
    );
}

#[test]
fn tests_directories_test_modules_and_binaries_are_not_read() {
    let lib = "pub fn f() {}\n#[cfg(test)]\nmod checks;\n#[cfg(test)]\nmod more_tests;\nmod ok;\n";
    let l = list(&["fn f", "fn ok::g"]);
    let r = run(&[
        (F, lib),
        ("crates/a/src/checks.rs", "pub fn t() {}"),
        ("crates/a/src/more_tests.rs", "pub fn t() {}"),
        ("crates/a/src/more_tests/sub.rs", "pub fn t() {}"),
        ("crates/a/src/ok.rs", "pub fn g() {}"),
        ("crates/a/src/tests.rs", "pub fn t() {}"),
        ("crates/a/src/main.rs", "pub fn m() {}"),
        ("crates/a/src/bin/x.rs", "pub fn m() {}"),
        ("crates/a/tests/it/main.rs", "pub fn t() {}"),
        ("crates/a/benches/b.rs", "pub fn t() {}"),
        ("crates/a/examples/e.rs", "pub fn t() {}"),
        ("testing/b/src/lib.rs", "pub fn t() {}"),
        ("xtask/public-items/a.txt", &l),
    ]);
    assert_eq!(r.findings, Vec::<String>::new());
}

#[test]
fn a_list_line_that_is_no_key_and_a_walk_that_finds_too_little_fail() {
    let r = run(&[
        (F, "pub fn f() {}\n"),
        ("xtask/public-items/a.txt", "nonsense\n"),
    ]);
    assert!(
        r.findings.iter().any(|f| f.contains("malformed")),
        "{:?}",
        r.findings
    );
    let r = run_gate(|t| check(t, 5), &[(F, "pub fn f() {}\n")]);
    assert!(r.findings[0].contains("the walk is broken"));
}

#[test]
fn the_lists_written_pass_the_gate_and_are_sorted_under_a_header() {
    let tree = Mem::with(&[(F, "pub fn b() {}\npub fn a() {}\n")]);
    let written = lists(&tree);
    let text = &written["xtask/public-items/a.txt"];
    assert!(text.starts_with("# "));
    assert!(text.ends_with("fn a\nfn b\n"), "{text}");
    let files: Vec<(&str, &str)> = vec![
        (F, "pub fn b() {}\npub fn a() {}\n"),
        ("xtask/public-items/a.txt", text),
    ];
    assert!(run(&files).passed());
}
