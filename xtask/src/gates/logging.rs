//! `check-logging`: logging is `tracing`, and nothing prints outside a `main`.
//!
//! - **No `log` dependency** in any manifest under `crates/` or `testing/`, in any
//!   table (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, their
//!   `target` forms and a `[dependencies.log]` table; a `package = "log"` rename
//!   counts).
//! - **No `println!`, `eprintln!`, `print!`, `eprint!` or `dbg!`** in library code.
//!   Exempt: `src/main.rs`, `src/bin/`, `build.rs`, a `tests`, `examples` or `benches`
//!   directory, a `tests.rs` file and test-only items (`#[cfg(test)]`, `#[cfg(all(..,
//!   test, ..))]`). Only `src/main.rs` and `src/bin/**` count as a `main`: the other
//!   modules of a binary crate are library code. Comments and string literals are not
//!   code.
//! - **No `stderr()` or `stdout()`** in the same library code: a write to a standard
//!   stream through a handle is a print.
//!
//! A finding that is a row of [`ALLOW`], `(path, reason)`, is exempt: the path is the
//! manifest (for a `log` dependency) or the source file (for a print). A row that
//! matches nothing is stale. The reason starts with `permanent:` (the file is a
//! program or a test harness whose output is its job) or `temporary:` (a lane
//! converts it, and the row leaves with it).

use std::collections::BTreeSet;

use super::lex::{blank, find_word_from, line_of, test_ranges};
use super::toml::Toml;
use super::{Report, Tree};

