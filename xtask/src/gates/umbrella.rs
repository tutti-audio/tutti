//! `check-umbrella`: the `tutti` umbrella re-exports no crate whole and offers
//! every item at one path.
//!
//! The umbrella's source (`crates/tutti/src`, every file lexed so comments and
//! literals are blanked) is read for each `pub use .. ;` as a use tree:
//!
//! 1. **No whole-crate re-export**: `pub use tutti_x;`, `pub use tutti_x as y;`,
//!    `pub use ::tutti_x ..`, `pub use tutti_x::{self}` and any glob (`::*`, under
//!    a `tutti_` path or a local one) are findings. Each item is listed by name.
//!    A module re-exported by its name (`pub use tutti_x::module;`) is not
//!    told apart from a function lexically and is not seen.
//! 2. **One path per item**: the exported name (the alias when there is an
//!    `as`) of every `pub use` leaf is collected per module path (the crate
//!    root, then each inline `pub mod` and each file module). A name that is
//!    exported twice, in one module or in two, is a finding.
//!
//! A name may sit at two paths when [`ALLOW`] holds a row for it with the reason;
//! a row for a name that is no longer duplicated is a finding. Each finding names
//! the file and line. The rule reads names, so one item exported under two
//! names is not seen, and a `cfg` pair offering one name from two sources needs
//! a row.

use super::lex::{blank, find_word_from, line_of, matching};
use super::session::{is_word, pub_uses, tokens};
use super::{Report, Tree};
use std::collections::BTreeMap;

const SRC: &str = "crates/tutti/src";

/// Names exported at two paths on purpose, each with its reason.
pub(super) const ALLOW: &[(&str, &str)] = &[
    (
        "DeviceId",
        "the sound device's id (`tutti_cpal`) at the root and the MIDI device's (`tutti_midi_device`) in `midi`: two types, one name",
    ),
    (
        "Bus",
        "the bus handle a node's shape issues (`graph::Bus`) and the session's bus key (`session::Bus`): two types, one name",
    ),
    (
        "Params",
        "the wrapper a node with params is inserted as (`graph::Params`) and a plugin's parameter view at the root: two types, one name",
    ),
    (
        "Error",
        "the session's one error at the root and `midi::Error`, the MIDI error it nests (`tutti_session::MidiError`)",
    ),
];

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git")
}

/// One leaf of a `pub use` tree.
#[derive(Debug, PartialEq, Eq)]
struct Leaf {
    /// The name the leaf exports; `*` for a glob.
    name: String,
    /// The leaf re-exports a `tutti_` crate whole, or is a glob.
    whole: bool,
}

/// The leaves of the use tree in `t` at `*k` under the prefix `pre`.
fn leaves(t: &[&str], k: &mut usize, pre: &[String], out: &mut Vec<Leaf>) {
    let mut path = pre.to_vec();
    if t.get(*k) == Some(&"::") {
        *k += 1;
    }
    let from_tutti = |path: &[String]| path.first().is_some_and(|p| p.starts_with("tutti_"));
    while *k < t.len() {
        let tok = t[*k];
        if tok == "{" {
            *k += 1;
            while *k < t.len() && t[*k] != "}" {
                leaves(t, k, &path, out);
                if t.get(*k) == Some(&",") {
                    *k += 1;
                }
            }
            *k += 1;
            return;
        }
        if tok == "*" {
            *k += 1;
            out.push(Leaf {
                name: "*".to_owned(),
                whole: true,
            });
            return;
        }
        if tok.bytes().all(is_word) {
            *k += 1;
            if t.get(*k) == Some(&"::") {
                path.push(tok.to_owned());
                *k += 1;
                continue;
            }
            let alias = if t.get(*k) == Some(&"as") {
                *k += 2;
                t.get(*k - 1).copied()
            } else {
                None
            };
            let self_leaf = tok == "self";
            let name = match (alias, self_leaf) {
                (Some(a), _) => a,
                (None, true) => path.last().map_or(tok, String::as_str),
                (None, false) => tok,
            };
            let whole = if self_leaf {
                from_tutti(&path)
            } else {
                path.is_empty() && tok.starts_with("tutti_")
            };
            out.push(Leaf {
                name: name.to_owned(),
                whole,
            });
            return;
        }
        *k += 1;
        return;
    }
}

/// The inline `mod NAME { .. }` blocks of lexed `code`, as `(name, open, close)`.
fn inline_mods(code: &str) -> Vec<(String, usize, usize)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_word_from(code, "mod", from) {
        from = at + 3;
        let mut i = from;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while i < b.len() && is_word(b[i]) {
            i += 1;
        }
        let name = &code[s..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if !name.is_empty()
            && b.get(i) == Some(&b'{')
            && let Some(close) = matching(code, i, b'{', b'}')
        {
            out.push((name.to_owned(), i, close));
        }
    }
    out
}

