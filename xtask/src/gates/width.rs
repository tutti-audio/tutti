//! `check-width`: a quantity has one width, data is generic over the graph's float, and
//! every crossing between them is a named place. What is not yet so is a row of
//! [`BASELINE`].
//!
//! **Library code** is every `crates/*/src/**/*.rs` file that is not under a `tests`,
//! `benches` or `examples` directory, not a file named `tests.rs` or ending `_tests.rs` and not a `contract`,
//! `testing` or `test_support` module (`contract.rs`, `testing.rs`, `test_support.rs` or a
//! directory of that name), and not a file its parent declares as a module behind a
//! `#[cfg(test)]` (`#[cfg(test)] mod doubles;` makes `doubles.rs`, `doubles/mod.rs`, a
//! `#[path]` target and everything under `doubles/` test code). Its text
//! is read with comments and literals blanked (so `//`, `///` and `//!` lines and doc
//! examples are not code) with every test-only item blanked: the item behind a
//! `#[cfg(test)]` (or `#[cfg(all(.., test, ..))]`), up to its `;` for an out-of-line `mod
//! tests;` or to its matching `}` for an inline one. Code after that item is library code.
//!
//! **What it refuses** in library code, each hit one site:
//!
//! - `from_f32`: a `::from_f32(` call on any path (`F::`, `S::`, `Self::`, `Seconds::`);
//! - `from_f64`: a `::from_f64(` call on any path;
//! - `to_f32`: a `.to_f32()` call;
//! - `to_f64`: a `.to_f64()` call;
//! - `as_f32`: `as f32`. Text cannot tell a float cast from an integer one, so the cast is
//!   refused outright; an integer-to-float conversion goes through a named helper;
//! - `lit`: a `::lit(` or `.lit(` call whose argument is not a numeric literal.
//!   `F::lit(0.5)` is allowed; `f64::from` is lossless and is never refused.
//!   `F::from_unit(q)` and `F::from_design(x)` are the named crossings and are never
//!   refused; review holds `from_design` to a kernel's design function;
//! - `plain_node`: an `impl Node for X` or `impl Node<f32> for X` with no `F`, an f32-only
//!   node (event-only nodes included);
//! - `fn_twin`: a `pub fn` named `*_f32` or `*_f64`;
//! - `f32_cell`: a tuple struct whose only field is an `f32` (a unit), or an atomic `f32`
//!   (a param cell);
//! - `bare_f32`: a `pub` item (`fn`, `type`, a `pub` field of a `pub` struct, a variant of
//!   a `pub` enum, a method of a `pub` trait) whose signature or field type names `f32`,
//!   which covers `&[f32]`. It is counted once per item, as `check-buffers` reads items.
//!
//! **Allowed sites** are the crossings, not exceptions ([`EDGES`], by file): the cast
//! forms (`from_f32`, `from_f64`, `to_f32`, `to_f64`, `as_f32`, `lit`), `fn_twin` and
//! `bare_f32` are not refused in an edge file. The `*-ffi` crates are edges by their name. An edge file's
//! `plain_node` and `f32_cell` sites are still refused.
//!
//! An [`EDGES`] path (a file, or a directory ending in `/`) that matches no library file
//! fails like a stale baseline row, so the list names only files that exist.
//!
//! **Baseline.** [`BASELINE`] holds `<path>  <max>  <reason>` rows, one per file, `#`
//! lines and blank lines aside. The reason is `migrating: <PR or step>`, debt a lane
//! removes, or `spec: <the external fact>`, a site an outside specification fixes (a
//! plugin format's audio width): kept, still counted and still ratcheted. A file fails when it holds more sites than its
//! max (a file with no row has max 0), naming each site. A row whose file holds no site
//! fails (delete it), and so does a row for an unknown reason, a malformed row and a file
//! listed twice. A file below its max passes and the report says how far it can drop.
//! The baseline ends with `spec:` rows only.
//!
//! **Blind spots.** The scan is lexical: it cannot see a cast hidden in a macro body that
//! the macro's caller writes, a type alias that hides `f32`, a `lit` argument that is a
//! named constant, or a coefficient computed in `F` after `F::from_unit` narrowed the
//! unit (review holds that one).

use super::buffers::{scan_with, squash};
use super::lex::{blank, find_word_from, line_of, matching, test_ranges};
use super::{Report, Tree};
use std::collections::BTreeMap;

/// The baseline: `<path>  <max>  migrating: <PR or step>` or `<path>  <max>  spec: <fact>`
/// per file.
pub(crate) const BASELINE: &str = "xtask/width-baseline.txt";

const GATE: &str = "check-width";

/// The fewest library files a real run reads; fewer means the walk is broken.
pub(super) const MIN_FILES: usize = 500;

