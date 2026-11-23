//! `check-session-api`: the session's public API has no `pub fn run` that borrows (a
//! session-driving call) and no `*_blocking`, and the main-thread proof is held only by the
//! types of [`ALLOW`]. A work value's consuming `run(self ..)` is the app's own call on its
//! thread, and is allowed.
//!
//! The gate reads every `crates/tutti-session/src/**/*.rs` (not a `tests` directory, a
//! `tests.rs` file or a `#[cfg(test)]` item), comments and literals blanked, and fails on:
//!
//! - a `pub fn` named `run` whose receiver is not `self` or `mut self` by value, or ending in
//!   `_blocking`, a method or a free function; a `pub(crate)`, `pub(super)` or `pub(in ..)` function is not public;
//! - a `pub struct` or `pub enum` with a `MainThreadMarker` in its body, in any spelling
//!   (`Option<MainThreadMarker>`, `PhantomData<MainThreadMarker>`, a path to it), unless
//!   [`ALLOW`] has the row `<file>::<Type>`.
//!
//! **Publicness is lexical.** Reachability from the crate root is not computed: every
//! `pub` item that is not `pub(..)` counts, wherever it sits, including one in a private
//! module that the root does not re-export. A `pub` item in a macro body is read as
//! written. Return types are not read.
//!
//! [`ALLOW`] holds `<file>::<Type>  <reason>` and `<file>::<fn>  <reason>` lines (`#`
//! lines and blank lines aside), the second for a `run` or `*_blocking` function that is
//! a row until its lane reshapes it; a row naming no such type or function, or with no
//! reason, fails.

use super::lex::{blank, find_word_from, matching, skipped_library, test_ranges, word};
use super::{Report, Tree};
use std::collections::BTreeSet;

/// The rows of types that hold the proof: `<file>::<Type>  <reason>` per line.
pub(crate) const ALLOW: &str = "xtask/session-api-allow.txt";

const ROOT: &str = "crates/tutti-session/src";
const MARKER: &str = "MainThreadMarker";

/// The fewest files a real run reads; fewer means the walk is broken.
pub(super) const MIN_FILES: usize = 10;

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

fn ident(code: &str, at: usize) -> (&str, usize) {
    let b = code.as_bytes();
    let end = at + b[at..].iter().take_while(|&&c| word(c)).count();
    (&code[at..end], end)
}

/// The position after the plain `pub` at `at` (not `pub(..)`), or `None`.
fn plain_pub(code: &str, at: usize) -> Option<usize> {
    let b = code.as_bytes();
    let i = skip_ws(b, at + 3);
    (b.get(i) != Some(&b'(')).then_some(i)
}

/// The name of the function a plain `pub` at `at` introduces, with its modifiers
/// (`const`, `async`, `unsafe`, `extern "C"`) skipped.
fn pub_fn(code: &str, at: usize) -> Option<(&str, usize)> {
    let b = code.as_bytes();
    let mut i = plain_pub(code, at)?;
    loop {
        let (w, end) = ident(code, i);
        match w {
            "fn" => {
                let n = skip_ws(b, end);
                let (name, _) = ident(code, n);
                return (!name.is_empty()).then_some((name, n));
            }
            "const" | "async" | "unsafe" | "extern" => {
                i = skip_ws(b, end);
                if b.get(i) == Some(&b'"') {
                    let close = b[i + 1..].iter().position(|&c| c == b'"')?;
                    i = skip_ws(b, i + close + 2);
                }
            }
            _ => return None,
        }
    }
}

/// Whether the function whose name starts at `name_at` takes `self` or `mut self` by value
/// as its first parameter.
fn consumes_self(code: &str, name_at: usize) -> bool {
    let b = code.as_bytes();
    let Some(open) = code[name_at..].find('(') else {
        return false;
    };
    let mut i = skip_ws(b, name_at + open + 1);
    let (mut w, mut end) = ident(code, i);
    if w == "mut" {
        i = skip_ws(b, end);
        (w, end) = ident(code, i);
    }
    w == "self" && matches!(b.get(skip_ws(b, end)), Some(b',' | b')'))
}

