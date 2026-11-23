//! `check-values`: a value is written with `set` on a target, not by a setter, and what
//! is not yet so is a row of [`LIST`].
//!
//! An app writes a value one way: `Session::set` or `Values::set` over an assignment built
//! from a target. The gate reads every `crates/tutti-session/src/**/*.rs` (not a `tests`
//! directory, a `tests.rs` file or a `#[cfg(test)]` item), comments and literals blanked,
//! and finds the public functions that write a value another way:
//!
//! - `setter`: a `pub fn` named `set_*`, on any owner or none.
//! - `set`: a `pub fn` named `set`.
//! - `unit`: a `pub` method (a receiver) that returns `()` (no return type, or `-> ()`)
//!   and has a parameter whose type names a unit of `tutti-units` (`Hz`, `Seconds`, ..).
//!
//! The units are read from `crates/tutti-units/src`: every `struct Name(f32)` or
//! `struct Name(f64)` (a `*OutOfRange` error aside).
//!
//! **Blind spots.** The scan is lexical: it does not see an item a macro writes, an owner
//! reached through a type alias, whether a `pub` type is reachable from the crate root, or
//! a unit behind a type alias or a generic parameter. A `pub(..)` function is not public.
//!
//! **Keys.** `<file>::<Owner>::<fn>`, or `<file>::<fn>` for a free function. Items that
//! share a key and a rule (an `impl Foo<f32>` and an `impl Foo<f64>`) share one row.
//!
//! **Lists.** [`ALLOW`] holds the functions that are the one way, by `Owner::fn`, each
//! with its reason; a violation a row covers is not counted, and a row that covers none
//! fails. [`LIST`] holds `<rule>  <key>  <reason>` lines (`#` lines and blank lines aside),
//! today's violations that a later change removes; the gate fails on a violation with no
//! row and on a row with no violation, so the list only shrinks.

use super::buffers::{blocks, item_end, split_top, squash};
use super::lex::{
    blank, find_word_from, ident_at, matching, skipped_library, strip_attrs, test_ranges,
};
use super::ratchet;
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

/// The baseline: today's violations, `<rule>  <key>  <reason>` per line.
pub(crate) const LIST: &str = "xtask/values-baseline.txt";

/// The functions that are the one way to write a value: `Owner::fn` and its reason.
pub(super) const ALLOW: &[(&str, &str)] = &[
    (
        "Session::set",
        "the one write: it takes assignments built from targets",
    ),
    (
        "Values::set",
        "the session's write for any control-side thread: the same assignments",
    ),
    (
        "Gesture::set",
        "a drag's write: absolute values for the targets the gesture was opened over, through the one batch of `set`",
    ),
];

const ROOT: &str = "crates/tutti-session/src";
const UNITS: &str = "crates/tutti-units/src";

/// The fewest files a real run reads, and the fewest units it finds; fewer means the walk
/// is broken.
pub(super) const MIN_FILES: usize = 10;
pub(super) const MIN_UNITS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    Setter,
    Set,
    Unit,
}