/// The crossings that are files: a path is one file, or a directory when it ends in `/`.
/// Each is a place the width rule defines (the design's crossings 1, 3 and 4).
pub(super) const EDGES: &[(&str, &str)] = &[
    (
        "crates/tutti-units/src/real.rs",
        "`Real::from_unit`, `Real::from_design` and `Real::lit`: the quantity and design-figure crossings into data",
    ),
    (
        "crates/tutti-buffers/src/sample.rs",
        "the `Sample` and `Float` impls",
    ),
    (
        "crates/tutti-graph/src/lane.rs",
        "the lane renderer: a control ramp or segment rendered into an `F` lane",
    ),
    (
        "crates/tutti-graph/src/meter.rs",
        "the meter folds: a per-block statistic crossing into a reading",
    ),
    (
        "crates/tutti-dsp/src/spatial/hrtf/sphere/words.rs",
        "the HRIR byte format's f32 words",
    ),
    (
        "crates/tutti-codec/src/",
        "an external edge: file sample words",
    ),
    (
        "crates/tutti-export/src/output.rs",
        "an external edge: the file writer's sample words",
    ),
    (
        "crates/tutti-cpal/src/edge.rs",
        "an external edge: the device's sample format",
    ),
    (
        "crates/tutti-plugin-types/src/param_unit.rs",
        "an external edge: a plugin format's param word",
    ),
    (
        "crates/tutti-clap-host/src/instance/lifecycle.rs",
        "an external edge: the CLAP ABI width",
    ),
    (
        "crates/tutti-clap-host/src/instance/processor.rs",
        "an external edge: the CLAP ABI width",
    ),
    (
        "crates/tutti-vst3-host/src/events/midi.rs",
        "an external edge: VST3 event words",
    ),
    (
        "crates/tutti-vst3-host/src/host/processor.rs",
        "an external edge: the VST3 ABI width",
    ),
    (
        "crates/tutti-vst3-host/src/host/loaded/editor.rs",
        "an external edge: a VST3 view's float words",
    ),
    (
        "crates/tutti-vst2-host/src/controller.rs",
        "an external edge: VST2 f32 params",
    ),
    (
        "crates/tutti-vst2-host/src/error.rs",
        "an external edge: VST2 f32 params",
    ),
    (
        "crates/tutti-vst2-host/src/host.rs",
        "an external edge: VST2 f32 params",
    ),
    (
        "crates/tutti-vst2-host/src/param_properties.rs",
        "an external edge: VST2 f32 params",
    ),
    (
        "crates/tutti-vst2-host/src/parameters.rs",
        "an external edge: VST2 f32 params",
    ),
    (
        "crates/tutti-vst2-host/src/processor.rs",
        "an external edge: the VST2 ABI width",
    ),
    (
        "crates/tutti-au-host/src/macos/automation.rs",
        "an external edge: AU f32 params",
    ),
    (
        "crates/tutti-au-host/src/macos/control.rs",
        "an external edge: AU f32 params",
    ),
    (
        "crates/tutti-au-host/src/macos/instance.rs",
        "an external edge: AU f32 params",
    ),
    (
        "crates/tutti-au-host/src/macos/midi_map.rs",
        "an external edge: AU f32 params",
    ),
    (
        "crates/tutti-au-host/src/macos/parameters.rs",
        "an external edge: AU f32 params",
    ),
    (
        "crates/tutti-au-host/src/macos/processor.rs",
        "an external edge: the AU ABI width",
    ),
    (
        "crates/tutti-au-host/src/macos/ranges.rs",
        "an external edge: AU f32 params",
    ),
];

/// Why `path` is an edge file, or `None`: a listed file, or a file of a `*-ffi` crate.
pub(super) fn edge(path: &str, edges: &[(&'static str, &'static str)]) -> Option<&'static str> {
    if path
        .strip_prefix("crates/")
        .and_then(|r| r.split('/').next())
        .is_some_and(|c| c.ends_with("-ffi"))
    {
        return Some("an external edge: a C ABI, IPC or wire word");
    }
    edges
        .iter()
        .find(|(p, _)| {
            if p.ends_with('/') {
                path.starts_with(p)
            } else {
                path == *p
            }
        })
        .map(|(_, why)| *why)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Form {
    FromF32,
    FromF64,
    ToF32,
    ToF64,
    AsF32,
    BadLit,
    PlainNode,
    FnTwin,
    F32Cell,
    BareF32,
}

impl Form {
    const ALL: [Form; 10] = [
        Form::FromF32,
        Form::FromF64,
        Form::ToF32,
        Form::ToF64,
        Form::AsF32,
        Form::BadLit,
        Form::PlainNode,
        Form::FnTwin,
        Form::F32Cell,
        Form::BareF32,
    ];

    fn name(self) -> &'static str {
        match self {
            Form::FromF32 => "from_f32",
            Form::FromF64 => "from_f64",
            Form::ToF32 => "to_f32",
            Form::ToF64 => "to_f64",
            Form::AsF32 => "as_f32",
            Form::BadLit => "lit",
            Form::PlainNode => "plain_node",
            Form::FnTwin => "fn_twin",
            Form::F32Cell => "f32_cell",
            Form::BareF32 => "bare_f32",
        }
    }

    /// Whether an edge file may hold it: a crossing's own spelling, including a format's
    /// `process_f32`-style twin.
    fn edge_allowed(self) -> bool {
        !matches!(self, Form::PlainNode | Form::F32Cell)
    }
}

/// One refused site: its form and where.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    form: Form,
    at: String,
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "tests" | "benches" | "examples")
}

