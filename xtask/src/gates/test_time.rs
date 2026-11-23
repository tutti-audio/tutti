//! `check-test-time`: a test never asserts wall-clock time, and a sleep never stands in
//! for waiting on the event.
//!
//! A real-time limit only guards a hang. The gate reads every `.rs` file under
//! `crates/`, `testing/` and `xtask/` with its comments and literals blanked, and counts,
//! per file and in test code only, two kinds of site.
//!
//! Test code is a file under a `tests` directory, a `tests.rs` file, a file reached
//! through a `#[cfg(test)] mod name;` declaration (and every module that file declares in
//! turn), and the items behind `#[cfg(test)]` (or `#[cfg(all(.., test, ..))]`) in any
//! other file. A declaration resolves as the compiler does: `name.rs` or `name/mod.rs` in
//! the module directory of the declaring file (its own directory for `lib.rs`, `main.rs`
//! and `mod.rs`, a directory named for the file otherwise, plus one per enclosing inline
//! `mod`), or the `#[path = ".."]` literal relative to that directory.
//!
//! - **A wall-clock comparison**: a time value that is compared with `<`, `<=`, `>` or
//!   `>=` (on either side), or is an argument of an `assert*!` macro. A time value is a
//!   `.elapsed()` call, a `.duration_since(..)` call, or `Instant::now() - x`, with any
//!   chain of method calls, `?` and `as` casts after it (`.elapsed().as_secs_f64()`).
//!   A value held in a `let` binding and compared later is not followed. A comparison
//!   in a `while` condition is a wait loop bounded by a deadline, which only guards a
//!   hang, so it is not counted; neither is a deadline held as an `Instant`
//!   (`Instant::now() + timeout`, then `Instant::now() < deadline`), which holds no
//!   `.elapsed()` or subtraction. A busy-wait speed check written as a `while` is
//!   therefore not caught. An `if t.elapsed() > T { panic!(..) }` hang guard is counted:
//!   write it as an `Instant` deadline (`Instant::now() + T`, compared directly), which
//!   is not. Only `<`, `<=`, `>` and `>=` count; `==` and `!=` do not. A value that is
//!   the `.elapsed()` or `.duration_since(..)` of a synthetic or non-wall `Instant` is
//!   counted when compared (a known false positive). In an `assert*!` only the compared
//!   operands count: the first argument of `assert!`, the first two of `assert_eq!` and
//!   `assert_ne!`, never a message argument.
//! - **A sleep**: a call of `thread::sleep(..)` (so `std::thread::sleep(..)` too), or of
//!   a bare `sleep(..)` in a file that imports it from `thread`. A method or a path that
//!   does not end in `thread::sleep` (a primitive under test) is not a sleep, and a
//!   `tokio` or other non-std sleep is not counted.
//!
//! [`BASELINE`] holds `<comparisons> <sleeps> <path>` for each file that held a site
//! when the gate began. A file fails when either count is over its baseline, naming each
//! site of that kind; a file not listed has 0 of each. A file below its baseline passes,
//! and the report says how far the baseline can be lowered. The report prints the
//! baseline lines of the current counts after a failure or slack, so a lane can paste
//! them.

use super::lex::{
    blank, find_word_from, is_test_file, line_of, matching, skipped, test_ranges, word, word_char,
};
use super::use_tree::inline_mods;
use super::{Report, Tree};
use std::collections::BTreeMap;

/// `<comparisons> <sleeps> <path>` lines: each file's counts when the gate began.
pub(crate) const BASELINE: &str = "xtask/test-time.txt";

const GATE: &str = "check-test-time";
const ROOTS: [&str; 3] = ["crates", "testing", "xtask"];

/// A file's counts.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
struct Counts {
    compares: usize,
    sleeps: usize,
}

/// The sites of one file, as printable lines.
#[derive(Default)]
struct Sites {
    compares: Vec<String>,
    sleeps: Vec<String>,
}

impl Sites {
    fn counts(&self) -> Counts {
        Counts {
            compares: self.compares.len(),
            sleeps: self.sleeps.len(),
        }
    }
}

fn parse(text: &str) -> Result<BTreeMap<String, Counts>, String> {
    let mut map = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(3, ' ');
        let row = (|| {
            let c = it.next()?.parse::<usize>().ok()?;
            let s = it.next()?.parse::<usize>().ok()?;
            let p = it.next()?.trim();
            (!p.is_empty()).then(|| {
                (
                    p.to_owned(),
                    Counts {
                        compares: c,
                        sleeps: s,
                    },
                )
            })
        })();
        let (path, counts) = row.ok_or_else(|| {
            format!(
                "{BASELINE}:{}: not `<comparisons> <sleeps> <path>`: {line}",
                n + 1
            )
        })?;
        map.insert(path, counts);
    }
    Ok(map)
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// The end of the method chain, `?` and `as` casts that follow a value ending at `end`.
fn chain_end(code: &str, mut end: usize) -> usize {
    let b = code.as_bytes();
    loop {
        let i = skip_ws(b, end);
        if b.get(i) == Some(&b'?') {
            end = i + 1;
        } else if b.get(i) == Some(&b'.') {
            let j = skip_ws(b, i + 1);
            let name = b[j..].iter().take_while(|&&c| word(c)).count();
            if name == 0 {
                return end;
            }
            end = j + name;
            let k = skip_ws(b, end);
            if b.get(k) == Some(&b'(') {
                match matching(code, k, b'(', b')') {
                    Some(c) => end = c + 1,
                    None => return end,
                }
            }
        } else if code[i..].starts_with("as ") {
            let j = skip_ws(b, i + 2);
            let name = b[j..].iter().take_while(|&&c| word(c)).count();
            if name == 0 {
                return end;
            }
            end = j + name;
        } else {
            return end;
        }
    }
}