impl Rule {
    fn name(self) -> &'static str {
        match self {
            Rule::Setter => "setter",
            Rule::Set => "set",
            Rule::Unit => "unit",
        }
    }

    fn parse(name: &str) -> Option<Rule> {
        [Rule::Setter, Rule::Set, Rule::Unit]
            .into_iter()
            .find(|r| r.name() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Site {
    rule: Rule,
    key: String,
    /// `Owner::fn`, or the bare function name.
    short: String,
}

fn is_receiver(param: &str) -> bool {
    let p = squash(strip_attrs(param));
    let p = p.trim();
    (p.ends_with("self") && !p.contains(':')) || p.starts_with("self:")
}

/// The units of `tutti-units`: `struct Name(f32)` and `struct Name(f64)`.
fn units(tree: &dyn Tree) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for file in tree.files(UNITS, &skipped_library) {
        if !file.ends_with(".rs") || file.ends_with("/tests.rs") {
            continue;
        }
        let Some(src) = tree.read(&file) else {
            continue;
        };
        let code = blank(&src);
        let mut from = 0;
        while let Some(at) = find_word_from(&code, "struct", from) {
            from = at + 6;
            let (name, end) = ident_at(&code, from);
            let rest = code[end..].trim_start();
            if name.is_empty() || name.ends_with("OutOfRange") || !rest.starts_with('(') {
                continue;
            }
            let open = code.len() - rest.len();
            let Some(close) = matching(&code, open, b'(', b')') else {
                continue;
            };
            let field = squash(&code[open + 1..close]);
            let field = field.trim();
            let field = field.strip_prefix("pub").unwrap_or(field).trim();
            if field == "f32" || field == "f64" {
                out.insert(name.to_owned());
            }
        }
    }
    out
}

/// The names of the types a file declares (`struct`, `enum`).
fn declared(code: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for kw in ["struct", "enum"] {
        let mut from = 0;
        while let Some(at) = find_word_from(code, kw, from) {
            from = at + kw.len();
            let (name, _) = ident_at(code, from);
            if !name.is_empty() {
                out.insert(name.to_owned());
            }
        }
    }
    out
}

/// Whether a parameter's type names one of `units`.
fn names_unit(param: &str, units: &BTreeSet<String>) -> bool {
    let Some((_, ty)) = param.split_once(':') else {
        return false;
    };
    ty.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|w| units.contains(w))
}

fn scan(path: &str, src: &str, units: &BTreeSet<String>) -> BTreeSet<Site> {
    let code = blank(src);
    let tests = test_ranges(&code);
    let in_test = |p: usize| tests.iter().any(|&(a, z)| a <= p && p <= z);
    let blocks = blocks(&code);
    let owner_of = |p: usize| {
        blocks
            .iter()
            .filter(|b| b.open < p && p < b.close)
            .max_by_key(|b| b.open)
    };
    let mut sites = BTreeSet::new();
    let mut from = 0;
    while let Some(p) = find_word_from(&code, "pub", from) {
        from = p + 3;
        if in_test(p) || code[from..].trim_start().starts_with('(') {
            continue;
        }
        let mut at = from;
        let (w, end) = loop {
            let (w, end) = ident_at(&code, at);
            match w {
                "const" | "async" | "unsafe" => at = end,
                "extern" => {
                    let rest = code[end..].trim_start();
                    at = end;
                    if rest.starts_with('"') {
                        let open = code[end..].find('"').map_or(end, |q| end + q);
                        at = code[open + 1..].find('"').map_or(open, |q| open + 1 + q) + 1;
                    }
                }
                _ => break (w, end),
            }
        };
        if w != "fn" {
            continue;
        }
        let (name, mut cur) = ident_at(&code, end);
        if code[cur..].trim_start().starts_with('<') {
            let lt = cur + (code[cur..].len() - code[cur..].trim_start().len());
            let Some(gt) = matching(&code, lt, b'<', b'>') else {
                continue;
            };
            cur = gt + 1;
        }
        let rest = code[cur..].trim_start();
        if !rest.starts_with('(') {
            continue;
        }
        let open = code.len() - rest.len();
        let Some(close) = matching(&code, open, b'(', b')') else {
            continue;
        };
        let params: Vec<&str> = split_top(&code[open + 1..close])
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        let tail = &code[close + 1..item_end(&code, close + 1)];
        let tail = find_word_from(tail, "where", 0).map_or(tail, |w| &tail[..w]);
        let ret = squash(tail.trim().strip_prefix("->").unwrap_or(""));
        let owner = owner_of(p).map(|b| b.owner.as_str());
        let (key, short) = match owner {
            Some(o) => (format!("{path}::{o}::{name}"), format!("{o}::{name}")),
            None => (format!("{path}::{name}"), name.to_owned()),
        };
        let mut hit = |rule| {
            sites.insert(Site {
                rule,
                key: key.clone(),
                short: short.clone(),
            });
        };
        if name.starts_with("set_") {
            hit(Rule::Setter);
        }
        if name == "set" {
            hit(Rule::Set);
        }
        let unit_ret = ret.trim().is_empty() || ret.trim() == "()";
        if unit_ret
            && params.first().is_some_and(|f| is_receiver(f))
            && params.iter().skip(1).any(|p| names_unit(p, units))
        {
            hit(Rule::Unit);
        }
        from = close;
    }
    sites
}