/// The library files of `crates/*/src`.
fn library_files(tree: &dyn Tree) -> Vec<String> {
    let files: Vec<String> = tree
        .files("crates", &skipped)
        .into_iter()
        .filter(|p| {
            let parts: Vec<&str> = p.split('/').collect();
            p.ends_with(".rs")
                && parts.get(2) == Some(&"src")
                && !parts
                    .last()
                    .is_some_and(|f| *f == "tests.rs" || f.ends_with("_tests.rs"))
                && !parts.iter().any(|s| {
                    matches!(
                        *s,
                        "contract"
                            | "contract.rs"
                            | "testing"
                            | "testing.rs"
                            | "test_support"
                            | "test_support.rs"
                    )
                })
        })
        .collect();
    let test_only = test_modules(tree, &files);
    files
        .into_iter()
        .filter(|f| {
            !test_only
                .iter()
                .any(|t| f == t || t.strip_suffix(".rs").is_some_and(|d| under(f, d)))
        })
        .collect()
}

/// Whether `file` sits in directory `dir`, at any depth.
fn under(file: &str, dir: &str) -> bool {
    file.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The files of `files` that a parent declares as an out-of-line module behind a
/// `#[cfg(test)]` (`#[cfg(test)] mod doubles;`): `doubles.rs`, `doubles/mod.rs`, or
/// the `#[path]` the declaration names. Such a file is all test code; a file under a
/// test module's directory is too (the caller skips `doubles/` for `doubles.rs`).
fn test_modules(tree: &dyn Tree, files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for parent in files {
        let Some(src) = tree.read(parent) else {
            continue;
        };
        let blanked = blank(&src);
        let (dir, file) = parent.rsplit_once('/').unwrap_or(("", parent));
        let module_dir = match file {
            "lib.rs" | "main.rs" | "mod.rs" => dir.to_owned(),
            f => format!("{dir}/{}", f.trim_end_matches(".rs")),
        };
        for (from, to) in test_ranges(&blanked) {
            let item = &blanked[from..to.min(blanked.len())];
            let Some(name) = out_of_line_mod(item) else {
                continue;
            };
            let raw = &src[from..to.min(src.len())];
            let candidates = match path_attr(raw) {
                Some(path) => vec![format!("{dir}/{path}")],
                None => vec![
                    format!("{module_dir}/{name}.rs"),
                    format!("{module_dir}/{name}/mod.rs"),
                ],
            };
            out.extend(candidates.into_iter().filter(|c| files.contains(c)));
        }
    }
    out
}

/// The name of an out-of-line module declaration (`mod x;`, `pub(crate) mod x;`) that
/// `item` ends with, or `None` for any other item.
fn out_of_line_mod(item: &str) -> Option<&str> {
    let body = item.trim_end().strip_suffix(';')?;
    let at = find_word_from(body, "mod", 0)?;
    let name = body[at + 3..].trim();
    (!name.is_empty() && name.bytes().all(word)).then_some(name)
}

/// The path a `#[path = "x"]` attribute in `raw` names, read from the unblanked text.
fn path_attr(raw: &str) -> Option<&str> {
    let at = raw.find("#[path")?;
    let rest = &raw[at..];
    let open = rest.find('"')? + 1;
    let close = rest[open..].find('"')? + open;
    Some(&rest[open..close])
}

/// The offsets of `needle` (a path or method spelling) followed by optional whitespace and `(`.
fn calls(code: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = code[from..].find(needle) {
        let at = from + p;
        from = at + needle.len();
        if code[from..].trim_start().starts_with('(') {
            out.push(at);
        }
    }
    out
}

/// Whether `arg` is a numeric literal: `0.5`, `-2.0`, `1e-9`, `3_f64`.
fn numeric_literal(arg: &str) -> bool {
    let a = arg.trim();
    let a = a.strip_prefix('-').unwrap_or(a).trim_start();
    a.starts_with(|c: char| c.is_ascii_digit())
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// The offsets of every `as f32` cast.
fn as_f32(code: &str) -> Vec<usize> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "f32", from) {
        from = at + 3;
        let before = code[..at].trim_end();
        if before.ends_with("as")
            && before.len().checked_sub(3).is_none_or(|i| !word(b[i]))
            && at > before.len()
        {
            out.push(at);
        }
    }
    out
}

/// The `impl Node` blocks with no `F`: `impl Node for X`, `impl Node<f32> for X`.
fn plain_nodes(code: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "impl", from) {
        from = at + 4;
        let mut rest_at = from;
        let trimmed = code[rest_at..].trim_start();
        rest_at = code.len() - trimmed.len();
        if trimmed.starts_with('<')
            && let Some(close) = matching(code, rest_at, b'<', b'>')
        {
            rest_at = close + 1;
        }
        let end = code[rest_at..]
            .find(['{', ';', ')'])
            .map_or(code.len(), |i| rest_at + i);
        let header = &code[rest_at..end];
        let Some(for_at) = find_word_from(header, "for", 0) else {
            continue;
        };
        let tr = squash(header[..for_at].trim());
        let (path, args) = match tr.split_once('<') {
            Some((p, a)) => (p, Some(a.trim_end_matches('>').trim())),
            None => (tr.as_str(), None),
        };
        let last = path.trim().rsplit("::").next().unwrap_or("");
        if last == "Node" && matches!(args, None | Some("f32")) {
            out.push(at);
        }
    }
    out
}