/// The start of the receiver expression whose `.method` dot is at `dot`: identifiers,
/// paths, fields and balanced call or index groups.
fn receiver_start(code: &str, dot: usize) -> usize {
    let b = code.as_bytes();
    let mut i = dot;
    while i > 0 {
        let c = b[i - 1];
        if word(c) || c == b'.' || c == b':' {
            i -= 1;
        } else if c == b')' || c == b']' {
            let (o, cl) = if c == b')' {
                (b'(', b')')
            } else {
                (b'[', b']')
            };
            let mut depth = 0;
            let mut j = i - 1;
            loop {
                if b[j] == cl {
                    depth += 1;
                } else if b[j] == o {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                if j == 0 {
                    return i;
                }
                j -= 1;
            }
            i = j;
        } else {
            break;
        }
    }
    i
}

/// Whether a comparison operator follows `at` in `code`.
fn compared_after(code: &str, at: usize) -> bool {
    let rest = &code[skip_ws(code.as_bytes(), at)..];
    rest.starts_with("<=")
        || rest.starts_with(">=")
        || (rest.starts_with('<') && !rest.starts_with("<<"))
        || (rest.starts_with('>') && !rest.starts_with(">>"))
}

/// Whether a comparison operator precedes `at` in `code`.
fn compared_before(code: &str, at: usize) -> bool {
    let b = code.as_bytes();
    let mut i = at;
    while i > 0 && b[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    match (
        i.checked_sub(2).map(|j| b[j]),
        i.checked_sub(1).map(|j| b[j]),
    ) {
        (Some(b'<' | b'>'), Some(b'=')) => true,
        (Some(p), Some(b'<')) => p != b'<',
        (Some(p), Some(b'>')) => !matches!(p, b'>' | b'-' | b'='),
        _ => false,
    }
}

/// Whether `at` sits in a compared operand of an `assert*!` macro call: the first
/// argument of `assert!`, the first two of `assert_eq!` and `assert_ne!`. A message or
/// format argument is not an operand.
fn in_assert(code: &str, at: usize) -> bool {
    let b = code.as_bytes();
    let mut depth = 0usize;
    let mut commas = 0usize;
    let mut i = at;
    while i > 0 {
        i -= 1;
        match b[i] {
            b')' | b']' | b'}' if depth > 0 || b[i] != b'}' => depth += 1,
            b'(' | b'[' if depth > 0 => depth -= 1,
            b'{' if depth > 0 => depth -= 1,
            b'(' => {
                let mut j = i;
                while j > 0 && b[j - 1].is_ascii_whitespace() {
                    j -= 1;
                }
                if j > 0 && b[j - 1] == b'!' {
                    let name_end = j - 1;
                    let mut s = name_end;
                    while s > 0 && word(b[s - 1]) {
                        s -= 1;
                    }
                    let name = &code[s..name_end];
                    if name.contains("assert") {
                        let operands = if name.ends_with("_eq") || name.ends_with("_ne") {
                            2
                        } else {
                            1
                        };
                        return commas < operands;
                    }
                }
                commas = 0;
            }
            b'[' => commas = 0,
            b',' if depth == 0 => commas += 1,
            b';' | b'{' | b'}' if depth == 0 => return false,
            _ => {}
        }
    }
    false
}

/// Whether `at` sits in the condition of a `while` loop: the statement that holds it
/// opens with `while`.
fn in_while_condition(code: &str, at: usize) -> bool {
    let b = code.as_bytes();
    let mut i = at;
    while i > 0 && !matches!(b[i - 1], b';' | b'{' | b'}') {
        i -= 1;
    }
    let stmt = code[i..at].trim_start();
    let stmt = stmt.strip_prefix("else").map_or(stmt, str::trim_start);
    stmt.starts_with("while ") || stmt.starts_with("while(")
}

/// The wall-clock comparisons of `code`'s offsets, as `(start, label)`; `code` is blanked.
fn wall_clock(code: &str) -> Vec<(usize, &'static str)> {
    let b = code.as_bytes();
    // `(value start, value end, label)` of each time value.
    let mut values: Vec<(usize, usize, &'static str)> = Vec::new();
    for (name, label) in [
        ("elapsed", "`.elapsed()`"),
        ("duration_since", "`.duration_since(..)`"),
    ] {
        let mut from = 0;
        while let Some(at) = find_word_from(code, name, from) {
            from = at + name.len();
            let k = skip_ws(b, from);
            let dot = (0..at).rev().find(|&i| !b[i].is_ascii_whitespace());
            if b.get(k) != Some(&b'(') || dot.is_none_or(|d| b[d] != b'.') {
                continue;
            }
            let Some(close) = matching(code, k, b'(', b')') else {
                continue;
            };
            values.push((
                receiver_start(code, dot.expect("checked")),
                chain_end(code, close + 1),
                label,
            ));
        }
    }
    let mut from = 0;
    while let Some(at) = find_word_from(code, "Instant", from) {
        from = at + "Instant".len();
        let Some(rest) = code[from..].strip_prefix("::now") else {
            continue;
        };
        let k = skip_ws(b, from + (code[from..].len() - rest.len()));
        if b.get(k) != Some(&b'(') {
            continue;
        }
        let Some(close) = matching(code, k, b'(', b')') else {
            continue;
        };
        let m = skip_ws(b, close + 1);
        if b.get(m) != Some(&b'-') || matches!(b.get(m + 1), Some(b'=' | b'>')) {
            continue;
        }
        let o = skip_ws(b, m + 1);
        let mut e = o;
        while e < b.len() && (word(b[e]) || b[e] == b'.' || b[e] == b':') {
            e += 1;
        }
        if e == o {
            continue;
        }
        values.push((at, chain_end(code, e), "an `Instant` subtraction"));
    }
    values.sort_unstable();
    let mut out = Vec::new();
    let mut last_end = 0;
    for (s, e, label) in values {
        if s < last_end {
            continue;
        }
        let counted = compared_after(code, e) || compared_before(code, s) || in_assert(code, s);
        if counted && !in_while_condition(code, s) {
            out.push((s, label));
            last_end = e;
        }
    }
    out
}

/// Whether `code` imports `sleep` from `thread` (`use std::thread::sleep;` or
/// `use std::thread::{self, sleep}`).
fn imports_sleep(code: &str) -> bool {
    let mut from = 0;
    while let Some(at) = find_word_from(code, "use", from) {
        from = at + 3;
        let end = code[from..].find(';').map_or(code.len(), |e| from + e);
        let item: String = code[from..end].split_whitespace().collect();
        let names_sleep = |tail: &str| find_word_from(tail, "sleep", 0).is_some();
        if let Some(i) = item.find("thread::") {
            let tail = &item[i + "thread::".len()..];
            if names_sleep(tail) && (tail.starts_with("sleep") || tail.starts_with('{')) {
                return true;
            }
        }
    }
    false
}

/// The sleeps of `code`, as offsets.
fn sleeps(code: &str) -> Vec<usize> {
    let b = code.as_bytes();
    let imported = imports_sleep(code);
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "sleep", from) {
        from = at + "sleep".len();
        if b.get(skip_ws(b, from)) != Some(&b'(') {
            continue;
        }
        let before = code[..at].trim_end();
        let hit = if before.ends_with("thread::") {
            true
        } else if before.ends_with("::")
            || before.ends_with('.')
            || before
                .strip_suffix("fn")
                .is_some_and(|p| !p.ends_with(word_char))
        {
            false
        } else {
            imported
        };
        if hit {
            out.push(at);
        }
    }
    out
}

/// The module directory of the file at `path` when no declaration loaded it: where the
/// files its `mod name;` declarations name sit. A crate root (`lib.rs`, `main.rs`,
/// `build.rs`, `src/bin/*.rs`, and a direct child of `tests/`, `examples/` or `benches/`
/// outside `src`) and a `mod.rs` keep them beside the file; any other file keeps them in
/// a directory named for it.
fn module_dir(path: &str) -> String {
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    let segs: Vec<&str> = dir.split('/').collect();
    let in_src = segs.contains(&"src");
    let root = match file.strip_suffix(".rs") {
        Some("lib" | "main" | "mod") | None => true,
        Some("build") => !in_src,
        Some(_) => {
            segs.ends_with(&["src", "bin"])
                || (!in_src && matches!(segs.last(), Some(&("tests" | "examples" | "benches"))))
        }
    };
    match file.strip_suffix(".rs") {
        Some(stem) if !root && dir.is_empty() => stem.to_owned(),
        Some(stem) if !root => format!("{dir}/{stem}"),
        _ => dir.to_owned(),
    }
}

/// `dir/rel` with `.` and `..` segments folded.
fn join(dir: &str, rel: &str) -> String {
    let mut segs: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    segs.join("/")
}

/// The `#[path = ".."]` literal among the attributes that precede the `mod` at `at`
/// (read from `text`; `code` is the same file blanked, at the same offsets).
fn path_attr(code: &str, text: &str, at: usize) -> Option<String> {
    let b = code.as_bytes();
    let mut end = code[..at].trim_end().len();
    let head = &code[..end];
    if head.ends_with(')') {
        let open = head.rfind('(')?;
        let before = head[..open].trim_end();
        if before.ends_with("pub") {
            end = before.len() - 3;
        }
    } else if head.ends_with("pub") {
        end -= 3;
    }
    let mut path = None;
    loop {
        end = code[..end].trim_end().len();
        if end == 0 || b[end - 1] != b']' {
            return path;
        }
        let mut depth = 0usize;
        let mut open = None;
        for i in (0..end).rev() {
            match b[i] {
                b']' => depth += 1,
                b'[' => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let open = open?;
        let hash = code[..open].trim_end();
        if !hash.ends_with('#') {
            return path;
        }
        let body = text[open + 1..end - 1].trim();
        if let Some(r) = body.strip_prefix("path")
            && let Some(r) = r.trim_start().strip_prefix('=')
            && let Some(r) = r.trim().strip_prefix('"')
            && let Some(lit) = r.strip_suffix('"')
        {
            path = Some(lit.to_owned());
        }
        end = hash.len() - 1;
    }
}

/// A file a `mod name;` declaration names, and the module directory of that file.
type Resolved = (String, String);

/// The files the `mod name;` declarations of the file at `path`, whose module directory
/// is `base`, name, as `(declaration offset, candidates)`.
fn declared(path: &str, base: &str, text: &str, code: &str) -> Vec<(usize, Vec<Resolved>)> {
    let b = code.as_bytes();
    let inline = inline_mods(code);
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "mod", from) {
        from = at + 3;
        let s = skip_ws(b, from);
        let name_len = b[s..].iter().take_while(|&&c| word(c)).count();
        if name_len == 0 || b.get(skip_ws(b, s + name_len)) != Some(&b';') {
            continue;
        }
        let name = &code[s..s + name_len];
        let mut nested: Vec<&(String, usize, usize)> = inline
            .iter()
            .filter(|(_, open, close)| *open < at && at < *close)
            .collect();
        nested.sort_by_key(|(_, open, _)| *open);
        let mut dir = base.to_owned();
        for (m, _, _) in &nested {
            dir = join(&dir, m);
        }
        let found = match path_attr(code, text, at) {
            Some(lit) => {
                let from_dir = if nested.is_empty() {
                    path.rsplit_once('/').map_or("", |(d, _)| d)
                } else {
                    &dir
                };
                let file = join(from_dir, &lit);
                let own = file.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
                vec![(file, own)]
            }
            None => {
                let own = join(&dir, name);
                vec![
                    (format!("{own}.rs"), own.clone()),
                    (format!("{own}/mod.rs"), own),
                ]
            }
        };
        out.push((at, found));
    }
    out
}

/// The files, among `files`, that are test code because a `cfg(test)` module
/// declaration reaches them, directly or through another file that is test code.
pub(super) fn cfg_test_files(
    files: &BTreeMap<String, String>,
) -> std::collections::BTreeSet<String> {
    // Every declaration, test or not, fixes the module directory of the file it loads;
    // a file no declaration loads is a root and keeps `module_dir`.
    let mut dirs: BTreeMap<&str, String> = BTreeMap::new();
    let mut changed = true;
    while changed {
        changed = false;
        for (path, text) in files {
            let base = dirs
                .get(path.as_str())
                .cloned()
                .unwrap_or_else(|| module_dir(path));
            for (_, candidates) in declared(path, &base, text, &blank(text)) {
                for (c, own) in candidates {
                    if let Some((p, _)) = files.get_key_value(&c)
                        && !dirs.contains_key(p.as_str())
                    {
                        dirs.insert(p, own);
                        changed = true;
                    }
                }
            }
        }
    }
    let mut found = std::collections::BTreeSet::new();
    let mut work: Vec<&String> = files.keys().collect();
    while let Some(path) = work.pop() {
        let text = &files[path];
        let code = blank(text);
        let base = dirs
            .get(path.as_str())
            .cloned()
            .unwrap_or_else(|| module_dir(path));
        let whole = is_test_file(path) || found.contains(path);
        let ranges = test_ranges(&code);
        for (at, candidates) in declared(path, &base, text, &code) {
            if !whole && !ranges.iter().any(|&(s, e)| at >= s && at <= e) {
                continue;
            }
            for (c, _) in candidates {
                if let Some((p, _)) = files.get_key_value(&c)
                    && found.insert(p.clone())
                {
                    work.push(p);
                }
            }
        }
    }
    found
}

fn scan(path: &str, text: &str, whole: bool) -> Sites {
    let code = blank(text);
    let ranges = if whole {
        vec![(0, code.len())]
    } else {
        test_ranges(&code)
    };
    let in_test = |at: usize| ranges.iter().any(|&(s, e)| at >= s && at <= e);
    let mut out = Sites::default();
    for (at, label) in wall_clock(&code) {
        if in_test(at) {
            out.compares.push(format!(
                "    {path}:{}: compares {label}",
                line_of(&code, at)
            ));
        }
    }
    for at in sleeps(&code) {
        if in_test(at) {
            out.sleeps
                .push(format!("    {path}:{}: sleeps", line_of(&code, at)));
        }
    }
    out
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let mut r = Report::default();
    let baseline = match tree.read(BASELINE).map(|t| parse(&t)) {
        Some(Ok(b)) => b,
        Some(Err(e)) => {
            r.fail(format!("{GATE}: FAIL: {e}"));
            return r;
        }
        None => {
            r.fail(format!("{GATE}: FAIL: {BASELINE} is missing"));
            return r;
        }
    };
    let mut paths: Vec<String> = ROOTS
        .iter()
        .flat_map(|d| tree.files(d, &skipped))
        .filter(|p| p.ends_with(".rs"))
        .collect();
    paths.sort();
    paths.dedup();
    let mut over = Vec::new();
    let mut current: BTreeMap<String, Counts> = BTreeMap::new();
    let mut slack = 0;
    let mut scanned = std::collections::BTreeSet::new();
    let files: BTreeMap<String, String> = paths
        .into_iter()
        .filter_map(|p| tree.read(&p).map(|t| (p, t)))
        .collect();
    let reached = cfg_test_files(&files);
    for (path, text) in &files {
        scanned.insert(path.clone());
        let s = scan(path, text, is_test_file(path) || reached.contains(path));
        let (n, allowed) = (s.counts(), baseline.get(path).copied().unwrap_or_default());
        if n != Counts::default() {
            current.insert(path.clone(), n);
        }
        for (count, base, noun, list) in [
            (
                n.compares,
                allowed.compares,
                "wall-clock comparisons",
                s.compares,
            ),
            (n.sleeps, allowed.sleeps, "sleeps", s.sleeps),
        ] {
            if count > base {
                over.push(format!("  {path}: {count} {noun}, baseline {base}"));
                over.extend(list);
            } else {
                slack += base - count;
            }
        }
    }
    let gone: Vec<&String> = baseline.keys().filter(|p| !scanned.contains(*p)).collect();
    for p in &gone {
        let c = baseline[*p];
        slack += c.compares + c.sleeps;
    }
    if over.is_empty() {
        r.say(format!("{GATE}: OK"));
    } else {
        r.fail(format!(
            "{GATE}: FAIL (a test asserts wall-clock time or sleeps; wait on the event, and let a time limit only guard a hang):"
        ));
        for line in over {
            r.fail(line);
        }
    }
    for p in &gone {
        r.say(format!(
            "{GATE}: {BASELINE} names {p}, which no longer exists; remove the row"
        ));
    }
    let total = |m: &BTreeMap<String, Counts>| {
        m.values().fold(Counts::default(), |a, c| Counts {
            compares: a.compares + c.compares,
            sleeps: a.sleeps + c.sleeps,
        })
    };
    let (have, base) = (total(&current), total(&baseline));
    r.say(format!(
        "{GATE}: {} comparisons and {} sleeps in {} files ({} and {} in {} files of {BASELINE})",
        have.compares,
        have.sleeps,
        current.len(),
        base.compares,
        base.sleeps,
        baseline.len(),
    ));
    if slack > 0 {
        r.say(format!(
            "{GATE}: {BASELINE} can drop by {slack}; lower it to the current counts"
        ));
    }
    if slack > 0 || !r.passed() {
        r.say(format!("{GATE}: {BASELINE} for the current counts:"));
        for (path, c) in &current {
            r.say(format!("{} {} {path}", c.compares, c.sleeps));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    fn run(files: &[(&str, &str)]) -> Report {
        check(&Mem::with(files))
    }

    fn sites(path: &str, text: &str) -> Sites {
        scan(path, text, is_test_file(path))
    }

    fn counts(path: &str, src: &str) -> Counts {
        sites(path, src).counts()
    }

    const LIB: &str = "crates/a/src/lib.rs";

    #[test]
    fn an_elapsed_comparison_in_a_test_counts_and_one_in_library_code_does_not() {
        let src = "fn f(t: Instant) -> bool { t.elapsed() < LIMIT }\n\
                   #[cfg(test)]\nmod tests {\n#[test]\nfn t() {\nlet t0 = Instant::now();\n\
                   assert!(t0.elapsed() < Duration::from_millis(5));\n}\n}\n";
        let s = sites(LIB, src);
        assert_eq!(
            s.counts(),
            Counts {
                compares: 1,
                sleeps: 0
            }
        );
        assert_eq!(
            s.compares,
            ["    crates/a/src/lib.rs:7: compares `.elapsed()`"]
        );
    }

    #[test]
    fn each_comparison_shape_counts_and_a_bare_read_does_not() {
        let hits = [
            "fn t() { if t.elapsed() >= x { } }",
            "fn t() { if x > t.elapsed() { } }",
            "fn t() { if t.elapsed().as_secs_f64() <= 1.0 { } }",
            "fn t() { if self.t.elapsed().as_millis() as u64 > 3 { } }",
            "fn t() { assert!(t.elapsed().is_zero()); }",
            "fn t() { assert_eq!(a.duration_since(b), d); }",
            "fn t() { if Instant::now() - start > d { } }",
            "fn t() { if Instant::now().duration_since(s) < d { } }",
        ];
        for h in hits {
            let src = format!("#[cfg(test)]\nmod t {{\n{h}\n}}\n");
            assert_eq!(counts(LIB, &src).compares, 1, "{h}");
        }
        let misses = [
            "fn t() { let e = t.elapsed(); log(e); }",
            "fn t() { let n = |x| x -> y; t.elapsed(); }",
            "fn t() { if t.elapsed() == d { } }",
            "fn t() { v.push(t.elapsed()); }",
            "fn t() { let x = Instant::now() - d; }",
        ];
        for m in misses {
            let src = format!("#[cfg(test)]\nmod t {{\n{m}\n}}\n");
            assert_eq!(counts(LIB, &src).compares, 0, "{m}");
        }
    }

    #[test]
    fn a_sleep_in_a_tests_rs_counts_and_a_sleep_in_library_code_does_not() {
        let src = "fn t() { std::thread::sleep(D); thread::sleep(D); }\n";
        assert_eq!(counts("crates/a/src/x/tests.rs", src).sleeps, 2);
        assert_eq!(counts(LIB, src).sleeps, 0);
        assert_eq!(counts("crates/a/tests/it/main.rs", src).sleeps, 2);
    }

    #[test]
    fn a_bare_sleep_counts_only_when_it_is_imported_from_thread() {
        let imported =
            "use std::thread::sleep;\nfn t() { sleep(D); clock.sleep(D); rt::sleep(D); }\n";
        assert_eq!(counts("crates/a/tests/t.rs", imported).sleeps, 1);
        let grouped = "use std::thread::{self, sleep};\nfn t() { sleep(D); }\n";
        assert_eq!(counts("crates/a/tests/t.rs", grouped).sleeps, 1);
        let own = "fn sleep(d: D) {}\nfn t() { sleep(D); }\n";
        assert_eq!(counts("crates/a/tests/t.rs", own).sleeps, 0);
    }

    #[test]
    fn a_hang_guard_deadline_is_not_counted() {
        let src = "fn t() {\nlet deadline = Instant::now() + Duration::from_secs(10);\n\
                   while Instant::now() < deadline { poll(); }\n\
                   let start = Instant::now();\n\
                   while start.elapsed() < Duration::from_secs(10) { poll(); }\n\
                   while !done() && Instant::now() - start < LIMIT { poll(); }\n}\n";
        assert_eq!(counts("crates/a/tests/t.rs", src), Counts::default());
        let asserted = "fn t() { let s = Instant::now(); assert!(s.elapsed() < D); }\n";
        assert_eq!(counts("crates/a/tests/t.rs", asserted).compares, 1);
    }

    #[test]
    fn comments_and_literals_are_not_code() {
        let src = "// t.elapsed() < d\nfn t() { log(\"thread::sleep(D)\"); }\n";
        assert_eq!(counts("crates/a/tests/t.rs", src), Counts::default());
    }

    #[test]
    fn a_file_over_its_baseline_fails_with_each_site() {
        let r = run(&[
            (BASELINE, "1 1 crates/a/tests/t.rs\n"),
            (
                "crates/a/tests/t.rs",
                "fn a() { assert!(t.elapsed() < D); }\nfn b() { if u.elapsed() > D {} }\n\
                 fn c() { thread::sleep(D); }\n",
            ),
        ]);
        assert_eq!(
            r.findings,
            [
                "check-test-time: FAIL (a test asserts wall-clock time or sleeps; wait on the event, and let a time limit only guard a hang):",
                "  crates/a/tests/t.rs: 2 wall-clock comparisons, baseline 1",
                "    crates/a/tests/t.rs:1: compares `.elapsed()`",
                "    crates/a/tests/t.rs:2: compares `.elapsed()`",
            ]
        );
        assert!(r.out.contains(&"2 1 crates/a/tests/t.rs".to_owned()));
    }

    #[test]
    fn a_sleep_over_its_baseline_fails_naming_the_kind() {
        let r = run(&[
            (BASELINE, "0 1 crates/a/tests/t.rs\n"),
            (
                "crates/a/tests/t.rs",
                "fn a() { thread::sleep(D); thread::sleep(D); }\n",
            ),
        ]);
        assert_eq!(r.findings[1], "  crates/a/tests/t.rs: 2 sleeps, baseline 1");
        assert_eq!(r.findings[2], "    crates/a/tests/t.rs:1: sleeps");
    }

    #[test]
    fn a_file_under_its_baseline_passes_and_reports_the_slack() {
        let r = run(&[
            (BASELINE, "3 2 crates/a/tests/t.rs\n"),
            (
                "crates/a/tests/t.rs",
                "fn a() { assert!(t.elapsed() < D); }\n",
            ),
        ]);
        assert!(r.passed());
        assert_eq!(
            r.out,
            [
                "check-test-time: OK",
                "check-test-time: 1 comparisons and 0 sleeps in 1 files (3 and 2 in 1 files of xtask/test-time.txt)",
                "check-test-time: xtask/test-time.txt can drop by 4; lower it to the current counts",
                "check-test-time: xtask/test-time.txt for the current counts:",
                "1 0 crates/a/tests/t.rs",
            ]
        );
    }

    #[test]
    fn an_unlisted_file_with_a_count_fails() {
        let r = run(&[
            (BASELINE, ""),
            ("crates/a/tests/t.rs", "fn a() { thread::sleep(D); }\n"),
        ]);
        assert_eq!(r.findings[1], "  crates/a/tests/t.rs: 1 sleeps, baseline 0");
    }

    #[test]
    fn a_missing_or_malformed_baseline_fails() {
        assert!(!run(&[]).passed());
        assert!(!run(&[(BASELINE, "many 0 crates/a/tests/t.rs\n")]).passed());
        assert!(!run(&[(BASELINE, "1 crates/a/tests/t.rs\n")]).passed());
    }

    #[test]
    fn a_zero_site_file_with_a_row_adds_its_slack_once() {
        let r = run(&[
            (BASELINE, "3 2 crates/a/tests/t.rs\n"),
            ("crates/a/tests/t.rs", "fn a() {}\n"),
        ]);
        assert!(r.passed());
        assert!(
            r.out
                .contains(&"check-test-time: xtask/test-time.txt can drop by 5; lower it to the current counts".to_owned()),
            "{:?}",
            r.out
        );
    }

    #[test]
    fn a_deleted_file_adds_its_slack_once_and_is_named() {
        let r = run(&[
            (BASELINE, "3 2 crates/a/tests/gone.rs\n"),
            (LIB, "fn f() {}\n"),
        ]);
        assert!(r.passed());
        assert!(r.out.contains(
            &"check-test-time: xtask/test-time.txt can drop by 5; lower it to the current counts".to_owned()
        ));
        assert!(r.out.contains(
            &"check-test-time: xtask/test-time.txt names crates/a/tests/gone.rs, which no longer exists; remove the row".to_owned()
        ));
    }

    #[test]
    fn a_message_argument_of_an_assert_is_not_an_operand() {
        let t = "crates/a/tests/t.rs";
        assert_eq!(
            counts(t, "fn a() { assert!(ok, \"took {:?}\", t.elapsed()); }").compares,
            0
        );
        assert_eq!(
            counts(t, "fn a() { assert_eq!(a, t.elapsed().as_secs(), \"m\"); }").compares,
            1
        );
        assert_eq!(
            counts(
                t,
                "fn a() { assert_eq!(a, b, \"{:?}\", t.elapsed().as_secs()); }"
            )
            .compares,
            0
        );
        assert_eq!(
            counts(t, "fn a() { assert!(t.elapsed().is_zero(), \"m\"); }").compares,
            1
        );
    }

    #[test]
    fn a_name_ending_in_fn_is_not_a_sleep_definition() {
        let src = "use std::thread::sleep;\nfn t() { run_fn sleep(D); }\nfn sleep(d: D) {}\n";
        assert_eq!(counts("crates/a/tests/t.rs", src).sleeps, 1);
        let other = "use std::thread::sleeper;\nfn t() { sleep(D); }\n";
        assert_eq!(counts("crates/a/tests/t.rs", other).sleeps, 0);
    }

    #[test]
    fn a_clean_tree_passes() {
        let r = run(&[(BASELINE, ""), (LIB, "fn f() {}\n")]);
        assert!(r.passed());
        assert_eq!(r.out[0], "check-test-time: OK");
    }

    const SLEEP: &str = "fn t() { thread::sleep(D); }\n";

    /// The failure lines a tree with an empty baseline gives.
    fn failing(files: &[(&str, &str)]) -> Vec<String> {
        let mut all = vec![(BASELINE, "")];
        all.extend_from_slice(files);
        run(&all).findings
    }

    #[test]
    fn a_file_declared_by_a_cfg_test_mod_is_test_code() {
        let f = failing(&[
            (LIB, "#[cfg(test)]\nmod x_tests;\n"),
            ("crates/a/src/x_tests.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/x_tests.rs: 1 sleeps, baseline 0".to_owned()));
        let f = failing(&[
            (LIB, "#[cfg(all(test, unix))]\npub(crate) mod x_tests;\n"),
            ("crates/a/src/x_tests/mod.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/x_tests/mod.rs: 1 sleeps, baseline 0".to_owned()));
    }

    #[test]
    fn a_file_declared_without_cfg_test_is_library_code() {
        let f = failing(&[
            (LIB, "mod x_tests;\n#[cfg(unix)]\nmod y;\n"),
            ("crates/a/src/x_tests.rs", SLEEP),
            ("crates/a/src/y.rs", SLEEP),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn modules_a_cfg_test_file_declares_are_test_code_transitively() {
        let f = failing(&[
            (LIB, "#[cfg(test)]\nmod a;\n"),
            ("crates/a/src/a.rs", "mod b;\n"),
            ("crates/a/src/a/b.rs", "mod c;\n"),
            ("crates/a/src/a/b/c.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/a/b/c.rs: 1 sleeps, baseline 0".to_owned()));
    }

    #[test]
    fn a_mod_rs_file_resolves_its_modules_beside_it() {
        let f = failing(&[
            (LIB, "#[cfg(test)]\nmod a;\n"),
            ("crates/a/src/a/mod.rs", "mod b;\n"),
            ("crates/a/src/a/b.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/a/b.rs: 1 sleeps, baseline 0".to_owned()));
    }

    #[test]
    fn a_mod_declared_inside_an_inline_cfg_test_mod_is_test_code() {
        let f = failing(&[
            (LIB, "#[cfg(test)]\nmod t {\n    mod h;\n}\n"),
            ("crates/a/src/t/h.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/t/h.rs: 1 sleeps, baseline 0".to_owned()));
    }

    #[test]
    fn a_path_attribute_names_the_file_relative_to_the_declaring_directory() {
        let f = failing(&[
            (
                "crates/a/src/lib.rs",
                "#[cfg(test)]\n#[path = \"../support/m.rs\"]\nmod x;\n",
            ),
            ("crates/a/support/m.rs", SLEEP),
            ("crates/a/src/x.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/support/m.rs: 1 sleeps, baseline 0".to_owned()));
        assert!(!f.iter().any(|l| l.contains("src/x.rs")), "{f:?}");
        let f = failing(&[
            (
                "crates/a/src/lib.rs",
                "#[path = \"m.rs\"]\n#[cfg(test)]\nmod x;\n",
            ),
            ("crates/a/src/m.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/m.rs: 1 sleeps, baseline 0".to_owned()));
    }

    #[test]
    fn a_cfg_test_file_inside_a_non_mod_rs_file_resolves_in_its_module_directory() {
        let f = failing(&[
            (LIB, "mod a;\n"),
            ("crates/a/src/a.rs", "#[cfg(test)]\nmod b;\n"),
            ("crates/a/src/a/b.rs", SLEEP),
            ("crates/a/src/b.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/a/b.rs: 1 sleeps, baseline 0".to_owned()));
        assert!(!f.iter().any(|l| l.contains("src/b.rs")), "{f:?}");
    }

    #[test]
    fn a_mod_behind_cfg_not_test_or_cfg_any_test_is_library_code() {
        let f = failing(&[
            (
                LIB,
                "#[cfg(not(test))]\nmod a;\n#[cfg(any(test, feature = \"x\"))]\nmod b;\n",
            ),
            ("crates/a/src/a.rs", SLEEP),
            ("crates/a/src/b.rs", SLEEP),
        ]);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn a_path_attribute_inside_an_inline_cfg_test_mod_is_resolved_in_its_directory() {
        let f = failing(&[
            (
                LIB,
                "#[cfg(test)]\nmod t {\n    #[path = \"x.rs\"]\n    mod y;\n}\n",
            ),
            ("crates/a/src/t/x.rs", SLEEP),
            ("crates/a/src/x.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/t/x.rs: 1 sleeps, baseline 0".to_owned()));
        assert!(!f.iter().any(|l| l.contains("src/x.rs")), "{f:?}");
    }

    #[test]
    fn a_child_of_a_path_loaded_file_sits_beside_it() {
        let f = failing(&[
            (LIB, "#[cfg(test)]\n#[path = \"sup/m.rs\"]\nmod m;\n"),
            ("crates/a/src/sup/m.rs", "mod k;\n"),
            ("crates/a/src/sup/k.rs", SLEEP),
            ("crates/a/src/sup/m/k.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/sup/k.rs: 1 sleeps, baseline 0".to_owned()));
        assert!(!f.iter().any(|l| l.contains("sup/m/k.rs")), "{f:?}");
    }

    #[test]
    fn a_child_of_a_non_lib_crate_root_sits_beside_it() {
        for root in [
            "crates/a/src/bin/foo.rs",
            "crates/a/build.rs",
            "crates/a/examples/e.rs",
            "crates/a/benches/b.rs",
        ] {
            let beside = join(root.rsplit_once('/').unwrap().0, "k.rs");
            let under = join(root.strip_suffix(".rs").unwrap(), "k.rs");
            let f = failing(&[
                (root, "#[cfg(test)]\nmod k;\n"),
                (&beside, SLEEP),
                (&under, SLEEP),
            ]);
            assert!(
                f.contains(&format!("  {beside}: 1 sleeps, baseline 0")),
                "{root}"
            );
            assert!(!f.iter().any(|l| l.contains(&under)), "{root}: {f:?}");
        }
    }

    #[test]
    fn a_module_file_under_src_keeps_its_children_in_its_own_directory() {
        let f = failing(&[
            (LIB, "mod build;\n"),
            ("crates/a/src/build.rs", "#[cfg(test)]\nmod k;\n"),
            ("crates/a/src/build/k.rs", SLEEP),
            ("crates/a/src/k.rs", SLEEP),
        ]);
        assert!(f.contains(&"  crates/a/src/build/k.rs: 1 sleeps, baseline 0".to_owned()));
        assert!(!f.iter().any(|l| l.contains("src/k.rs")), "{f:?}");
    }
}
