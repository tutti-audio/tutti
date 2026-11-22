//! `check-alloc-binaries`: an allocation test is a module of its area's test
//! binary, not a binary of its own.
//!
//! `assert_no_alloc::AllocDisabler` and `tutti_fixtures::RtAllocCheck` (which wraps it)
//! are global allocators, so an area binary (`tests/<area>/main.rs`) installs one once
//! and its allocation tests are modules in it. A file directly under a crate's `tests/`
//! is a binary of its own, and one that installs either as its
//! `#[global_allocator]` is a finding. A file with another allocator (a counting
//! or a no-free one) is not, and nor is a file in a subdirectory.
//!
//! The converse: an area binary under `crates/` or `testing/` installs
//! `tutti_fixtures::RtAllocCheck`, so every realtime render in it is an allocation test.
//! One that installs another global allocator (a counting or a no-free one) is not
//! flagged. One that installs none is a finding unless it is a row of [`EXEMPT`] or
//! [`DEBT`].
//!
//! [`ALLOWED`] is the debt of the first rule, one row per file with what removes it.
//! [`EXEMPT`] is permanent: a binary that cannot or need not install the check, with
//! the reason. [`DEBT`] shrinks: a binary that renders realtime but aborts under the
//! check today, with its first failing test and the allocation site. A row whose
//! binary now installs an allocator, or whose file is gone, is an error, so the lists
//! cannot rot.
//!
//! Limits: a textual scan of code (comments and literals blanked). It sees a
//! `#[global_allocator]` whose `static` item names `AllocDisabler` or `RtAllocCheck` before its
//! `;`.

use super::lex::{blank, find_word_from, line_of};
use super::{Report, Tree};

const SKIPPED_DIRS: &[&str] = &["target", ".git"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

/// `(file, what removes the row)`.
pub(super) const ALLOWED: &[(&str, &str)] = &[];

/// `(area binary, why it never installs `RtAllocCheck`)`: permanent.
pub(super) const EXEMPT: &[(&str, &str)] = &[
    (
        "crates/tutti-platform/tests/main_thread/main.rs",
        "harness = false, and tutti-fixtures depends on tutti-platform, so a dev-dependency is a cycle; never enters an RtScope",
    ),
    (
        "crates/tutti-winmm-ffi/tests/winmm/main.rs",
        "Windows-only tests whose tutti-fixtures dev-dependency is Windows-only; calls winmm, never enters an RtScope",
    ),
    (
        "crates/tutti/tests/error_bounds/main.rs",
        "compile-time trait-bound checks that run no test; never enters an RtScope",
    ),
    (
        "testing/tutti-hrir-convert/tests/convert/main.rs",
        "SOFA file conversion tests; no tutti-fixtures dependency, never enters an RtScope",
    ),
    (
        "testing/tutti-plugin-contract/tests/vst3/main.rs",
        "`contract!` installs RtAllocCheck itself, and a second global allocator is a compile error",
    ),
    (
        "testing/tutti-web-test/tests/web/main.rs",
        "wasm32-only, run in a browser: it renders a Session, but RtAllocCheck is not built for wasm32 and the binary cannot run on the native host, so its renders are checked only by the native suites that cover the same paths",
    ),
];

/// `(area binary, first failing test and its allocation site)`: removed by fixing the
/// test node or helper that allocates inside an `RtScope`.
pub(super) const DEBT: &[(&str, &str)] = &[
    (
        "crates/tutti-engine/tests/render/main.rs",
        "engine_graph::a_tempo_wiggle_under_the_clock_hysteresis_does_not_move_the_beat: Vec::push in Gate::process, tests/render/engine_graph.rs:121",
    ),
    (
        "crates/tutti-graph/tests/mixing/main.rs",
        "mode::a_mode_stored_mid_block_lands_on_the_next_block: GraphSpec::set_mode called from a node's process, src/spec.rs:559 via tests/mixing/mode.rs:1335",
    ),
    (
        "crates/tutti-session/tests/io/main.rs",
        "clock_switch::the_old_driver_renders_its_last_block_before_the_new_one_its_first: Renderer::process, tests/io/clock_switch.rs:628",
    ),
];

/// Whether `path` is `(crates|testing)/<crate>/tests/<area>/main.rs`, an area binary.
fn is_area_binary(path: &str) -> bool {
    let Some(rest) = path
        .strip_prefix("crates/")
        .or_else(|| path.strip_prefix("testing/"))
    else {
        return false;
    };
    let mut parts = rest.split('/');
    matches!(
        (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next()
        ),
        (Some(_), Some("tests"), Some(_), Some("main.rs"), None)
    )
}

/// Whether `path` is `crates/<crate>/tests/<file>.rs`, a test binary of its own.
fn is_top_level_test(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("crates/") else {
        return false;
    };
    let mut parts = rest.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(_), Some("tests"), Some(file), None, None) if file.ends_with(".rs")
    )
}