/// `(rule, key, reason)` rows of the baseline.
fn read(tree: &dyn Tree) -> Result<BTreeMap<(Rule, String), String>, Report> {
    let rows = ratchet::read_rows(tree, "check-values", LIST, |at, line, errors| {
        let row = (|| {
            let (rule, rest) = line.split_once(char::is_whitespace)?;
            let rule = Rule::parse(rule)?;
            let (key, reason) = rest.trim_start().split_once(char::is_whitespace)?;
            let reason = reason.trim();
            (key.contains("::") && !reason.is_empty())
                .then(|| ((rule, key.to_owned()), reason.to_owned()))
        })();
        if row.is_none() {
            errors.push(format!(
                "{LIST}:{at}: not `<rule>  <file>::<Owner>::<fn>  <reason>`: {line}"
            ));
        }
        row
    })?;
    Ok(rows.into_iter().collect())
}

pub(super) fn check(tree: &dyn Tree, min_files: usize, min_units: usize) -> Report {
    let mut r = Report::default();
    let rows = match read(tree) {
        Ok(rows) => rows,
        Err(r) => return r,
    };
    let units = units(tree);
    let files: Vec<String> = tree
        .files(ROOT, &skipped_library)
        .into_iter()
        .filter(|f| f.ends_with(".rs") && !f.ends_with("/tests.rs"))
        .collect();
    if files.len() < min_files || units.len() < min_units {
        r.fail(format!(
            "check-values: FAIL: {} files read under {ROOT} (at least {min_files} expected) and \
             {} units under {UNITS} (at least {min_units}); the walk is broken",
            files.len(),
            units.len()
        ));
        return r;
    }
    // A type the session declares under a unit's name (`Phase`) is not that unit.
    let sources: Vec<(&String, String)> = files
        .iter()
        .filter_map(|f| tree.read(f).map(|src| (f, src)))
        .collect();
    let own: BTreeSet<String> = sources
        .iter()
        .flat_map(|(_, src)| declared(&blank(src)))
        .collect();
    let units: BTreeSet<String> = units.difference(&own).cloned().collect();
    let mut found = BTreeSet::new();
    for (file, src) in &sources {
        found.extend(scan(file, src, &units));
    }
    let mut used = BTreeSet::new();
    let sites: BTreeSet<(Rule, String)> = found
        .iter()
        .filter(|s| match ALLOW.iter().position(|(k, _)| *k == s.short) {
            Some(i) => {
                used.insert(i);
                false
            }
            None => true,
        })
        .map(|s| (s.rule, s.key.clone()))
        .collect();
    let fmt = |(rule, key): &(Rule, String)| format!("{}  {key}", rule.name());
    let new: Vec<_> = sites.iter().filter(|s| !rows.contains_key(*s)).collect();
    let stale: Vec<_> = rows.keys().filter(|k| !sites.contains(*k)).collect();
    if !new.is_empty() {
        r.fail(
            "check-values: FAIL (a value is written with `set` on a target: `Session::set` or \
             `Values::set` over `target.to(v)` or `target.modify(f)`; a public `set_*`, a `set`, or \
             a `()`-returning method that takes a unit is a row of the list or the gate's ALLOW):",
        );
        new.into_iter()
            .for_each(|s| r.fail(format!("  {}", fmt(s))));
    }
    if !stale.is_empty() {
        r.fail(format!(
            "check-values: FAIL (a row of {LIST} names no violation; delete it):"
        ));
        stale
            .into_iter()
            .for_each(|s| r.fail(format!("  {}", fmt(s))));
    }
    let idle: Vec<_> = (0..ALLOW.len()).filter(|i| !used.contains(i)).collect();
    if !idle.is_empty() {
        r.fail("check-values: FAIL (a row of ALLOW covers no function; delete it):");
        for i in idle {
            r.fail(format!("  {}: {}", ALLOW[i].0, ALLOW[i].1));
        }
    }
    if r.passed() {
        r.say(format!(
            "check-values: OK ({} files, {} units, {} rows, {} allow rows)",
            files.len(),
            units.len(),
            rows.len(),
            ALLOW.len()
        ));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const HZ: &str = "crates/tutti-units/src/units/hz.rs";
    const A: &str = "crates/tutti-session/src/a.rs";
    const SESSION: &str = "crates/tutti-session/src/session.rs";
    const KNOBS: &str = "crates/tutti-session/src/knobs.rs";
    const GESTURE: &str = "crates/tutti-session/src/gesture.rs";
    const ONE_WAY: &str = "impl Session { pub fn set(&self, a: u8) {} }\nimpl Values { pub fn set(&self, a: u8) {} }\nimpl Gesture { pub fn set(&mut self, a: u8) {} }";

    fn run(files: &[(&str, &str)], list: &str) -> Report {
        let mut all: Vec<(&str, &str)> = files.to_vec();
        all.push((LIST, list));
        all.push((HZ, "pub struct Hz(pub f32);\npub struct Seconds(pub f64);"));
        all.push((SESSION, "impl Session { pub fn set(&self, a: u8) {} }"));
        all.push((KNOBS, "impl Values { pub fn set(&self, a: u8) {} }"));
        all.push((GESTURE, "impl Gesture { pub fn set(&mut self, a: u8) {} }"));
        check(&Mem::with(&all), 0, 0)
    }

    fn fails_on(r: &Report, text: &str) -> bool {
        !r.passed() && r.findings.iter().any(|l| l.contains(text))
    }

    #[test]
    fn a_public_setter_without_a_row_fails_with_its_key() {
        for src in [
            "impl S { pub fn set_gain(&mut self, g: u8) {} }",
            "pub fn set_all() {}",
            "impl S { pub const fn set_x(&self) {} }",
        ] {
            let r = run(&[(A, src)], "");
            assert!(
                fails_on(&r, "setter  crates/tutti-session/src/a.rs::"),
                "{src}"
            );
        }
        let r = run(&[(A, "impl S { pub fn set_gain(&mut self) {} }")], "");
        assert!(fails_on(&r, "a.rs::S::set_gain"));
    }

    #[test]
    fn a_public_set_that_is_not_an_allow_row_fails() {
        let r = run(&[(A, "impl Dial { pub fn set(&self, v: u8) {} }")], "");
        assert!(fails_on(
            &r,
            "set  crates/tutti-session/src/a.rs::Dial::set"
        ));
    }

    #[test]
    fn gesture_set_is_an_allow_row_and_an_idle_one_fails() {
        let r = run(
            &[(A, "impl Gesture { pub fn set(&mut self, v: u8) {} }")],
            "",
        );
        assert!(r.passed(), "{:?}", r.findings);
        let r = check(
            &Mem::with(&[
                (LIST, ""),
                (SESSION, "impl Session { pub fn set(&self, a: u8) {} }"),
                (KNOBS, "impl Values { pub fn set(&self, a: u8) {} }"),
            ]),
            0,
            0,
        );
        assert!(fails_on(&r, "Gesture::set"));
    }

    #[test]
    fn a_unit_returning_method_taking_a_unit_fails_and_other_shapes_pass() {
        for src in [
            "impl S { pub fn nudge(&self, by: Hz) {} }",
            "impl S { pub fn nudge(&mut self, by: Hz) -> () {} }",
            "impl S { pub fn nudge(&self, n: u8, by: impl Into<Seconds>) {} }",
            "impl S { pub fn nudge(&self, by: &Hz) {} }",
        ] {
            let r = run(&[(A, src)], "");
            assert!(
                fails_on(&r, "unit  crates/tutti-session/src/a.rs::S::nudge"),
                "{src}"
            );
        }
        for src in [
            "impl S { pub fn nudge(&self, by: Hz) -> bool { true } }",
            "impl S { pub fn nudge(&self, by: Hz) -> Result<(), E> { Ok(()) } }",
            "impl S { pub fn nudge(&self, by: u32) {} }",
            "impl S { pub fn nudge(by: Hz) {} }",
            "impl S { pub fn nudge(&self, by: HzLike) {} }",
            "impl S { pub(crate) fn nudge(&self, by: Hz) {} }",
            "impl S { fn nudge(&self, by: Hz) {} }",
            "impl S { pub fn nudge(&self, by: Hz) -> Hz { by } }",
        ] {
            let r = run(&[(A, src)], "");
            assert!(r.passed(), "{src}: {:?}", r.findings);
        }
    }

    #[test]
    fn restricted_test_and_commented_items_pass() {
        let src = "pub(crate) fn set_a() {}\nfn set_b() {}\n// pub fn set_c() {}\n\
                   const S: &str = \"pub fn set_d() {}\";\n\
                   #[cfg(test)]\nmod t { pub fn set_e() {} }";
        let r = run(&[(A, src)], "");
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_row_with_its_reason_covers_a_violation_and_a_stale_row_fails() {
        let row =
            "setter  crates/tutti-session/src/a.rs::S::set_gain  goes at the editor-table targets";
        let src = "impl S { pub fn set_gain(&mut self) {} }";
        let r = run(&[(A, src)], row);
        assert!(r.passed(), "{:?}", r.findings);
        let r = run(&[(A, "impl S { pub fn gain(&mut self) {} }")], row);
        assert!(fails_on(&r, "names no violation"));
        let r = run(
            &[(A, src)],
            "setter  crates/tutti-session/src/a.rs::S::set_x  r\n",
        );
        assert!(fails_on(&r, "set_gain"));
        assert!(fails_on(&r, "set_x"));
    }

    #[test]
    fn a_row_is_for_one_rule_and_one_file() {
        let src = "impl S { pub fn set_gain(&mut self) {} }";
        let r = run(
            &[(A, src)],
            "unit  crates/tutti-session/src/a.rs::S::set_gain  r\n",
        );
        assert!(!r.passed());
        let b = "crates/tutti-session/src/b.rs";
        let r = run(
            &[(A, src), (b, src)],
            "setter  crates/tutti-session/src/a.rs::S::set_gain  r\n",
        );
        assert_eq!(
            r.findings
                .iter()
                .filter(|l| l.contains("b.rs::S::set_gain"))
                .count(),
            1
        );
    }

    #[test]
    fn the_allow_rows_are_not_counted_and_an_idle_one_fails() {
        let r = check(&Mem::with(&[(LIST, ""), (SESSION, ONE_WAY)]), 0, 0);
        assert!(r.passed(), "{:?}", r.findings);
        let r = check(
            &Mem::with(&[(LIST, ""), (SESSION, "impl Session {}")]),
            0,
            0,
        );
        assert!(fails_on(&r, "Session::set"));
        assert!(fails_on(&r, "Values::set"));
    }

    #[test]
    fn a_malformed_list_fails() {
        for list in [
            "setter  x.rs::S::f\n",
            "wrong  x.rs::S::f  r\n",
            "setter  nofile  r\n",
        ] {
            let r = run(&[], list);
            assert!(fails_on(&r, "malformed"), "{list}");
        }
    }

    #[test]
    fn the_units_are_the_float_newtypes_of_tutti_units() {
        let tree = Mem::with(&[(
            HZ,
            "pub struct Hz(pub f32);\nunit! { pub struct Db(pub f32): Display }\n\
             pub struct Wide(f64);\npub struct Pair(f32, f32);\npub struct Id(pub u32);\n\
             pub struct BpmOutOfRange(f32);",
        )]);
        let u = units(&tree);
        assert_eq!(
            u.iter().map(String::as_str).collect::<Vec<_>>(),
            ["Db", "Hz", "Wide"]
        );
    }

    #[test]
    fn a_session_type_named_like_a_unit_is_not_the_unit() {
        let src = "pub enum Hz { A }\nimpl S { pub fn go(&self, by: Hz) {} }";
        let r = run(&[(A, src)], "");
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_short_walk_fails() {
        let r = check(&Mem::with(&[(LIST, "")]), 1, 0);
        assert!(fails_on(&r, "the walk is broken"));
        let r = check(&Mem::with(&[(LIST, "")]), 0, 1);
        assert!(fails_on(&r, "the walk is broken"));
    }
}