/// The module path of a file under the umbrella's `src`, as segments.
fn file_module(path: &str) -> Vec<String> {
    let rel = path
        .strip_prefix(SRC)
        .unwrap_or(path)
        .trim_start_matches('/');
    let rel = rel.strip_suffix(".rs").unwrap_or(rel);
    let mut segs: Vec<String> = rel.split('/').map(str::to_owned).collect();
    if matches!(segs.last().map(String::as_str), Some("lib" | "mod")) {
        segs.pop();
    }
    segs
}

pub(super) fn check(tree: &dyn Tree, allow: &[(&str, &str)]) -> Report {
    let mut r = Report::default();
    let files: Vec<String> = tree
        .files(SRC, &skipped)
        .into_iter()
        .filter(|p| p.ends_with(".rs"))
        .collect();
    if files.is_empty() {
        r.fail("check-umbrella: FAIL");
        r.fail(format!("  {SRC} holds no .rs file; was the crate moved?"));
        return r;
    }

    let mut bad: Vec<String> = Vec::new();
    // Exported name -> every `(module path, file:line)` that exports it.
    let mut seen: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut exports = 0;
    for path in &files {
        let src = tree.read(path).unwrap_or_default();
        let code = blank(&src);
        let mods = inline_mods(&code);
        for (at, semi) in pub_uses(&code) {
            let body = code[at + 3..semi].trim_start();
            let body = &body["use".len()..];
            let t = tokens(body);
            let (mut k, mut found) = (0, Vec::new());
            leaves(&t, &mut k, &[], &mut found);
            let place = format!("{path}:{}", line_of(&code, at));
            let mut module = file_module(path);
            module.extend(
                mods.iter()
                    .filter(|(_, open, close)| *open < at && at < *close)
                    .map(|(n, ..)| n.clone()),
            );
            let module = if module.is_empty() {
                "the root".to_owned()
            } else {
                module.join("::")
            };
            for leaf in found {
                if leaf.whole {
                    bad.push(format!(
                        "{place}: re-exports a crate or module whole (`{}`); list each item by name",
                        leaf.name
                    ));
                } else if leaf.name != "*" && leaf.name != "_" {
                    exports += 1;
                    seen.entry(leaf.name)
                        .or_default()
                        .push((module.clone(), place.clone()));
                }
            }
        }
    }
    for (name, why) in allow {
        if seen.get(*name).is_none_or(|at| at.len() < 2) {
            bad.push(format!(
                "ALLOW row `{name}` ({why}) names a name that is not exported twice; delete the row"
            ));
        }
    }
    for (name, at) in &seen {
        if at.len() > 1 && !allow.iter().any(|(n, _)| n == name) {
            let paths: Vec<String> = at.iter().map(|(m, p)| format!("{m} ({p})")).collect();
            bad.push(format!(
                "{}: `{name}` is exported {} times: {}; one path per item",
                at[1].1,
                at.len(),
                paths.join(", ")
            ));
        }
    }

    if bad.is_empty() {
        r.say(format!(
            "check-umbrella: ok (tutti re-exports no crate whole and {exports} items sit at one path each; {} files)",
            files.len()
        ));
    } else {
        r.fail("check-umbrella: FAIL");
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

    const LIB: &str = "crates/tutti/src/lib.rs";

    fn lib(src: &str) -> Report {
        check(&Mem::with(&[(LIB, src)]), &[])
    }

    const CURATED: &str = "\
//! Docs that say `pub use tutti_x;` and `pub use tutti_x::*;` are prose.
pub use tutti_session::{Session, SessionBuilder as Builder, Error};
pub(crate) use tutti_graph;
/// Nodes.
pub mod nodes {
    pub use tutti_nodes::{LfoNode, filters::{SvfFilterNode, SvfType}};
}
pub mod types {
    pub use tutti_types::{Hz, Db};
}
";

    #[test]
    fn a_curated_umbrella_passes_and_counts_its_items() {
        let r = lib(CURATED);
        assert!(r.passed(), "{:?}", r.findings);
        assert_eq!(
            r.out[0],
            "check-umbrella: ok (tutti re-exports no crate whole and 8 items sit at one path each; 1 files)"
        );
    }

    #[test]
    fn a_whole_crate_in_each_spelling_is_reported_with_its_line() {
        for (src, name, line) in [
            ("pub use tutti_engine;", "tutti_engine", 2),
            ("pub use tutti_engine as engine;", "engine", 2),
            ("pub use ::tutti_engine as engine;", "engine", 2),
            ("pub use tutti_engine::*;", "*", 2),
            ("pub use tutti_graph::editor::*;", "*", 2),
            ("pub use self::inner::*;", "*", 2),
            ("pub use super::*;", "*", 2),
            ("pub use tutti_graph::{self};", "tutti_graph", 2),
            ("pub use tutti_graph::{self as g};", "g", 2),
            ("pub use {tutti_engine as e, tutti_types::Hz};", "e", 2),
            (
                "#[cfg(feature = \"x\")]\npub use tutti_x\n    as y;",
                "y",
                3,
            ),
        ] {
            let r = lib(&format!("pub use tutti_ok::Fine;\n{src}"));
            assert_eq!(r.findings[0], "check-umbrella: FAIL", "{src}");
            assert_eq!(r.findings.len(), 2, "{src}: {:?}", r.findings);
            assert!(
                r.findings[1].starts_with(&format!(
                    "  crates/tutti/src/lib.rs:{line}: re-exports a crate whole"
                )),
                "{src}: {:?}",
                r.findings
            );
            assert!(r.findings[1].contains(&format!("(`{name}`)")), "{src}");
        }
    }

    #[test]
    fn a_name_at_the_root_and_in_a_module_is_reported() {
        let r = lib("pub use tutti_a::{Hz, Db};\npub mod types {\n    pub use tutti_b::Hz;\n}\n");
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(r.findings[1].contains("crates/tutti/src/lib.rs:3: `Hz` is exported 2 times"));
        assert!(r.findings[1].contains("the root (crates/tutti/src/lib.rs:1)"));
        assert!(r.findings[1].contains("types (crates/tutti/src/lib.rs:3)"));
    }

    #[test]
    fn a_name_in_two_modules_and_twice_in_one_are_reported() {
        let r = lib(
            "pub mod a { pub use x::Hz; }\npub mod b { pub use y::Hz; }\n\
             pub mod c { pub use x::{Db, Db}; }\n",
        );
        assert_eq!(r.findings.len(), 3, "{:?}", r.findings);
        assert!(r.findings[1].contains("`Db` is exported 2 times"));
        assert!(r.findings[2].contains("`Hz` is exported 2 times"));
    }

    #[test]
    fn an_alias_is_the_exported_name() {
        let ok = lib("pub use a::Error as AError;\npub mod m { pub use b::Error; }\n");
        assert!(ok.passed(), "{:?}", ok.findings);
        let bad = lib("pub use a::Error;\npub mod m { pub use b::Thing as Error; }\n");
        assert!(!bad.passed());
        assert!(bad.findings[1].contains("`Error` is exported 2 times"));
    }

    #[test]
    fn a_file_module_is_its_own_path() {
        let mut m = Mem::with(&[(LIB, "pub use a::Hz;\n")]);
        m.files.insert(
            "crates/tutti/src/types.rs".into(),
            "pub use b::Hz;\n".into(),
        );
        let r = check(&m, &[]);
        assert!(r.findings[1].contains("types.rs:1: `Hz`"));
        assert!(r.findings[1].contains("the root (crates/tutti/src/lib.rs:1), types"));
    }

    #[test]
    fn comments_strings_and_private_uses_are_not_exports() {
        let r = lib(
            "// pub use tutti_x;\n/* pub use a::Hz; */\nuse tutti_x;\npub(crate) use a::Hz;\n\
             pub use a::Hz;\nconst S: &str = \"pub use tutti_y;\";\n",
        );
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn an_allowed_name_may_sit_at_two_paths_and_a_stale_row_fails() {
        let src = "pub use a::Id;\npub mod m { pub use b::Id; }\n";
        let m = Mem::with(&[(LIB, src)]);
        let r = check(&m, &[("Id", "two types")]);
        assert!(r.passed(), "{:?}", r.findings);
        let r = check(&m, &[("Id", "two types"), ("Gone", "no longer duplicated")]);
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(r.findings[1].contains("ALLOW row `Gone`"));
        // The row covers its name only.
        let m = Mem::with(&[(
            LIB,
            "pub use a::Id;\npub mod m { pub use b::Id; pub use b::Other; }\npub use c::Other;\n",
        )]);
        let r = check(&m, &[("Id", "two types")]);
        assert!(
            r.findings[1].contains("`Other` is exported 2 times"),
            "{:?}",
            r.findings
        );
    }

    #[test]
    fn a_missing_source_directory_fails() {
        let r = check(&Mem::default(), &[]);
        assert_eq!(r.findings[0], "check-umbrella: FAIL");
        assert!(r.findings[1].contains("holds no .rs file"));
    }
}
