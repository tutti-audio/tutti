//! `check-platform`: time comes from `tutti_platform`, so one `Instant` reads the clock
//! on every target.
//!
//! std's `Instant::now` and `SystemTime::now` panic on `wasm32-unknown-unknown`;
//! `tutti_platform::{Instant, SystemTime}` are std's types on a native target and read
//! the browser's clock on the web. The compiler cannot hold the rule (on a native target
//! the two are one type, so a lint on std's would flag the platform's too), so the gate
//! reads the source.
//!
//! In every `.rs` file under `crates/` and `testing/` except `crates/tutti-platform/`, with
//! comments and string literals blanked, it refuses:
//!
//! - `std::time::Instant`, named by a path, a `use`, a group (`use std::time::{Duration,
//!   Instant}`), a glob (`use std::time::*`) or through the module (`use std::time;` then
//!   `time::Instant`);
//! - `std::time::SystemTime::now`, by its path or through the module, and
//!   `SystemTime::now` where the file imports `SystemTime` from `std::time` (under its
//!   alias, when it has one).
//!
//! `SystemTime` as a type stays std's: `fs::Metadata::modified` returns it, so a file
//! stamp keeps it. `Duration` is plain arithmetic and stays `std::time::Duration`. `xtask`
//! is exempt: it is std only.

use std::collections::BTreeSet;

use super::lex::{blank, find_word_from, line_of, skipped};
use super::use_tree::item_uses;
use super::{Report, Tree};

const GATE: &str = "check-platform";
const ROOTS: [&str; 2] = ["crates", "testing"];
const EXEMPT_PREFIX: &str = "crates/tutti-platform/";

/// The fewest source files a real run reads; fewer means the walk is broken.
pub(super) const MIN_FILES: usize = 500;

/// Whether `path` names `std::time` (a leading `::` already stripped by the use tree).
fn is_std_time(path: &[String]) -> bool {
    matches!(path, [a, b] if a == "std" && b == "time")
}

fn named_by(code: &str, needle: &str, mut report: impl FnMut(usize)) {
    let mut from = 0;
    while let Some(at) = find_word_from(code, needle, from) {
        from = at + needle.len();
        report(at);
    }
}