/// The offsets of `pub fn name_f32` and `pub fn name_f64`.
fn fn_twins(code: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "fn", from) {
        from = at + 2;
        let rest = code[from..].trim_start();
        let name: String = rest.chars().take_while(|&c| word(c as u8)).collect();
        if !(name.ends_with("_f32") || name.ends_with("_f64")) {
            continue;
        }
        let line_start = code[..at].rfind('\n').map_or(0, |i| i + 1);
        let head = &code[line_start..at];
        let public = find_word_from(head, "pub", 0)
            .is_some_and(|p| !head[p + 3..].trim_start().starts_with('('));
        if public {
            out.push(at);
        }
    }
    out
}

/// The offsets of an `f32` unit or cell: `struct X(f32)`, an atomic `f32`.
fn f32_cells(code: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "AtomicF32", from) {
        from = at + 9;
        out.push(at);
    }
    let mut from = 0;
    while let Some(at) = find_word_from(code, "struct", from) {
        from = at + 6;
        let rest = code[from..].trim_start();
        let name_len = rest.chars().take_while(|&c| word(c as u8)).count();
        let after = rest[name_len..].trim_start();
        if name_len == 0 || !after.starts_with('(') {
            continue;
        }
        let open = code.len() - after.len();
        let Some(close) = matching(code, open, b'(', b')') else {
            continue;
        };
        let field = squash(code[open + 1..close].trim());
        let field = field
            .strip_prefix("pub")
            .map_or(field.as_str(), str::trim_start);
        if field == "f32" {
            out.push(at);
        }
    }
    out
}

/// Every refused site of one library file; `src` is the file's text.
fn scan(path: &str, src: &str) -> Vec<Hit> {
    let blanked = blank(src);
    let mut bytes = blanked.clone().into_bytes();
    for (from, to) in test_ranges(&blanked) {
        for b in &mut bytes[from..to.min(blanked.len())] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }
    let code = String::from_utf8(bytes).unwrap_or(blanked);
    let code = code.as_str();
    let mut hits = Vec::new();
    let mut add = |form: Form, at: usize| {
        hits.push(Hit {
            form,
            at: format!(
                "line {}",
                line_of(code, at.min(code.len().saturating_sub(1)))
            ),
        });
    };
    for at in calls(code, "::from_f32") {
        add(Form::FromF32, at);
    }
    for at in calls(code, "::from_f64") {
        add(Form::FromF64, at);
    }
    for at in calls(code, ".to_f32") {
        add(Form::ToF32, at);
    }
    for at in calls(code, ".to_f64") {
        add(Form::ToF64, at);
    }
    for at in as_f32(code) {
        add(Form::AsF32, at);
    }
    for needle in ["::lit", ".lit"] {
        for at in calls(code, needle) {
            let open = code[at..].find('(').map_or(at, |i| at + i);
            let arg = matching(code, open, b'(', b')').map_or("", |c| &code[open + 1..c]);
            if !numeric_literal(arg) {
                add(Form::BadLit, at);
            }
        }
    }
    for at in plain_nodes(code) {
        add(Form::PlainNode, at);
    }
    for at in fn_twins(code) {
        add(Form::FnTwin, at);
    }
    for at in f32_cells(code) {
        add(Form::F32Cell, at);
    }
    let named = |text: &str, _generic: bool| {
        find_word_from(&squash(text), "f32", 0).map(|_| "f32".to_owned())
    };
    for s in scan_with(path, src, &named) {
        hits.push(Hit {
            form: Form::BareF32,
            at: s.key,
        });
    }
    hits
}

/// A baseline row: its max and reason.
struct Row {
    max: usize,
    line: usize,
}

/// The baseline's rows, or what is malformed in them.
/// The reason kinds a row may give, each followed by its text.
const REASONS: [&str; 2] = ["migrating:", "spec:"];

