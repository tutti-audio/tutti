//! `check-threads`: background work is a future on `tutti_platform::Executor`, and nothing
//! outside `tutti-platform` starts an OS thread.
//!
//! A blocking OS edge goes through `Executor::spawn_dedicated`. The threads that exist
//! today are rows of [`LIST`] that only shrink, each migrating to a future; the audio
//! side and the CoreMIDI hub are named in [`EXEMPT`].
//!
//! **Where it reads.** Every `crates/*/src/**/*.rs`, comments and literals blanked, except
//! `crates/tutti-platform/src` (the executor), `tests`, `benches` and `examples`
//! directories, `tests.rs`, `testing.rs`, `test_support.rs` and `*_tests.rs` files, files a
//! `#[cfg(test)] mod` declaration reaches, and `#[cfg(test)]` items.
//!
//! **What it finds.** A call of `spawn_named(`, `thread::spawn(`, `thread::Builder::new()`
//! or `thread::scope(` (with any path before `thread` or `spawn_named`).
//!
//! **Keys.** `<file>::<fn>` for a free function and `<file>::<Owner>::<fn>` for a method,
//! `<file>::-` for a start outside any function. Several starts in one function share a
//! row.
//!
//! **The exemptions** are not baselined: an [`Exempt`] entry names a file whose thread
//! starts are not the executor's to take (the audio side's, or a process-lifetime OS
//! client's), with its reason. An entry that covers no start fails, like a stale row.
//!
//! **List.** [`LIST`] holds `<crate>  <target>  <key>  <owner>` lines, `#` and blank lines
//! aside: the target is `spawn` or `spawn_dedicated` (what the site becomes), the owner a
//! lane. The gate fails on a start with no row, on a row with no start (delete it), on
//! an unknown target or owner, on a key outside its crate, and on a key listed twice, so
//! the list only shrinks.

use super::buffers_loops::functions;
use super::lex::{blank, find_word_from, test_ranges};
use super::test_time::{cfg_test_files, test_file};
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// The baseline: today's thread starts.
pub(crate) const LIST: &str = "xtask/threads-baseline.txt";

/// The walk reads at least this many files, or it is broken.
pub(super) const MIN_FILES: usize = 500;

const TARGETS: [&str; 2] = ["spawn", "spawn_dedicated"];
const OWNERS: [&str; 4] = ["core-foundation", "core-engine", "plugin", "midi"];

/// A file whose thread starts the executor does not take, with its reason.
pub(crate) struct Exempt {
    pub(crate) file: &'static str,
    pub(crate) reason: &'static str,
}

/// The files whose thread starts the executor does not take: the audio side's (a thread
/// there must not allocate, lock or wait on a waker) and the CoreMIDI hub's.
pub(crate) const EXEMPT: &[Exempt] = &[
    Exempt {
        file: "crates/tutti-midi-hardware/src/backend/coremidi/watch.rs",
        reason: "the CoreMIDI hub: a process-static client whose notification loop never returns, so its thread outlives every Executor",
    },
    Exempt {
        file: "crates/tutti-graph/src/pool.rs",
        reason: "the graph's real-time render workers: they must not allocate, lock or wait on a waker",
    },
    Exempt {
        file: "crates/tutti-engine/src/timer.rs",
        reason: "the deviceless audio clock: it paces and renders blocks against OS deadlines",
    },
];

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "tests" | "benches" | "examples")
}

/// A file that is test code by its name.
fn test_named(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    test_file(path)
        || matches!(file, "testing.rs" | "test_support.rs")
        || file.ends_with("_tests.rs")
}

/// The files the gate reads (test-named ones too: a `cfg(test)` declaration in one reaches
/// the files it declares).
fn source_files(tree: &dyn Tree) -> Vec<String> {
    tree.files("crates", &skipped)
        .into_iter()
        .filter(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            p.ends_with(".rs") && parts.get(2) == Some(&"src") && parts[1] != "tutti-platform"
        })
        .collect()
}