/// The name and body of the type a plain `pub` at `at` introduces: a `pub struct` or
/// `pub enum`, its header skipped to the first `{`, `(` or `;` outside the generics.
fn pub_type(code: &str, at: usize) -> Option<(&str, &str)> {
    let b = code.as_bytes();
    let i = plain_pub(code, at)?;
    let (kw, end) = ident(code, i);
    if kw != "struct" && kw != "enum" {
        return None;
    }
    let n = skip_ws(b, end);
    let (name, mut j) = ident(code, n);
    let mut angle = 0usize;
    while j < b.len() {
        match b[j] {
            b'-' if b.get(j + 1) == Some(&b'>') => j += 1,
            b'<' => angle += 1,
            b'>' => angle = angle.saturating_sub(1),
            b'{' | b'(' | b';' if angle == 0 => break,
            _ => {}
        }
        j += 1;
    }
    let (o, c) = match b.get(j)? {
        b'{' => (b'{', b'}'),
        b'(' => (b'(', b')'),
        _ => return Some((name, "")),
    };
    let close = matching(code, j, o, c)?;
    Some((name, &code[j..=close]))
}

struct Found {
    /// `(<file>::<fn>, <file>:<line>: pub fn <fn>)` of a banned name.
    fns: Vec<(String, String)>,
    /// `<file>::<Type>` keys of a public type holding the proof.
    types: BTreeSet<String>,
}

fn scan(tree: &dyn Tree) -> (Found, usize) {
    let mut found = Found {
        fns: Vec::new(),
        types: BTreeSet::new(),
    };
    let files: Vec<String> = tree
        .files(ROOT, &skipped_library)
        .into_iter()
        .filter(|f| f.ends_with(".rs") && !f.ends_with("/tests.rs"))
        .collect();
    for file in &files {
        let Some(src) = tree.read(file) else { continue };
        let code = blank(&src);
        let tests = test_ranges(&code);
        let in_test = |at: usize| tests.iter().any(|&(s, e)| s <= at && at < e);
        let mut from = 0;
        while let Some(at) = find_word_from(&code, "pub", from) {
            from = at + 3;
            if in_test(at) {
                continue;
            }
            if let Some((name, n)) = pub_fn(&code, at) {
                if (name == "run" && !consumes_self(&code, n)) || name.ends_with("_blocking") {
                    let line = code[..n].matches('\n').count() + 1;
                    found.fns.push((
                        format!("{file}::{name}"),
                        format!("{file}:{line}: pub fn {name}"),
                    ));
                }
            } else if let Some((name, body)) = pub_type(&code, at)
                && find_word_from(body, MARKER, 0).is_some()
            {
                found.types.insert(format!("{file}::{name}"));
            }
        }
    }
    (found, files.len())
}

fn parse(text: &str) -> Result<Vec<(String, String)>, Vec<String>> {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once(char::is_whitespace) {
            Some((key, reason)) if key.contains("::") && !reason.trim().is_empty() => {
                rows.push((key.to_owned(), reason.trim().to_owned()));
            }
            _ => errors.push(format!(
                "{ALLOW}:{}: not `<file>::<Type>  <reason>`: {line}",
                n + 1
            )),
        }
    }
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