fn parse(text: &str) -> Result<BTreeMap<String, Row>, Vec<String>> {
    let mut rows = BTreeMap::new();
    let mut errors = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = n + 1;
        let (path, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let (max, reason) = rest
            .trim_start()
            .split_once(char::is_whitespace)
            .unwrap_or((rest, ""));
        let (max, why) = (
            max.parse::<usize>().ok(),
            REASONS
                .iter()
                .find_map(|k| reason.trim().strip_prefix(k))
                .map(str::trim),
        );
        match (max, why) {
            (Some(max), Some(w)) if !w.is_empty() && max > 0 => {
                if rows.insert(path.to_owned(), Row { max, line: at }).is_some() {
                    errors.push(format!("  {BASELINE}:{at}: `{path}` is listed twice"));
                }
            }
            _ => errors.push(format!(
                "  {BASELINE}:{at}: `{line}` is not `<path>  <max>  migrating: <PR or step>` or `<path>  <max>  spec: <fact>` with a max above 0"
            )),
        }
    }
    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

pub(super) fn check(
    tree: &dyn Tree,
    edges: &[(&'static str, &'static str)],
    min_files: usize,
) -> Report {
    let mut r = Report::default();
    let Some(text) = tree.read(BASELINE) else {
        r.fail(format!("{GATE}: FAIL: {BASELINE} is missing"));
        return r;
    };
    let rows = match parse(&text) {
        Ok(rows) => rows,
        Err(errors) => {
            r.fail(format!("{GATE}: FAIL ({BASELINE} is malformed):"));
            errors.into_iter().for_each(|e| r.fail(e));
            return r;
        }
    };
    let files = library_files(tree);
    if files.len() < min_files {
        r.fail(format!(
            "{GATE}: FAIL: {} library files read, at least {min_files} expected; the walk is broken",
            files.len()
        ));
        return r;
    }
    let mut over = Vec::new();
    let mut stale = Vec::new();
    let missing: Vec<&str> = edges
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| {
            !files.iter().any(|f| {
                if p.ends_with('/') {
                    f.starts_with(p)
                } else {
                    f == p
                }
            })
        })
        .collect();
    let mut slack = Vec::new();
    let mut by_form: BTreeMap<Form, usize> = BTreeMap::new();
    let mut total = 0;
    let mut matched = std::collections::BTreeSet::new();
    for path in files {
        let Some(src) = tree.read(&path) else {
            continue;
        };
        let is_edge = edge(&path, edges).is_some();
        let hits: Vec<Hit> = scan(&path, &src)
            .into_iter()
            .filter(|h| !(is_edge && h.form.edge_allowed()))
            .collect();
        if hits.is_empty() {
            continue;
        }
        total += hits.len();
        for h in &hits {
            *by_form.entry(h.form).or_default() += 1;
        }
        matched.insert(path.clone());
        let max = rows.get(&path).map_or(0, |row| row.max);
        if hits.len() > max {
            over.push(format!("  {path}: {} sites, max {max}", hits.len()));
            over.extend(
                hits.iter()
                    .map(|h| format!("    {}: {}", h.form.name(), h.at)),
            );
        } else if hits.len() < max {
            slack.push(format!("{path} {max} -> {}", hits.len()));
        }
    }
    for (path, row) in &rows {
        if !matched.contains(path) {
            stale.push(format!("  {path} ({BASELINE}:{})", row.line));
        }
    }
    if over.is_empty() && stale.is_empty() && missing.is_empty() {
        r.say(format!("{GATE}: OK"));
    }
    if !over.is_empty() {
        r.fail(format!(
            "{GATE}: FAIL (a quantity has one width and data is generic over `F`: a crossing is \
             `F::from_unit`, `F::lit(<number>)`, a kernel's `Design::of`, the lane render or an edge; \
             or the file is a row of {BASELINE}):"
        ));
        over.into_iter().for_each(|l| r.fail(l));
    }
    if !missing.is_empty() {
        r.fail(format!(
            "{GATE}: FAIL (an edge of EDGES names no library file; delete it or fix its path):"
        ));
        missing.iter().for_each(|p| r.fail(format!("  {p}")));
    }
    if !stale.is_empty() {
        r.fail(format!(
            "{GATE}: FAIL (a row of {BASELINE} matches no site; delete it):"
        ));
        stale.into_iter().for_each(|l| r.fail(l));
    }
    let forms: Vec<String> = Form::ALL
        .iter()
        .filter_map(|f| by_form.get(f).map(|n| format!("{n} {}", f.name())))
        .collect();
    r.say(format!(
        "{GATE}: {total} sites in {} files, {} rows in {BASELINE} ({})",
        matched.len(),
        rows.len(),
        forms.join(", ")
    ));
    if !slack.is_empty() {
        r.say(format!(
            "{GATE}: {BASELINE} can drop; lower these to the current counts:"
        ));
        slack.into_iter().for_each(|l| r.say(format!("  {l}")));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const F: &str = "crates/a/src/lib.rs";
    const E: &[(&str, &str)] = &[
        ("crates/a/src/edge.rs", "an edge"),
        ("crates/d/src/", "a dir"),
    ];

    fn forms(src: &str) -> Vec<&'static str> {
        scan(F, src).iter().map(|h| h.form.name()).collect()
    }

    fn run(baseline: &str, files: &[(&str, &str)]) -> Report {
        let mut all = vec![(BASELINE, baseline)];
        all.extend_from_slice(files);
        for (edge, _) in E {
            let path = if edge.ends_with('/') {
                format!("{edge}x.rs")
            } else {
                (*edge).to_owned()
            };
            if !files.iter().any(|(p, _)| *p == path) {
                all.push((Box::leak(path.into_boxed_str()), "fn ok() {}\n"));
            }
        }
        check(&Mem::with(&all), E, 0)
    }

    #[test]
    fn the_named_crossings_are_no_site() {
        assert!(
            forms(
                "fn d() { let g = F::from_design(k.tan()); let c = F::from_unit(hz); \
                 let t = F::from_count((i + 1) as u32); }"
            )
            .is_empty()
        );
    }

    #[test]
    fn each_cast_spelling_is_a_site_on_any_path() {
        assert_eq!(forms("fn f() { let a = F::from_f32(x); }"), ["from_f32"]);
        assert_eq!(
            forms("fn f() { Seconds::from_f32(x); S::from_f32 (x); }"),
            ["from_f32"; 2]
        );
        assert_eq!(
            forms("fn f() { <F as Float>::from_f64(x); Self::from_f64(x) }"),
            ["from_f64"; 2]
        );
        assert_eq!(forms("fn f() { x.to_f32(); }"), ["to_f32"]);
        assert_eq!(forms("fn f() { x.to_f64() + y.to_f64(); }"), ["to_f64"; 2]);
        assert_eq!(
            forms("fn f() { let a = n as f32; let b = (k + 1) as   f32 / 2.0; }"),
            ["as_f32"; 2]
        );
    }

    #[test]
    fn a_lossless_or_numeric_literal_crossing_is_not_a_site() {
        let src = "fn f() { f64::from(x); F::lit(0.5); F::lit(-2.0); F::lit(1e-9); \
                   S::lit(3_f64); n as f64; n as f32x8; x.as_f32(); }";
        assert!(forms(src).is_empty(), "{:?}", forms(src));
    }

    #[test]
    fn a_lit_of_anything_but_a_numeric_literal_is_a_site() {
        assert_eq!(
            forms("fn f() { F::lit(x); F::lit(2.0 * x); y.lit(K); }"),
            ["lit"; 3]
        );
    }

    #[test]
    fn a_definition_is_not_a_call() {
        assert!(
            forms("trait T { fn from_f32(x: f64) -> Self; fn to_f32(&self) -> Self; }").is_empty()
        );
    }

    #[test]
    fn a_comment_a_doc_example_and_a_string_are_not_code() {
        let src = "// x as f32\n/// F::from_f32(x)\n//! impl Node for X {}\n\
                   /* y.to_f32() */\nfn f() { let s = \"x as f32 F::from_f64(1)\"; }\n";
        assert!(forms(src).is_empty());
    }

    #[test]
    fn an_inline_cfg_test_item_is_not_library_code_but_the_code_after_it_is() {
        let src = "fn a() { x as f32; }\n#[cfg(test)]\nmod tests { fn t() { y as f32; } }\n\
                   fn after() { z as f32; w.to_f32(); }\n";
        assert_eq!(forms(src), ["to_f32", "as_f32", "as_f32"]);
        let src = "pub fn a(x: f32) {}\n#[cfg(test)]\nmod tests { pub fn t(x: f32) {} }\n";
        assert_eq!(forms(src), ["bare_f32"]);
    }

    #[test]
    fn a_test_contract_testing_or_support_file_is_not_library_code() {
        let bad = "fn f() { x as f32; }\n";
        let mem = Mem::with(&[
            (BASELINE, ""),
            ("crates/a/src/tests.rs", bad),
            ("crates/a/src/m/tests/x.rs", bad),
            ("crates/a/src/param_tests.rs", bad),
            ("crates/a/src/contract.rs", bad),
            ("crates/a/src/contract/x.rs", bad),
            ("crates/a/src/testing.rs", bad),
            ("crates/a/src/testing/x.rs", bad),
            ("crates/a/src/test_support.rs", bad),
            ("crates/a/tests/x.rs", bad),
            ("crates/a/benches/x.rs", bad),
            ("crates/a/examples/x.rs", bad),
            ("testing/b/src/lib.rs", bad),
            ("crates/a/src/edge.rs", "fn ok() {}\n"),
            ("crates/d/src/x.rs", "fn ok() {}\n"),
        ]);
        let r = check(&mem, E, 0);
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[1].contains("0 sites in 0 files"), "{:?}", r.out);
    }

    #[test]
    fn an_out_of_line_cfg_test_mod_hides_only_itself() {
        let src = "#[cfg(test)]\nmod tests;\nfn a() { x as f32; }\n";
        assert_eq!(forms(src), ["as_f32"]);
        assert_eq!(scan(F, src)[0].at, "line 3");
        let src = "#[cfg(test)]\nmod t { fn a() { y as f32; } }\nfn b() { z.to_f32(); }\n\
                   #[cfg(test)]\nuse x::y;\nfn c() { w as f32; }\n";
        assert_eq!(forms(src), ["to_f32", "as_f32"]);
    }

    #[test]
    fn a_file_its_parent_declares_under_cfg_test_is_not_library_code() {
        let bad = "fn f() { x as f32; }\n";
        let r = run(
            "",
            &[
                (
                    "crates/a/src/lib.rs",
                    "#[cfg(test)]\nmod doubles;\nmod live;\n",
                ),
                ("crates/a/src/doubles.rs", bad),
                ("crates/a/src/doubles/inner.rs", bad),
                ("crates/a/src/live.rs", bad),
                (
                    "crates/a/src/ledger.rs",
                    "#[cfg(test)]\npub(crate) mod graphs;\n",
                ),
                ("crates/a/src/ledger/graphs/mod.rs", bad),
                (
                    "crates/a/src/lib2.rs",
                    "#[cfg(test)]\n#[path = \"lib2/fake.rs\"]\nmod fake;\n",
                ),
                ("crates/a/src/lib2/fake.rs", bad),
            ],
        );
        assert_eq!(
            r.findings[1..],
            [
                "  crates/a/src/live.rs: 1 sites, max 0",
                "    as_f32: line 1"
            ]
        );
    }

    #[test]
    fn only_tests_rs_and_a_suffix_tests_rs_are_test_files() {
        let bad = "fn f() { x as f32; }\n";
        let r = run(
            "",
            &[
                ("crates/a/src/contests.rs", bad),
                ("crates/a/src/tests.rs", bad),
                ("crates/a/src/x_tests.rs", bad),
            ],
        );
        assert_eq!(r.findings[1], "  crates/a/src/contests.rs: 1 sites, max 0");
    }

    #[test]
    fn an_edge_that_names_no_library_file_fails() {
        let mem = Mem::with(&[
            (BASELINE, ""),
            (F, "fn f() {}\n"),
            ("crates/a/src/edge.rs", "fn f() {}\n"),
        ]);
        let r = check(&mem, E, 0);
        assert!(!r.passed());
        assert!(r.findings[0].contains("names no library file"));
        assert_eq!(r.findings[1], "  crates/d/src/");
        let r = check(&mem, &[("crates/a/src/gone.rs", "gone")], 0);
        assert_eq!(r.findings[1], "  crates/a/src/gone.rs");
    }

    #[test]
    fn a_plain_node_impl_is_a_site_and_one_over_f_is_not() {
        let src = "impl Node for X {}\n\
                   impl Node<f32> for Y {}\n\
                   impl<T: Float> Node for Z<T> {}\n\
                   impl tutti_graph::Node for W {}\n\
                   impl<F: Float> Node<F> for A {}\n\
                   impl<F> Node<F> for B<F> where F: Float {}\n\
                   impl Node<f64> for C {}\n\
                   impl IntoNode for D {}\n\
                   impl Display for Node {}\n\
                   fn make() -> impl Node { todo!() }\n";
        assert_eq!(forms(src), ["plain_node"; 4]);
    }

    #[test]
    fn an_impl_node_in_a_doc_comment_is_not_a_site() {
        let src =
            "/// ```\n/// impl Node for Gain {}\n/// ```\n//! impl Node for X {}\npub struct S;\n";
        assert!(forms(src).is_empty());
    }

    #[test]
    fn a_pub_fn_named_for_a_width_is_a_site() {
        let src = "pub fn run_f32() {}\npub const fn to_f64() {}\nfn private_f32() {}\n\
                   pub(crate) fn crate_f32() {}\nimpl X { pub fn new_f64(&self) {} }\n";
        assert_eq!(forms(src), ["fn_twin"; 3]);
    }

    #[test]
    fn an_f32_unit_or_cell_is_a_site() {
        let src = "pub struct Hz(pub f32);\nstruct Q(f32);\nstruct Ok(f64);\nstruct Pair(f32, f32);\n\
                   struct Named { x: f32 }\nstruct C { cell: Arc<AtomicF32> }\n";
        let cells: Vec<_> = forms(src)
            .into_iter()
            .filter(|f| *f == "f32_cell")
            .collect();
        assert_eq!(cells, ["f32_cell"; 3]);
    }

    #[test]
    fn an_f32_in_a_public_signature_or_field_is_a_site_once_per_item() {
        let src = "pub fn a(x: &[f32], y: f32) -> f32 { 0.0 }\n\
                   pub struct S { pub v: Vec<f32>, w: f32, pub k: u8 }\n\
                   pub enum E { A(f32), B }\n\
                   pub type T = f32;\n\
                   pub trait Tr { fn m(&self, x: f32); }\n\
                   pub fn wide(x: f32x8) {}\n";
        assert_eq!(forms(src), ["bare_f32"; 5]);
    }

    #[test]
    fn a_private_or_f32_free_item_is_not_a_bare_f32_site() {
        let src = "fn a(x: f32) {}\npub(crate) fn b(x: f32) {}\npub fn c(x: f64) {}\n\
                   pub struct S { x: f32, pub y: f64 }\nimpl Tr for S { fn m(&self, x: f32) {} }\n\
                   pub fn d() { let x: f32 = 1.0; }\n";
        assert!(forms(src).is_empty(), "{:?}", forms(src));
    }

    #[test]
    fn a_file_over_its_max_fails_naming_each_site() {
        let r = run(
            "crates/a/src/lib.rs  1  migrating: B2/B3\n",
            &[(F, "fn f() { a as f32; b.to_f32(); }\n")],
        );
        assert!(!r.passed());
        assert_eq!(
            r.findings[1..],
            [
                "  crates/a/src/lib.rs: 2 sites, max 1",
                "    to_f32: line 1",
                "    as_f32: line 1",
            ]
            .map(str::to_owned)
        );
    }

    #[test]
    fn a_file_with_no_row_has_max_zero() {
        let r = run("", &[(F, "fn f() { a as f32; }\n")]);
        assert!(!r.passed());
        assert!(r.findings[1].ends_with("1 sites, max 0"));
    }

    #[test]
    fn a_file_at_or_below_its_max_passes_and_reports_the_slack() {
        let r = run(
            "crates/a/src/lib.rs  3  migrating: R1/S7\n",
            &[(F, "fn f() { a as f32; }\n")],
        );
        assert!(r.passed());
        assert_eq!(r.out[0], "check-width: OK");
        assert!(
            r.out
                .iter()
                .any(|l| l.contains("crates/a/src/lib.rs 3 -> 1"))
        );
        let r = run(
            "crates/a/src/lib.rs  1  migrating: R1/S7\n",
            &[(F, "fn f() { a as f32; }\n")],
        );
        assert!(r.passed());
        assert!(!r.out.iter().any(|l| l.contains("can drop")));
    }

    #[test]
    fn a_row_with_no_match_fails() {
        let r = run(
            "crates/a/src/lib.rs  2  migrating: R1/S7\n",
            &[(F, "fn f() {}\n")],
        );
        assert!(!r.passed());
        assert!(r.findings[0].contains("matches no site"));
        assert_eq!(
            r.findings[1],
            "  crates/a/src/lib.rs (xtask/width-baseline.txt:1)"
        );
        let r = run(
            "crates/gone/src/lib.rs  2  migrating: R1/S7\n",
            &[(F, "fn f() {}\n")],
        );
        assert!(!r.passed());
    }

    #[test]
    fn a_row_for_an_edge_file_matches_no_site() {
        let r = run(
            "crates/a/src/edge.rs  1  migrating: R1/S7\n",
            &[("crates/a/src/edge.rs", "fn f() { a as f32; }\n")],
        );
        assert!(!r.passed());
        assert!(r.findings[0].contains("matches no site"));
    }

    #[test]
    fn a_cast_inside_an_edge_file_is_allowed() {
        let src =
            "pub fn process_f32(x: &[f32]) { a as f32; F::from_f32(b); c.to_f64(); F::lit(x); }\n";
        for path in [
            "crates/a/src/edge.rs",
            "crates/d/src/deep/x.rs",
            "crates/a-ffi/src/lib.rs",
            "crates/tutti-vst3-ffi/src/x.rs",
        ] {
            let r = run("", &[(path, src)]);
            assert!(r.passed(), "{path}: {:?}", r.findings);
        }
        let r = run("", &[("crates/a/src/not_edge.rs", src)]);
        assert!(!r.passed());
    }

    #[test]
    fn an_edge_file_still_refuses_a_plain_node_and_an_f32_cell() {
        let r = run(
            "",
            &[(
                "crates/a/src/edge.rs",
                "impl Node for X {}\nstruct Q(f32);\n",
            )],
        );
        assert!(!r.passed());
        assert_eq!(
            r.findings[2..],
            ["    plain_node: line 1", "    f32_cell: line 2"].map(str::to_owned)
        );
    }

    #[test]
    fn a_malformed_row_fails() {
        for row in [
            "crates/a/src/lib.rs",
            "crates/a/src/lib.rs  1",
            "crates/a/src/lib.rs  many  migrating: B2/B3",
            "crates/a/src/lib.rs  0  migrating: B2/B3",
            "crates/a/src/lib.rs  1  later",
            "crates/a/src/lib.rs  1  migrating:",
            "crates/a/src/lib.rs  1  spec:",
            "crates/a/src/lib.rs  1  spec-f32: CLAP",
            "crates/a/src/lib.rs  1  ffi",
        ] {
            let r = run(&format!("{row}\n"), &[(F, "fn f() { a as f32; }\n")]);
            assert!(!r.passed(), "{row}");
            assert!(r.findings[0].contains("malformed"), "{row}");
        }
        let twice =
            "crates/a/src/lib.rs  1  migrating: B2/B3\ncrates/a/src/lib.rs  2  migrating: B2/B3\n";
        let r = run(twice, &[(F, "fn f() { a as f32; }\n")]);
        assert!(r.findings[1].ends_with("is listed twice"));
    }

    #[test]
    fn a_spec_row_is_kept_counted_and_ratcheted() {
        let row =
            "crates/a/src/lib.rs  1  spec: f32 is the audio width every plugin format processes\n";
        assert!(run(row, &[(F, "fn f() { a as f32; }\n")]).passed());
        let over = run(row, &[(F, "fn f() { a as f32; b as f32; }\n")]);
        assert!(!over.passed(), "a spec row holds its max like any row");
        let stale = run(row, &[(F, "fn f() {}\n")]);
        assert!(!stale.passed(), "a spec row whose site is gone is deleted");
    }

    #[test]
    fn comments_and_blank_lines_of_the_baseline_are_skipped() {
        let r = run(
            "# why\n\n   \ncrates/a/src/lib.rs  1  migrating: B2/B3\n",
            &[(F, "fn f() { a as f32; }\n")],
        );
        assert!(r.passed());
    }

    #[test]
    fn a_missing_baseline_fails_and_so_does_a_walk_that_reads_too_few_files() {
        assert!(!check(&Mem::with(&[(F, "fn f() {}\n")]), E, 0).passed());
        let r = check(&Mem::with(&[(BASELINE, ""), (F, "fn f() {}\n")]), E, 2);
        assert!(r.findings[0].contains("the walk is broken"));
    }

    #[test]
    fn the_summary_counts_sites_by_form() {
        let r = run(
            "crates/a/src/lib.rs  3  migrating: B2/B3\n",
            &[(F, "fn f() { a as f32; b as f32; F::from_f64(c); }\n")],
        );
        assert_eq!(
            r.out[1],
            "check-width: 3 sites in 1 files, 1 rows in xtask/width-baseline.txt (1 from_f64, 2 as_f32)"
        );
    }
}