/// The line of the first `#[global_allocator]` whose static names `AllocDisabler`
/// or `RtAllocCheck`.
fn installs_alloc_disabler(text: &str) -> Option<usize> {
    installs(text, &["AllocDisabler", "RtAllocCheck"])
}

/// Whether `text` installs any `#[global_allocator]`.
fn installs_any_allocator(text: &str) -> bool {
    blank(text).contains("#[global_allocator]")
}

/// The line of the first `#[global_allocator]` whose static names one of `names`.
fn installs(text: &str, names: &[&str]) -> Option<usize> {
    let code = blank(text);
    let mut from = 0;
    while let Some(at) = code[from..].find("#[global_allocator]") {
        let start = from + at;
        let item = &code[start..];
        let end = item.find(';').unwrap_or(item.len());
        if names
            .iter()
            .any(|name| find_word_from(&item[..end], name, 0).is_some())
        {
            return Some(line_of(&code, start));
        }
        from = start + 1;
    }
    None
}

/// The three lists the gate reads.
pub(super) struct Rows<'a> {
    pub allowed: &'a [(&'a str, &'a str)],
    pub exempt: &'a [(&'a str, &'a str)],
    pub debt: &'a [(&'a str, &'a str)],
}

pub(super) const ROWS: Rows<'static> = Rows {
    allowed: ALLOWED,
    exempt: EXEMPT,
    debt: DEBT,
};