/// The byte offsets of every thread start in blanked `code`.
fn starts(code: &str) -> Vec<usize> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "spawn_named", from) {
        from = at + "spawn_named".len();
        if b.get(from) == Some(&b'(') {
            out.push(at);
        }
    }
    // `thread::spawn`, `thread::scope` or `thread::Builder`, called or imported
    // (`use std::thread::spawn;`, `use std::thread::{scope, Builder};`): an import
    // is a start, so a bare `spawn(` after it cannot pass unseen.
    let mut from = 0;
    while let Some(at) = find_word_from(code, "thread", from) {
        from = at + "thread".len();
        let Some(rest) = code[from..].strip_prefix("::") else {
            continue;
        };
        let names_a_start = match rest.strip_prefix('{') {
            Some(group) => group
                .split('}')
                .next()
                .unwrap_or("")
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|word| THREAD_STARTS.contains(&word)),
            None => THREAD_STARTS.iter().any(|name| {
                rest.strip_prefix(name).is_some_and(|after| {
                    !after.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                })
            }),
        };
        if names_a_start {
            out.push(at);
        }
    }
    out.sort_unstable();
    out
}

/// The `std::thread` items that start a thread.
const THREAD_STARTS: [&str; 3] = ["spawn", "scope", "Builder"];

/// The keys of one file's thread starts.
fn scan(path: &str, src: &str, whole_file_is_test: bool) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    if whole_file_is_test {
        return keys;
    }
    let code = blank(src);
    let tests = test_ranges(&code);
    let funcs = functions(&code);
    for at in starts(&code) {
        if tests.iter().any(|&(a, z)| a <= at && at <= z) {
            continue;
        }
        let key = match funcs
            .iter()
            .filter(|f| f.start < at && at < f.close)
            .max_by_key(|f| f.start)
        {
            Some(f) => format!("{path}::{}", f.key),
            None => format!("{path}::-"),
        };
        keys.insert(key);
    }
    keys
}

/// The crate a path under `crates/` is in.
fn crate_of(path: &str) -> &str {
    path.split('/').nth(1).unwrap_or("")
}

/// The rows, key to target; the errors name the line.
fn parse(text: &str) -> Result<BTreeMap<String, String>, Vec<String>> {
    let mut rows = BTreeMap::new();
    let mut errors = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = n + 1;
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [krate, target, key, owner] = parts[..] else {
            errors.push(format!(
                "  {LIST}:{at}: not `<crate>  <target>  <key>  <owner>`: {line}"
            ));
            continue;
        };
        if !TARGETS.contains(&target) {
            errors.push(format!("  {LIST}:{at}: `{target}` is not a target"));
        }
        if !OWNERS.contains(&owner) {
            errors.push(format!("  {LIST}:{at}: `{owner}` is not an owner"));
        }
        if crate_of(key) != krate {
            errors.push(format!("  {LIST}:{at}: `{key}` is not in crate `{krate}`"));
        }
        if rows.insert(key.to_owned(), target.to_owned()).is_some() {
            errors.push(format!("  {LIST}:{at}: `{key}` is listed twice"));
        }
    }
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