pub(super) fn check(tree: &dyn Tree, min_files: usize) -> Report {
    let mut r = Report::default();
    let Some(text) = tree.read(ALLOW) else {
        r.fail(format!("check-session-api: FAIL: {ALLOW} is missing"));
        return r;
    };
    let rows = match parse(&text) {
        Ok(rows) => rows,
        Err(errors) => {
            r.fail(format!("check-session-api: FAIL ({ALLOW} is malformed):"));
            errors.into_iter().for_each(|e| r.fail(e));
            return r;
        }
    };
    let (found, files) = scan(tree);
    if files < min_files {
        r.fail(format!(
            "check-session-api: FAIL: {files} files read under {ROOT}, at least {min_files} expected; the walk is broken"
        ));
        return r;
    }
    let allowed: BTreeSet<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
    let loose_fns: Vec<&String> = found
        .fns
        .iter()
        .filter(|(k, _)| !allowed.contains(k.as_str()))
        .map(|(_, site)| site)
        .collect();
    if !loose_fns.is_empty() {
        r.fail(
            "check-session-api: FAIL (a public fn is a borrowing `run` or named `*_blocking`; a work \
             value's consuming `run(self ..)` is the app's door, or the fn is a row of the list):",
        );
        loose_fns.into_iter().for_each(|f| r.fail(format!("  {f}")));
    }
    let loose: Vec<&String> = found
        .types
        .iter()
        .filter(|t| !allowed.contains(t.as_str()))
        .collect();
    if !loose.is_empty() {
        r.fail(format!(
            "check-session-api: FAIL (a public type holds a `{MARKER}`; the proof is a parameter, \
             or the type is a row of {ALLOW} with its reason):"
        ));
        loose.into_iter().for_each(|t| r.fail(format!("  {t}")));
    }
    let seen: BTreeSet<&str> = found
        .fns
        .iter()
        .map(|(k, _)| k.as_str())
        .chain(found.types.iter().map(String::as_str))
        .collect();
    let stale: Vec<&&str> = allowed.iter().filter(|k| !seen.contains(**k)).collect();
    if !stale.is_empty() {
        r.fail(format!(
            "check-session-api: FAIL (a row of {ALLOW} names no such fn or type; delete it):"
        ));
        stale.into_iter().for_each(|k| r.fail(format!("  {k}")));
    }
    if r.passed() {
        r.say(format!(
            "check-session-api: OK ({files} files, {} allow rows)",
            rows.len()
        ));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const ED: &str = "crates/tutti-session/src/editor.rs";
    const OK_ROW: &str = "crates/tutti-session/src/editor.rs::Editor  holds the proof";

    fn run(files: &[(&str, &str)], allow: &str) -> Report {
        let mut all: Vec<(&str, &str)> = files.to_vec();
        all.push((ALLOW, allow));
        check(&Mem::with(&all), 0)
    }

    #[test]
    fn a_public_run_or_blocking_fn_fails_with_its_line() {
        for src in [
            "impl S {\n    pub fn run(&self) {}\n}",
            "pub fn run() {}",
            "impl S { pub fn wait_blocking(&self) {} }",
            "impl S { pub const fn x_blocking() {} }",
            "impl S { pub unsafe extern \"C\" fn run() {} }",
        ] {
            let r = run(&[("crates/tutti-session/src/a.rs", src)], "");
            assert!(!r.passed(), "{src}");
            assert!(r.findings.iter().any(|l| l.contains("a.rs:")), "{src}");
        }
        let r = run(
            &[(
                "crates/tutti-session/src/a.rs",
                "impl S {\n pub fn run(&self) {}\n}",
            )],
            "",
        );
        assert!(r.findings.iter().any(|l| l.contains("a.rs:2: pub fn run")));
    }

    #[test]
    fn a_consuming_run_passes_and_a_borrowing_or_free_run_fails() {
        let at = |src: &str| run(&[("crates/tutti-session/src/a.rs", src)], "").passed();
        for ok in [
            "impl W { pub fn run(self) {} }",
            "impl W { pub fn run(mut self) {} }",
            "impl W { pub fn run(self, main: M) -> R {} }",
            "impl W { pub fn run( self ) {} }",
        ] {
            assert!(at(ok), "{ok}");
        }
        for bad in [
            "impl W { pub fn run(&self) {} }",
            "impl W { pub fn run(&mut self) {} }",
            "pub fn run() {}",
            "impl W { pub fn run(x: Self) {} }",
            "impl W { pub fn run(x: u8, self) {} }",
        ] {
            assert!(!at(bad), "{bad}");
        }
    }

    #[test]
    fn a_restricted_run_and_look_alikes_pass() {
        let src = "pub(crate) fn run() {}\npub(super) fn x_blocking() {}\n\
                   pub(in crate::a) fn run() {}\nfn run() {}\npub fn running() {}\n\
                   pub fn blocking() {}\npub fn run_to(x: u8) {}\n// pub fn run() {}\n\
                   const S: &str = \"pub fn run() {}\";\n\
                   #[cfg(test)]\nmod t { pub fn run() {} }";
        let r = run(&[("crates/tutti-session/src/a.rs", src)], "");
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_public_type_holding_the_marker_fails_in_every_spelling() {
        for body in [
            "pub struct T { m: MainThreadMarker }",
            "pub struct T { m: Option<MainThreadMarker> }",
            "pub struct T(PhantomData<MainThreadMarker>);",
            "pub struct T { m: tutti_platform::MainThreadMarker }",
            "pub struct T { m: tutti_plugin_types::MainThreadMarker }",
            "pub enum T { A(MainThreadMarker), B }",
            "pub struct T<F: Fn(u8) -> u8> where F: Send { pub m: MainThreadMarker, f: F }",
        ] {
            let r = run(&[("crates/tutti-session/src/a.rs", body)], "");
            assert!(!r.passed(), "{body}");
            assert!(
                r.findings.iter().any(|l| l.contains("a.rs::T")),
                "{body}: {:?}",
                r.findings
            );
        }
    }

    #[test]
    fn a_restricted_or_unrelated_type_passes() {
        let src = "pub(crate) struct A { m: MainThreadMarker }\n\
                   struct B { m: MainThreadMarker }\n\
                   pub struct C { n: MainThreadMarkerLike }\n\
                   pub struct D;\npub struct E { x: u8 }\n\
                   pub fn f(main: MainThreadMarker) {}\n\
                   pub struct F<'a>(&'a u8);\n\
                   impl X { fn g(&self, m: MainThreadMarker) {} }";
        let r = run(&[("crates/tutti-session/src/a.rs", src)], "");
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn an_allowed_type_passes_and_a_stale_row_fails() {
        let held = "pub struct Editor { main: MainThreadMarker }";
        let r = run(&[(ED, held)], OK_ROW);
        assert!(r.passed(), "{:?}", r.findings);
        let r = run(&[(ED, "pub struct Editor { x: u8 }")], OK_ROW);
        assert!(!r.passed());
        assert!(r.findings.iter().any(|l| l.contains("editor.rs::Editor")));
        let r = run(&[(ED, held)], "# none\n");
        assert!(!r.passed());
    }

    #[test]
    fn an_allowed_fn_passes_and_a_stale_fn_row_fails() {
        let a = "crates/tutti-session/src/a.rs";
        let row = "crates/tutti-session/src/a.rs::run  the spawner's door";
        let r = run(&[(a, "impl T { pub fn run(&self) {} }")], row);
        assert!(r.passed(), "{:?}", r.findings);
        let r = run(&[(a, "impl T { pub(crate) fn run(&self) {} }")], row);
        assert!(!r.passed());
        let b = "crates/tutti-session/src/b.rs";
        let r = run(&[(a, "pub fn run() {}"), (b, "pub fn run() {}")], row);
        assert_eq!(r.findings.iter().filter(|l| l.contains("b.rs:")).count(), 1);
        assert!(!r.findings.iter().any(|l| l.contains("a.rs:")));
    }

    #[test]
    fn an_allow_row_is_for_one_file_and_type() {
        let r = run(
            &[(
                "crates/tutti-session/src/other.rs",
                "pub struct Editor { m: MainThreadMarker }",
            )],
            OK_ROW,
        );
        assert_eq!(r.findings.iter().filter(|l| l.starts_with("  ")).count(), 2);
    }

    #[test]
    fn a_row_without_a_reason_or_a_missing_list_fails() {
        let r = run(
            &[(ED, "pub struct Editor { m: MainThreadMarker }")],
            "x.rs::Editor\n",
        );
        assert!(r.findings.iter().any(|l| l.contains("malformed")));
        let r = check(&Mem::with(&[(ED, "")]), 0);
        assert!(r.findings.iter().any(|l| l.contains("is missing")));
    }

    #[test]
    fn a_short_walk_fails() {
        let r = check(&Mem::with(&[(ALLOW, "")]), 1);
        assert!(r.findings.iter().any(|l| l.contains("the walk is broken")));
    }
}