pub(super) fn check(tree: &dyn Tree, rows: &Rows) -> Report {
    let allowed = rows.allowed;
    let mut bad: Vec<String> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for path in tree.files("crates", &skipped) {
        if !is_top_level_test(&path) {
            continue;
        }
        let Some(text) = tree.read(&path) else {
            continue;
        };
        let Some(line) = installs_alloc_disabler(&text) else {
            continue;
        };
        match allowed.iter().find(|(p, _)| *p == path) {
            Some((p, _)) => seen.push(p),
            None => bad.push(format!(
                "{path}:{line}: installs AllocDisabler as its own test binary; make it a module of its \
                 area binary (tests/<area>/main.rs installs it once)"
            )),
        }
    }
    for (p, why) in allowed {
        if !seen.contains(p) {
            bad.push(format!("stale allowance {p} ({why})"));
        }
    }

    let mut exempt_seen: Vec<&str> = Vec::new();
    let mut debt_seen: Vec<&str> = Vec::new();
    for path in tree
        .files("crates", &skipped)
        .into_iter()
        .chain(tree.files("testing", &skipped))
    {
        if !is_area_binary(&path) {
            continue;
        }
        let Some(text) = tree.read(&path) else {
            continue;
        };
        if installs_any_allocator(&text) {
            // `RtAllocCheck` or another allocator: nothing is missing. A row for it is
            // stale, which the loops below report.
            continue;
        }
        if let Some((p, _)) = rows.exempt.iter().find(|(p, _)| *p == path) {
            exempt_seen.push(p);
        } else if let Some((p, _)) = rows.debt.iter().find(|(p, _)| *p == path) {
            debt_seen.push(p);
        } else {
            bad.push(format!(
                "{path}: installs no global allocator; install \
                 `tutti_fixtures::RtAllocCheck` so its realtime renders are allocation tests, or add a row \
                 to EXEMPT (permanent, with the reason) or DEBT (the first failing test and its allocation site)"
            ));
        }
    }
    for (p, why) in rows.exempt {
        if !exempt_seen.contains(p) {
            bad.push(format!(
                "stale exempt row {p} ({why}): the binary installs an allocator or is gone"
            ));
        }
    }
    for (p, why) in rows.debt {
        if !debt_seen.contains(p) {
            bad.push(format!(
                "stale debt row {p} ({why}): the binary installs an allocator or is gone"
            ));
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!(
            "check-alloc-binaries: ok ({} allowed, {} exempt, {} debt)",
            seen.len(),
            exempt_seen.len(),
            debt_seen.len()
        ));
    } else {
        r.fail("check-alloc-binaries: FAIL");
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

    const DISABLER: &str = "use assert_no_alloc::AllocDisabler;\n\n#[global_allocator]\nstatic A: AllocDisabler = AllocDisabler;\n";
    const RT_CHECK: &str = "#[global_allocator]\nstatic A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;\n";
    const COUNTING: &str = "#[global_allocator]\nstatic A: Counting = Counting;\n";

    const NONE: &[(&str, &str)] = &[];

    fn run(files: &[(&str, &str)], allowed: &[(&str, &str)]) -> Report {
        run_rows(files, allowed, NONE, NONE)
    }

    fn run_rows(
        files: &[(&str, &str)],
        allowed: &[(&str, &str)],
        exempt: &[(&str, &str)],
        debt: &[(&str, &str)],
    ) -> Report {
        check(
            &Mem::with(files),
            &Rows {
                allowed,
                exempt,
                debt,
            },
        )
    }

    const PLAIN: &str = "mod a;\n";
    const AREA: &str = "crates/tutti-a/tests/area/main.rs";

    #[test]
    fn a_top_level_test_file_installing_alloc_disabler_fails() {
        let r = run(&[("crates/tutti-a/tests/rt_no_alloc.rs", DISABLER)], &[]);
        assert_eq!(
            r.findings,
            [
                "check-alloc-binaries: FAIL",
                "  crates/tutti-a/tests/rt_no_alloc.rs:3: installs AllocDisabler as its own test binary; make it a module of its \
                 area binary (tests/<area>/main.rs installs it once)",
            ]
        );
    }

    #[test]
    fn a_top_level_test_file_installing_rt_alloc_check_fails() {
        let r = run(&[("crates/tutti-a/tests/rt_no_alloc.rs", RT_CHECK)], &[]);
        assert!(!r.passed());
    }

    #[test]
    fn an_area_binary_a_counting_allocator_and_a_comment_pass() {
        let r = run(
            &[
                (AREA, DISABLER),
                ("crates/tutti-a/tests/alloc_budget.rs", COUNTING),
                (
                    "crates/tutti-a/tests/prose.rs",
                    "// #[global_allocator]\n// static A: AllocDisabler = AllocDisabler;\n",
                ),
            ],
            &[],
        );
        assert_eq!(
            r.out,
            ["check-alloc-binaries: ok (0 allowed, 0 exempt, 0 debt)"]
        );
        assert!(r.passed());
    }

    #[test]
    fn an_allowed_file_passes_and_a_stale_row_fails() {
        let allowed = [("crates/tutti-a/tests/old.rs", "temporary: x folds it")];
        let r = run(&[("crates/tutti-a/tests/old.rs", DISABLER)], &allowed);
        assert_eq!(
            r.out,
            ["check-alloc-binaries: ok (1 allowed, 0 exempt, 0 debt)"]
        );

        let r = run(&[("crates/tutti-a/tests/old.rs", COUNTING)], &allowed);
        assert_eq!(
            r.findings,
            [
                "check-alloc-binaries: FAIL",
                "  stale allowance crates/tutti-a/tests/old.rs (temporary: x folds it)",
            ]
        );
    }

    #[test]
    fn an_area_binary_without_an_allocator_is_a_finding() {
        for path in [AREA, "testing/tutti-b/tests/area/main.rs"] {
            let r = run(&[(path, PLAIN)], &[]);
            assert_eq!(
                r.findings,
                [
                    "check-alloc-binaries: FAIL".to_owned(),
                    format!(
                        "  {path}: installs no global allocator; install `tutti_fixtures::RtAllocCheck` so its realtime \
                         renders are allocation tests, or add a row to EXEMPT (permanent, with the reason) or DEBT \
                         (the first failing test and its allocation site)"
                    ),
                ]
            );
        }
    }

    #[test]
    fn an_area_binary_installing_the_check_or_another_allocator_is_not_flagged() {
        let r = run(
            &[
                (AREA, RT_CHECK),
                ("crates/tutti-a/tests/other/main.rs", COUNTING),
            ],
            &[],
        );
        assert!(r.passed());
    }

    #[test]
    fn a_file_directly_under_tests_or_in_a_submodule_is_not_an_area_binary() {
        let r = run(
            &[
                ("crates/tutti-a/tests/flat.rs", PLAIN),
                ("crates/tutti-a/tests/area/other.rs", PLAIN),
                ("crates/tutti-a/tests/area/deep/main.rs", PLAIN),
            ],
            &[],
        );
        assert!(r.passed());
    }

    #[test]
    fn an_exempt_row_and_a_debt_row_cover_a_binary() {
        let exempt = [(AREA, "never enters an RtScope")];
        let r = run_rows(&[(AREA, PLAIN)], NONE, &exempt, NONE);
        assert_eq!(
            r.out,
            ["check-alloc-binaries: ok (0 allowed, 1 exempt, 0 debt)"]
        );

        let debt = [(AREA, "t::first: Vec::push at x.rs:1")];
        let r = run_rows(&[(AREA, PLAIN)], NONE, NONE, &debt);
        assert_eq!(
            r.out,
            ["check-alloc-binaries: ok (0 allowed, 0 exempt, 1 debt)"]
        );
    }

    #[test]
    fn a_stale_exempt_or_debt_row_fails() {
        let rows = [(AREA, "why")];
        let r = run_rows(&[(AREA, RT_CHECK)], NONE, &rows, NONE);
        assert_eq!(
            r.findings,
            [
                "check-alloc-binaries: FAIL",
                "  stale exempt row crates/tutti-a/tests/area/main.rs (why): the binary installs an allocator or is gone",
            ]
        );
        let r = run_rows(&[], NONE, NONE, &rows);
        assert_eq!(
            r.findings,
            [
                "check-alloc-binaries: FAIL",
                "  stale debt row crates/tutti-a/tests/area/main.rs (why): the binary installs an allocator or is gone",
            ]
        );
    }
}