const SKIPPED_DIRS: &[&str] = &["target", ".git"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

/// The fewest manifests a real run finds; fewer means the walk is broken.
pub(super) const MIN_MANIFESTS: usize = 20;

const MACROS: &[&str] = &["println", "eprintln", "print", "eprint", "dbg"];

/// The functions that hand out a standard stream.
const STREAMS: &[&str] = &["stderr", "stdout"];

/// `(manifest or source path, reason)`. The reason starts with `permanent:` or
/// `temporary:`.
pub(super) const ALLOW: &[(&str, &str)] = &[
    (
        "testing/tutti-fixtures/src/main_thread.rs",
        "permanent: the `harness = false` test runner prints each test's name and result as a test harness does",
    ),
    (
        "testing/tutti-plugin-contract/src/format.rs",
        "permanent: the contract prints its gap marker, which the dashboard and the ratchet read from the test's stdout",
    ),
    (
        "testing/tutti-shared-plugin/src/clap.rs",
        "permanent: the test plugin writes to stdout and stderr on init so the host tests can check that a plugin's own output is captured",
    ),
];

/// Whether `path` (repo-relative) is a program, a test, an example or a bench.
fn exempt(path: &str) -> bool {
    let segs: Vec<&str> = path.split('/').collect();
    let file = segs.last().copied().unwrap_or("");
    if file == "build.rs" || file == "tests.rs" {
        return true;
    }
    if segs[2..segs.len() - 1]
        .iter()
        .any(|c| matches!(*c, "tests" | "examples" | "benches"))
    {
        return true;
    }
    match segs.get(2).copied() {
        Some("src") => {
            let rest = &segs[3..];
            rest == ["main.rs"] || rest.first() == Some(&"bin")
        }
        _ => false,
    }
}

/// `file:line: macro!` of each print in library code.
fn prints(path: &str, text: &str) -> Vec<String> {
    let code = blank(text);
    let ranges = test_ranges(&code);
    let mut out = Vec::new();
    for name in MACROS {
        let mut from = 0;
        while let Some(at) = find_word_from(&code, name, from) {
            from = at + name.len();
            if !code[from..].trim_start().starts_with('!') {
                continue;
            }
            if ranges.iter().any(|&(s, e)| at >= s && at <= e) {
                continue;
            }
            out.push(format!("{path}:{}: `{name}!`", line_of(&code, at)));
        }
    }
    for name in STREAMS {
        let mut from = 0;
        while let Some(at) = find_word_from(&code, name, from) {
            from = at + name.len();
            let before = code[..at].trim_end();
            if !code[from..].trim_start().starts_with('(')
                || before.ends_with('.')
                || before.ends_with("fn")
            {
                continue;
            }
            if ranges.iter().any(|&(s, e)| at >= s && at <= e) {
                continue;
            }
            out.push(format!("{path}:{}: `{name}()`", line_of(&code, at)));
        }
    }
    out.sort();
    out
}

pub(super) fn check(tree: &dyn Tree, allow: &[(&str, &str)]) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut manifests = 0;
    let mut sources = 0;
    let mut findings: Vec<(String, String)> = Vec::new();

    for dir in ["crates", "testing"] {
        for path in tree.files(dir, &skipped) {
            let segs: Vec<&str> = path.split('/').collect();
            if segs.len() < 3 {
                continue;
            }
            let Some(text) = tree.read(&path) else {
                continue;
            };
            if segs.len() == 3 && segs[2] == "Cargo.toml" {
                manifests += 1;
                for (name, kind) in Toml::parse(&text).dependencies() {
                    if name == "log" {
                        findings.push((
                            path.clone(),
                            format!(
                                "{path}: a {} dependency on `log`; use `tracing`",
                                kind.label()
                            ),
                        ));
                    }
                }
            } else if path.ends_with(".rs") && !exempt(&path) {
                sources += 1;
                for hit in prints(&path, &text) {
                    findings.push((
                        path.clone(),
                        format!(
                            "{hit} in library code; log through `tracing` (a `main` may print)"
                        ),
                    ));
                }
            }
        }
    }

    if manifests < MIN_MANIFESTS {
        bad.push(format!(
            "only {manifests} manifests found under crates/ and testing/; the scan is broken"
        ));
    }
    for (path, reason) in allow {
        if !(reason.starts_with("permanent:") || reason.starts_with("temporary:")) {
            bad.push(format!(
                "ALLOW {path}: the reason must start with `permanent:` or `temporary:`"
            ));
        }
    }
    for (path, message) in findings {
        if allow.iter().any(|(p, _)| *p == path) {
            found.insert(path);
        } else {
            bad.push(message);
        }
    }
    for (path, _) in allow {
        if !found.contains(*path) {
            bad.push(format!(
                "ALLOW {path}: stale; it holds no `log` dependency or print any more, remove the row"
            ));
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!(
            "check-logging: ok ({manifests} manifests, {sources} source files, {} allowed)",
            found.len()
        ));
    } else {
        r.fail("check-logging: FAIL");
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

    const MANIFEST: &str = "[package]\nname = \"x\"\n";

    /// Enough manifests for the scan to believe it found the repo, plus `extra`.
    fn tree(extra: &[(&str, &str)]) -> Mem {
        let mut files: Vec<(String, String)> = (0..20)
            .map(|i| (format!("crates/c{i}/Cargo.toml"), MANIFEST.to_owned()))
            .collect();
        for (p, t) in extra {
            files.retain(|(q, _)| q != p);
            files.push(((*p).to_owned(), (*t).to_owned()));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        Mem::with(&refs)
    }

    fn findings(extra: &[(&str, &str)], allow: &[(&str, &str)]) -> Vec<String> {
        check(&tree(extra), allow).findings
    }

    const LIB: &str = "crates/c1/src/lib.rs";

    #[test]
    fn a_log_dependency_fails_in_every_table() {
        for table in [
            "[dependencies]\nlog = \"0.4\"\n",
            "[dev-dependencies]\nlog = { version = \"0.4\" }\n",
            "[build-dependencies]\nlog.workspace = true\n",
            "[target.'cfg(unix)'.dependencies]\nlog = \"0.4\"\n",
            "[dependencies.log]\nversion = \"0.4\"\n",
            "[dependencies]\nlogging = { package = \"log\", version = \"0.4\" }\n",
        ] {
            let f = findings(&[("testing/t/Cargo.toml", table)], &[]);
            assert_eq!(f.len(), 2, "{table}: {f:?}");
            assert!(f[1].contains("testing/t/Cargo.toml"), "{f:?}");
        }
        let ok = "[dependencies]\ntracing = \"0.1\"\nlog-like = \"1\"\n";
        assert!(findings(&[("crates/c1/Cargo.toml", ok)], &[]).is_empty());
    }

    #[test]
    fn a_print_in_library_code_fails() {
        for m in MACROS {
            let src = format!("pub fn f() {{ {m}!(\"x\"); }}\n");
            let f = findings(&[(LIB, &src)], &[]);
            assert_eq!(f.len(), 2, "{m}: {f:?}");
            assert!(f[1].contains(&format!("{LIB}:1: `{m}!`")), "{f:?}");
        }
        let nested = "fn f() { std::eprintln!(\"x\"); }\n";
        assert_eq!(findings(&[(LIB, nested)], &[]).len(), 2);
        let path = "fn f() { std::println!(\"x\"); }\n";
        let f = findings(&[(LIB, path)], &[]);
        assert!(f[1].contains("`println!`"), "{f:?}");
        let spaced = "fn f() { println !(\"x\"); }\n";
        assert_eq!(findings(&[(LIB, spaced)], &[]).len(), 2);
    }

    #[test]
    fn a_print_where_printing_is_allowed_passes() {
        let print = "fn main() { println!(\"x\"); }\n";
        for path in [
            "crates/c1/src/main.rs",
            "crates/c1/src/bin/tool.rs",
            "crates/c1/src/bin/tool/mod.rs",
            "crates/c1/tests/area/main.rs",
            "crates/c1/examples/demo.rs",
            "crates/c1/benches/b.rs",
            "crates/c1/build.rs",
            "crates/c1/src/node/tests.rs",
            "crates/c1/src/node/tests/case.rs",
        ] {
            assert!(findings(&[(path, print)], &[]).is_empty(), "{path}");
        }
    }

    #[test]
    fn a_cfg_test_module_a_comment_and_a_string_are_not_code() {
        let src = "\
/// `println!(\"doc\")`
// eprintln!(\"comment\")
pub fn f() -> &'static str { \"dbg!(x) and print!\" }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { println!(\"in a test\"); }
}
";
        let f = findings(&[(LIB, src)], &[]);
        assert!(f.is_empty(), "{f:?}");
        let after = format!("{src}pub fn g() {{ dbg!(1); }}\n");
        assert_eq!(findings(&[(LIB, &after)], &[]).len(), 2);
    }

    #[test]
    fn a_cfg_test_fn_with_a_semicolon_in_its_signature_is_not_code() {
        let src = "#[cfg(test)] fn f(x: [u8; 3]) { println!(); }\npub fn live() {}\n";
        assert!(findings(&[(LIB, src)], &[]).is_empty());
        let live = "#[cfg(not(test))] fn f(x: [u8; 3]) { println!(); }\n";
        assert_eq!(findings(&[(LIB, live)], &[]).len(), 2);
    }

    #[test]
    fn a_write_to_a_standard_stream_handle_is_a_print() {
        let src = "fn f() { let _ = writeln!(std::io::stderr(), \"x\"); }\n";
        let f = findings(&[(LIB, src)], &[]);
        assert!(f[1].contains("`stderr()`"), "{f:?}");
        let out = "fn f() { std::io::stdout().flush(); }\n";
        assert_eq!(findings(&[(LIB, out)], &[]).len(), 2);
        assert!(findings(&[("crates/c1/src/main.rs", src)], &[]).is_empty());
        let method = "fn f(c: &mut C) { c.stdout(1).stderr(2); }\nfn stderr() {}\n";
        assert!(findings(&[(LIB, method)], &[]).is_empty());
        let named = "fn f(stderr: u8, stdout_lines: u8) { let _ = (stderr, stdout_lines); }\n";
        assert!(findings(&[(LIB, named)], &[]).is_empty());
    }

    #[test]
    fn a_word_that_only_contains_a_macro_name_is_not_a_print() {
        let src = "fn f() { reprint!(); let print = 1; let _ = print; sprintln(); }\n";
        assert!(findings(&[(LIB, src)], &[]).is_empty());
    }

    #[test]
    fn an_allowed_row_passes_and_is_counted() {
        let src = "fn f() { println!(); }\n";
        let allow = [(LIB, "permanent: a harness")];
        let r = check(&tree(&[(LIB, src)]), &allow);
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[0].contains("1 allowed"), "{:?}", r.out);
        let dep = [("crates/c1/Cargo.toml", "[dependencies]\nlog = \"0.4\"\n")];
        let allow = [("crates/c1/Cargo.toml", "temporary: a lane converts it")];
        assert!(check(&tree(&dep), &allow).passed());
    }

    #[test]
    fn a_stale_allow_row_fails() {
        let allow = [(LIB, "permanent: a harness")];
        let f = findings(&[(LIB, "pub fn f() {}\n")], &allow);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[1].contains("ALLOW crates/c1/src/lib.rs: stale"), "{f:?}");
    }

    #[test]
    fn an_allow_reason_names_its_kind() {
        let allow = [(LIB, "a harness")];
        let f = findings(&[(LIB, "fn f() { dbg!(); }\n")], &allow);
        assert!(
            f.iter().any(|l| l.contains("`permanent:` or `temporary:`")),
            "{f:?}"
        );
    }
}