pub(super) fn check(tree: &dyn Tree, exempt: &[Exempt], min_files: usize) -> Report {
    let mut r = Report::default();
    let Some(text) = tree.read(LIST) else {
        r.fail(format!("check-threads: FAIL: {LIST} is missing"));
        return r;
    };
    let rows = match parse(&text) {
        Ok(rows) => rows,
        Err(errors) => {
            r.fail(format!("check-threads: FAIL ({LIST} is malformed):"));
            errors.into_iter().for_each(|e| r.fail(e));
            return r;
        }
    };
    let files: BTreeMap<String, String> = source_files(tree)
        .into_iter()
        .filter_map(|p| tree.read(&p).map(|t| (p, t)))
        .collect();
    if files.len() < min_files {
        r.fail(format!(
            "check-threads: FAIL: {} source files read, at least {min_files} expected; the walk is broken",
            files.len()
        ));
        return r;
    }
    let reached = cfg_test_files(&files);
    let mut sites = BTreeSet::new();
    let mut used = BTreeSet::new();
    for (path, text) in &files {
        let whole = reached.contains(path) || test_named(path);
        for key in scan(path, text, whole) {
            match exempt.iter().position(|a| a.file == path) {
                Some(i) => {
                    used.insert(i);
                }
                None => {
                    sites.insert(key);
                }
            }
        }
    }
    let unlisted: Vec<_> = sites.iter().filter(|k| !rows.contains_key(*k)).collect();
    let stale: Vec<_> = rows.keys().filter(|k| !sites.contains(*k)).collect();
    let unused: Vec<_> = (0..exempt.len()).filter(|i| !used.contains(i)).collect();
    if unlisted.is_empty() && stale.is_empty() && unused.is_empty() {
        r.say("check-threads: OK");
    }
    if !unlisted.is_empty() {
        r.fail(format!(
            "check-threads: FAIL (a thread start outside tutti-platform; background work is a future \
             on tutti_platform::Executor, a blocking OS edge goes through Executor::spawn_dedicated; \
             the baseline {LIST} never gains a row):"
        ));
        unlisted.into_iter().for_each(|k| r.fail(format!("  {k}")));
    }
    if !stale.is_empty() {
        r.fail(format!(
            "check-threads: FAIL (a row of {LIST} names no thread start; delete it):"
        ));
        stale.into_iter().for_each(|k| r.fail(format!("  {k}")));
    }
    if !unused.is_empty() {
        r.fail(
            "check-threads: FAIL (an exempt file of the gate holds no thread start; delete it):",
        );
        for i in unused {
            r.fail(format!("  {}", exempt[i].file));
        }
    }
    for &i in &used {
        let a = &exempt[i];
        r.say(format!("check-threads: exempt {}: {}", a.file, a.reason));
    }
    let spawn = rows.values().filter(|v| *v == "spawn").count();
    r.say(format!(
        "check-threads: {} thread starts, {} rows in {LIST} ({spawn} to spawn, {} to spawn_dedicated), {} exempt files",
        sites.len(),
        rows.len(),
        rows.len() - spawn,
        used.len()
    ));
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const SRC: &str = "fn run() { let t = spawn_named(\"x\", None, move |_| {}); }\n";
    const A: &str = "crates/tutti-a/src/lib.rs";
    const ROW: &str = "tutti-a  spawn_dedicated  crates/tutti-a/src/lib.rs::run  plugin\n";

    fn run(list: &str, rt: &[Exempt], files: &[(&str, &str)]) -> Report {
        let mut all = vec![(LIST, list)];
        all.extend_from_slice(files);
        check(&Mem::with(&all), rt, 0)
    }

    #[test]
    fn a_new_spawn_named_fails_and_names_the_file() {
        let r = run("", &[], &[(A, SRC)]);
        assert!(!r.passed());
        assert!(
            r.findings
                .iter()
                .any(|f| f.contains("thread start outside tutti-platform"))
        );
        assert!(r.findings.contains(&format!("  {A}::run")));
    }

    #[test]
    fn every_thread_start_form_is_found() {
        for body in [
            "std::thread::spawn(|| {});",
            "thread::spawn(|| {});",
            "thread::Builder::new().spawn(|| {});",
            "std::thread::scope(|s| {});",
            "thread::scope(|s| {});",
            "tutti_platform::spawn_named(\"x\", None, |_| {});",
        ] {
            let keys = scan(A, &format!("fn run() {{ {body} }}\n"), false);
            assert_eq!(keys.len(), 1, "{body}");
        }
        for body in [
            "thread::sleep(d);",
            "thread::current();",
            "s.spawn(|| {});",
            "x.thread_spawn(1);",
        ] {
            assert!(
                scan(A, &format!("fn run() {{ {body} }}\n"), false).is_empty(),
                "{body}"
            );
        }
    }

    /// An import of a thread start is itself a start, keyed outside any
    /// function, so a bare `spawn(` after it cannot pass unseen; importing
    /// something else from `std::thread` is not.
    #[test]
    fn an_imported_thread_start_is_a_start() {
        for import in [
            "use std::thread::spawn;",
            "use std::thread::{self, scope};",
            "use std::thread::{Builder, sleep};",
            "use std::thread::spawn as go;",
        ] {
            let keys = scan(A, &format!("{import}\nfn run() {{}}\n"), false);
            assert_eq!(
                keys.into_iter().collect::<Vec<_>>(),
                [format!("{A}::-")],
                "{import}"
            );
        }
        for import in [
            "use std::thread::{sleep, current};",
            "use std::thread;",
            "use std::thread::JoinHandle;",
        ] {
            assert!(
                scan(A, &format!("{import}\nfn run() {{}}\n"), false).is_empty(),
                "{import}"
            );
        }
    }

    #[test]
    fn a_baselined_site_passes() {
        let r = run(ROW, &[], &[(A, SRC)]);
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_row_whose_site_is_gone_is_stale_and_fails() {
        let r = run(ROW, &[], &[(A, "fn run() {}\n")]);
        assert!(!r.passed());
        assert!(
            r.findings
                .iter()
                .any(|f| f.contains("names no thread start"))
        );
        assert!(r.findings.contains(&format!("  {A}::run")));
    }

    #[test]
    fn a_site_in_a_cfg_test_module_or_a_test_file_is_ignored() {
        let in_tests = format!("fn p() {{}}\n#[cfg(test)]\nmod tests {{\n{SRC}}}\n");
        let r = run(
            "",
            &[],
            &[
                (A, &in_tests),
                ("crates/tutti-a/src/tests.rs", SRC),
                ("crates/tutti-a/src/testing.rs", SRC),
                ("crates/tutti-a/src/sink_tests.rs", SRC),
                ("crates/tutti-a/tests/it/main.rs", SRC),
            ],
        );
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn tutti_platform_is_ignored() {
        let r = run("", &[], &[("crates/tutti-platform/src/lib.rs", SRC)]);
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn the_exempt_files_pass_without_rows_and_a_stale_entry_fails() {
        let files = [
            ("crates/tutti-graph/src/pool.rs", SRC),
            ("crates/tutti-engine/src/timer.rs", SRC),
            (
                "crates/tutti-midi-hardware/src/backend/coremidi/watch.rs",
                SRC,
            ),
        ];
        let r = run("", EXEMPT, &files);
        assert!(r.passed(), "{:?}", r.findings);
        let r = run("", EXEMPT, &files[..1]);
        assert!(!r.passed());
        assert!(
            r.findings
                .contains(&"  crates/tutti-engine/src/timer.rs".to_owned())
        );
    }

    #[test]
    fn a_malformed_baseline_a_duplicate_and_a_missing_one_fail() {
        for list in [
            "tutti-a  spawn  crates/tutti-a/src/lib.rs::run\n",
            "tutti-a  fork  crates/tutti-a/src/lib.rs::run  plugin\n",
            "tutti-a  spawn  crates/tutti-a/src/lib.rs::run  nobody\n",
            "tutti-b  spawn  crates/tutti-a/src/lib.rs::run  plugin\n",
            &format!("{ROW}{ROW}"),
        ] {
            assert!(!run(list, &[], &[(A, SRC)]).passed(), "{list}");
        }
        assert!(!check(&Mem::with(&[(A, SRC)]), &[], 0).passed());
    }

    #[test]
    fn a_walk_that_reads_too_few_files_fails() {
        let r = check(&Mem::with(&[(LIST, ""), (A, "fn f() {}\n")]), &[], 5);
        assert!(r.findings[0].contains("the walk is broken"));
    }
}