/// `line: what` of each std clock in the source `text`, sorted.
fn hits(text: &str) -> Vec<(usize, &'static str)> {
    let code = blank(text);
    let mut found: BTreeSet<(usize, &'static str)> = BTreeSet::new();
    let mut system_time_names: Vec<String> = Vec::new();
    let mut time_module_names: Vec<String> = Vec::new();

    named_by(&code, "std::time::Instant", |at| {
        found.insert((line_of(&code, at), "std::time::Instant"));
    });
    named_by(&code, "std::time::SystemTime::now", |at| {
        found.insert((line_of(&code, at), "std::time::SystemTime::now"));
    });

    for (at, leaves) in item_uses(&code) {
        let line = line_of(&code, at);
        for leaf in leaves {
            if is_std_time(&leaf.path) {
                match leaf.source.as_str() {
                    "Instant" => {
                        found.insert((line, "std::time::Instant"));
                    }
                    "SystemTime" => system_time_names.push(leaf.name.clone()),
                    "*" => {
                        found.insert((line, "a glob of std::time, which holds Instant"));
                    }
                    _ => {}
                }
            } else if leaf.path == ["std"] && leaf.source == "time" {
                time_module_names.push(leaf.name.clone());
            }
        }
    }

    for name in &system_time_names {
        named_by(&code, &format!("{name}::now"), |at| {
            if !code[..at].ends_with("::") {
                found.insert((line_of(&code, at), "SystemTime::now from std::time"));
            }
        });
    }
    for name in &time_module_names {
        named_by(&code, &format!("{name}::Instant"), |at| {
            if !code[..at].ends_with("::") {
                found.insert((line_of(&code, at), "std::time::Instant"));
            }
        });
        named_by(&code, &format!("{name}::SystemTime::now"), |at| {
            if !code[..at].ends_with("::") {
                found.insert((line_of(&code, at), "std::time::SystemTime::now"));
            }
        });
    }
    found.into_iter().collect()
}

pub(super) fn check(tree: &dyn Tree, min_files: usize) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let mut files = 0;
    for dir in ROOTS {
        for path in tree.files(dir, &skipped) {
            if !path.ends_with(".rs") || path.starts_with(EXEMPT_PREFIX) {
                continue;
            }
            let Some(text) = tree.read(&path) else {
                continue;
            };
            files += 1;
            for (line, what) in hits(&text) {
                bad.push(format!(
                    "{path}:{line}: `{what}`; name `Instant` and `SystemTime::now` through `tutti_platform`, whose clock runs on wasm"
                ));
            }
        }
    }
    if files < min_files {
        bad.push(format!(
            "only {files} source files found under crates/ and testing/; the scan is broken"
        ));
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!("{GATE}: ok ({files} source files)"));
    } else {
        r.fail(format!("{GATE}: FAIL"));
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
        check(&Mem::with(files), 0)
    }

    fn fails(path: &str, src: &str) -> bool {
        !run(&[(path, src)]).findings.is_empty()
    }

    const LIB: &str = "crates/tutti-engine/src/lib.rs";

    #[test]
    fn a_direct_std_instant_path_fails() {
        assert!(fails(
            LIB,
            "fn f() { let t = std::time::Instant::now(); }\n"
        ));
        assert!(fails(LIB, "use std::time::Instant;\n"));
        assert!(fails(LIB, "fn f(t: ::std::time::Instant) {}\n"));
    }

    #[test]
    fn instant_in_a_std_time_group_fails() {
        assert!(fails(LIB, "use std::time::{Duration, Instant};\n"));
        assert!(fails(
            LIB,
            "use std::{sync::Arc, time::{Duration, Instant}};\n"
        ));
        assert!(fails(LIB, "use std::time::{Instant as Clock};\n"));
    }

    #[test]
    fn a_glob_or_the_module_import_of_std_time_fails_when_it_reaches_instant() {
        assert!(fails(LIB, "use std::time::*;\n"));
        assert!(fails(
            LIB,
            "use std::time;\nfn f() { time::Instant::now(); }\n"
        ));
        assert!(!fails(
            LIB,
            "use std::time;\nfn f() { time::Duration::ZERO; }\n"
        ));
    }

    #[test]
    fn system_time_now_through_the_std_time_module_fails() {
        assert!(fails(
            LIB,
            "use std::time;\nfn f() { time::SystemTime::now(); }\n"
        ));
        assert!(fails(
            LIB,
            "use std::time as t;\nfn f() { t::SystemTime::now(); }\n"
        ));
    }

    #[test]
    fn a_use_inside_a_function_body_is_read() {
        assert!(fails(
            LIB,
            "fn f() {\n    use std::time::Instant;\n    Instant::now();\n}\n"
        ));
    }

    #[test]
    fn duration_alone_in_a_group_passes() {
        assert!(!fails(LIB, "use std::time::{Duration};\n"));
        assert!(!fails(
            LIB,
            "use std::time::Duration;\nfn f() -> Duration { Duration::ZERO }\n"
        ));
        assert!(!fails(LIB, "use std::time::{Duration, UNIX_EPOCH};\n"));
    }

    #[test]
    fn the_platform_instant_passes() {
        assert!(!fails(
            LIB,
            "use tutti_platform::Instant;\nfn f() { Instant::now(); tutti_platform::Instant::now(); }\n"
        ));
    }

    #[test]
    fn a_mention_in_a_comment_or_a_string_passes() {
        let src = "// std::time::Instant is std's\n/// `use std::time::{Duration, Instant};`\n\
                   fn f() { let s = \"std::time::Instant::now()\"; }\n";
        assert!(!fails(LIB, src));
    }

    #[test]
    fn the_platform_crate_is_exempt() {
        let src = "pub use std::time::Instant;\nfn f() { std::time::SystemTime::now(); }\n";
        assert!(!fails("crates/tutti-platform/src/clock.rs", src));
        assert!(fails("crates/tutti-platform-extras/src/lib.rs", src));
    }

    #[test]
    fn xtask_is_exempt() {
        assert!(!fails(
            "xtask/src/main.rs",
            "use std::time::Instant;\nfn f() { Instant::now(); }\n"
        ));
    }

    #[test]
    fn testing_crates_and_test_files_are_read_too() {
        assert!(fails(
            "testing/tutti-fixtures/src/lib.rs",
            "use std::time::Instant;\n"
        ));
        assert!(fails(
            "crates/tutti-engine/tests/render/a.rs",
            "use std::time::Instant;\n"
        ));
    }

    #[test]
    fn system_time_as_a_type_passes() {
        let src = "use std::time::SystemTime;\nstruct Mtime(SystemTime);\n\
                   fn f(m: &std::fs::Metadata) -> SystemTime { m.modified().unwrap() }\n\
                   fn g() -> SystemTime { SystemTime::UNIX_EPOCH }\n";
        assert!(!fails(LIB, src));
        assert!(!fails(
            LIB,
            "fn f() { let e = std::time::SystemTime::UNIX_EPOCH; }\n"
        ));
    }

    #[test]
    fn system_time_now_fails_through_std_however_it_is_named() {
        assert!(fails(LIB, "fn f() { std::time::SystemTime::now(); }\n"));
        assert!(fails(
            LIB,
            "use std::time::SystemTime;\nfn f() { SystemTime::now(); }\n"
        ));
        assert!(fails(
            LIB,
            "use std::time::{Duration, SystemTime as Wall};\nfn f() { Wall::now(); }\n"
        ));
    }

    #[test]
    fn the_platform_system_time_now_passes() {
        assert!(!fails(
            LIB,
            "use tutti_platform::SystemTime;\nfn f() { SystemTime::now(); tutti_platform::SystemTime::now(); }\n"
        ));
    }

    #[test]
    fn a_finding_names_its_file_and_line() {
        let r = run(&[(LIB, "fn a() {}\nuse std::time::Instant;\n")]);
        assert!(
            r.findings.iter().any(|l| l.contains(&format!("{LIB}:2:"))),
            "{:?}",
            r.findings
        );
    }

    #[test]
    fn a_walk_that_finds_too_few_files_fails() {
        assert!(
            !check(&Mem::with(&[(LIB, "fn f() {}\n")]), 2)
                .findings
                .is_empty()
        );
    }
}
