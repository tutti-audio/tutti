//! The scan: the source files of each crate read into their public items.

use super::super::Tree;
use super::super::buffers::blocks;
use super::super::building::{macro_bodies, source_files};
use super::super::lex::{blank, find_word_from, ident_at, line_of, matching, test_ranges};
use std::collections::{BTreeMap, BTreeSet};

/// A public item: its key, and where it is declared.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Item {
    pub(super) key: String,
    pub(super) file: String,
    pub(super) line: usize,
}

/// The public items of each crate under `crates/`, sorted by key.
pub(super) fn items(tree: &dyn Tree) -> BTreeMap<String, Vec<Item>> {
    let files = source_files(tree);
    let skipped = test_modules(tree, &files);
    let mut out: BTreeMap<String, Vec<Item>> = BTreeMap::new();
    for path in files {
        let src_dir = path.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
        let rel = &path[src_dir.len() + 1..];
        if rel == "main.rs"
            || rel.starts_with("bin/")
            || skipped
                .iter()
                .any(|s| path == *s || path.starts_with(&format!("{}/", s.trim_end_matches(".rs"))))
        {
            continue;
        }
        let Some(src) = tree.read(&path) else {
            continue;
        };
        let krate = path.split('/').nth(1).unwrap_or("").to_owned();
        out.entry(krate).or_default().extend(scan(&path, &src));
    }
    out.values_mut().for_each(|v| v.sort());
    out
}

/// The files that `#[cfg(test)] mod name;` declares, as `foo.rs` and `foo/mod.rs`
/// beside the file that declares them.
fn test_modules(tree: &dyn Tree, files: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for path in files {
        let Some(src) = tree.read(path) else {
            continue;
        };
        let code = blank(&src);
        let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
        let home = match file {
            "lib.rs" | "main.rs" | "mod.rs" => dir.to_owned(),
            f => format!("{dir}/{}", f.trim_end_matches(".rs")),
        };
        for (a, z) in test_ranges(&code) {
            let mut at = a;
            while let Some(m) = find_word_from(&code[..z], "mod", at) {
                at = m + 3;
                let (name, end) = ident_at(&code, at);
                if !name.is_empty() && code[end..].trim_start().starts_with(';') {
                    out.insert(format!("{home}/{name}.rs"));
                    out.insert(format!("{home}/{name}/mod.rs"));
                }
            }
        }
    }
    out
}

/// The module path of a file inside its crate: `crates/c/src/a/mod.rs` is `a`.
fn module_path(path: &str) -> Vec<String> {
    let rel = path.splitn(4, '/').nth(3).unwrap_or("");
    let mut parts: Vec<String> = rel
        .trim_end_matches(".rs")
        .split('/')
        .map(str::to_owned)
        .collect();
    if matches!(
        parts.last().map(String::as_str),
        Some("lib" | "main" | "mod")
    ) {
        parts.pop();
    }
    parts
}

/// The kind and name of the item whose `pub` is at `p`, or `None` for a field, a `pub use`
/// or another item that is no listed kind.
fn declared(code: &str, p: usize) -> Option<(&'static str, String)> {
    let mut at = p + 3;
    let (kind, end) = loop {
        let (w, end) = ident_at(code, at);
        match w {
            "const" => {
                let (next, _) = ident_at(code, end);
                if !matches!(next, "fn" | "unsafe" | "async" | "extern") {
                    break ("const", end);
                }
                at = end;
            }
            "async" | "unsafe" | "default" | "auto" => at = end,
            "extern" => {
                let rest = code[end..].trim_start();
                at = end;
                if rest.starts_with('"') {
                    let open = code[end..].find('"').map_or(end, |q| end + q);
                    at = code[open + 1..].find('"').map_or(open, |q| open + 1 + q) + 1;
                }
            }
            "fn" => break ("fn", end),
            "struct" => break ("struct", end),
            "enum" => break ("enum", end),
            "trait" => break ("trait", end),
            "type" => break ("type", end),
            "static" => break ("static", end),
            "mod" => break ("mod", end),
            "union" => break ("union", end),
            _ => return None,
        }
    };
    let (mut name, mut after) = ident_at(code, end);
    if kind == "static" && name == "mut" {
        (name, after) = ident_at(code, after);
    }
    let _ = after;
    (!name.is_empty()).then(|| (kind, name.to_owned()))
}

/// The public items of one file's text.
pub(super) fn scan(path: &str, src: &str) -> Vec<Item> {
    let code = blank(src);
    let tests = test_ranges(&code);
    let in_test = |p: usize| tests.iter().any(|&(a, z)| a <= p && p <= z);
    let macros = macro_bodies(&code);
    let in_macro = |p: usize| macros.iter().any(|(a, z, _)| *a < p && p < *z);
    let blocks = blocks(&code);
    let inline = inline_mods(&code);
    let base = module_path(path);
    let item = |p: usize, kind: &str, name: &str| {
        let mut parts = base.clone();
        let mut around: Vec<_> = inline.iter().filter(|m| m.0 < p && p < m.1).collect();
        around.sort_by_key(|m| m.0);
        parts.extend(around.iter().map(|m| m.2.clone()));
        if let Some(b) = blocks
            .iter()
            .filter(|b| b.open < p && p < b.close)
            .max_by_key(|b| b.open)
        {
            parts.push(b.owner.clone());
        }
        parts.push(name.to_owned());
        Item {
            key: format!("{kind} {}", parts.join("::")),
            file: path.to_owned(),
            line: line_of(&code, p),
        }
    };
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = find_word_from(&code, "pub", from) {
        from = p + 3;
        if in_test(p) || in_macro(p) || code[from..].trim_start().starts_with('(') {
            continue;
        }
        if let Some((kind, name)) = declared(&code, p) {
            out.push(item(p, kind, &name));
        }
    }
    let mut from = 0;
    while let Some(p) = find_word_from(&code, "macro_export", from) {
        from = p + 12;
        let Some(m) = find_word_from(&code, "macro_rules", from) else {
            break;
        };
        if in_test(p) || code[from..m].contains(['{', '}', ';']) {
            continue;
        }
        let rest = code[m + 11..].trim_start();
        if let Some(rest) = rest.strip_prefix('!') {
            let (name, _) = ident_at(code.as_str(), code.len() - rest.len());
            out.push(item(p, "macro", name));
        }
    }
    out
}

/// The `mod name { .. }` blocks of a file: where each opens and closes, and its name.
fn inline_mods(code: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(m) = find_word_from(code, "mod", from) {
        from = m + 3;
        let (name, end) = ident_at(code, from);
        if name.is_empty() || !code[end..].trim_start().starts_with('{') {
            continue;
        }
        let open = end + code[end..].find('{').unwrap_or(0);
        if let Some(close) = matching(code, open, b'{', b'}') {
            out.push((open, close, name.to_owned()));
        }
    }
    out
}
