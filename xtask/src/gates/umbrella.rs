//! `check-umbrella`: the `tutti` umbrella re-exports no crate whole and offers
//! every item at one path.
//!
//! The umbrella's source (`crates/tutti/src`) is read for each `pub use .. ;` as a
//! use tree ([`super::use_tree`]):
//!
//! 1. **No whole re-export**: `pub use tutti_x;`, `pub use tutti_x as y;`,
//!    `pub use ::tutti_x ..`, `pub use tutti_x::{self}` and any glob are findings.
//!    Each item is listed by name.
//! 2. **One item, one path**: an item (its source path: crate, modules, name) is
//!    exported once, under one name: `pub use tutti_x::A` and `pub use tutti_x::A
//!    as B`, or `tutti_x::A` in two modules, are findings, and so is a module
//!    exported by name (`pub use tutti_x::module`) together with one of its items.
//!    Two different items that share a name in different modules are fine: a
//!    module is a namespace, and the compiler refuses a name twice in one.
//!
//! 3. **One noun per concept**: a name exported for items of more than one crate, or an
//!    item at two paths, is a finding unless a row of `xtask/umbrella-collisions-baseline.txt`
//!    or `-allow.txt` covers it ([`super::umbrella_collisions`]).
//!
//! Each finding of 1 and 2 names the file and line. The rule reads source paths, so one item
//! reached through two crates (a type `tutti_nodes` re-exports from `tutti_dsp`,
//! exported from both) is not seen.
//!
//! Its companion `cargo xtask umbrella-api` ([`super::umbrella_api`]) reads the
//! other way: every type in the signature of an exported item is itself exported.

use super::Report;
use super::Tree;
use super::lex::skipped;
use super::use_tree::{Leaf, placed_uses};
use std::collections::BTreeMap;

pub(super) const SRC: &str = "crates/tutti/src";

/// The module path of a file under the umbrella's `src`, as segments.
pub(super) fn file_module(path: &str) -> Vec<String> {
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

/// Where one export stands: its module, the name it takes and `file:line`.
struct At {
    module: String,
    name: String,
    place: String,
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let mut r = items(tree);
    let c = super::umbrella_collisions::check(tree);
    r.out.extend(c.out);
    r.findings.extend(c.findings);
    r
}

/// One `pub use` leaf of a crate's source: the file, its `file:line`, the
/// inline modules the statement sits in, and the leaf.
pub(super) struct Export {
    pub(super) path: String,
    pub(super) place: String,
    pub(super) module: Vec<String>,
    pub(super) leaf: Leaf,
}

/// The `.rs` files under `dir` and every `pub use` leaf in them.
pub(super) fn exports(tree: &dyn Tree, dir: &str) -> (usize, Vec<Export>) {
    let files: Vec<String> = tree
        .files(dir, &skipped)
        .into_iter()
        .filter(|p| p.ends_with(".rs"))
        .collect();
    let mut out = Vec::new();
    for path in &files {
        let src = tree.read(path).unwrap_or_default();
        for u in placed_uses(&src) {
            out.push(Export {
                path: path.clone(),
                place: format!("{path}:{}", u.line),
                module: u.module,
                leaf: u.leaf,
            });
        }
    }
    (files.len(), out)
}

fn items(tree: &dyn Tree) -> Report {
    let mut r = Report::default();
    let (nfiles, uses) = exports(tree, SRC);
    if nfiles == 0 {
        r.fail("check-umbrella: FAIL");
        r.fail(format!("  {SRC} holds no .rs file; was the crate moved?"));
        return r;
    }

    let mut bad: Vec<String> = Vec::new();
    // Item path -> every export of it.
    let mut seen: BTreeMap<String, Vec<At>> = BTreeMap::new();
    for Export {
        path,
        place,
        module,
        leaf,
    } in uses
    {
        if leaf.whole {
            bad.push(format!(
                "{place}: re-exports a crate or module whole (`{}`); list each item by name",
                leaf.name
            ));
            continue;
        }
        if leaf.name == "_" {
            continue;
        }
        let mut full = file_module(&path);
        full.extend(module);
        let module = if full.is_empty() {
            "the root".to_owned()
        } else {
            full.join("::")
        };
        seen.entry(leaf.item()).or_default().push(At {
            module,
            name: leaf.name,
            place,
        });
    }
    for (item, at) in &seen {
        if at.len() > 1 {
            let paths: Vec<String> = at
                .iter()
                .map(|a| format!("`{}` in {} ({})", a.name, a.module, a.place))
                .collect();
            bad.push(format!(
                "{}: `{item}` is exported {} times: {}; keep one `pub use`",
                at[1].place,
                at.len(),
                paths.join(", ")
            ));
        }
        // An item under a module that is exported by name is exported twice.
        for (under, inner) in seen.range(format!("{item}::")..) {
            if !under.starts_with(&format!("{item}::")) {
                break;
            }
            bad.push(format!(
                "{}: `{under}` is also under `{item}`, which {} exports whole by name; keep one `pub use`",
                inner[0].place, at[0].place
            ));
        }
    }

    if bad.is_empty() {
        r.say(format!(
            "check-umbrella: ok (tutti re-exports no crate whole and {} items sit at one path each; {} file{})",
            seen.len(),
            nfiles,
            if nfiles == 1 { "" } else { "s" }
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

    /// The item rules alone; the collision lists have their own tests.
    fn check(tree: &dyn Tree) -> Report {
        items(tree)
    }

    #[test]
    fn the_entry_point_adds_the_collision_findings() {
        let mut m = Mem::with(&[(LIB, "pub use tutti_a::Hz;\npub use tutti_b::Hz;\n")]);
        m.files.insert(
            "xtask/umbrella-collisions-baseline.txt".into(),
            String::new(),
        );
        m.files
            .insert("xtask/umbrella-collisions-allow.txt".into(), String::new());
        assert!(items(&m).passed());
        let r = super::check(&m);
        assert!(
            r.findings
                .iter()
                .any(|f| f.contains("crates  Hz  tutti_a,tutti_b"))
        );
    }

    fn lib(src: &str) -> Report {
        check(&Mem::with(&[(LIB, src)]))
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
    pub use tutti_units::{Hz, Db};
}
";

    #[test]
    fn a_curated_umbrella_passes_and_counts_its_items() {
        let r = lib(CURATED);
        assert!(r.passed(), "{:?}", r.findings);
        assert_eq!(
            r.out[0],
            "check-umbrella: ok (tutti re-exports no crate whole and 8 items sit at one path each; 1 file)"
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
            ("pub use local::*;", "*", 2),
            ("pub use tutti_graph::{self};", "tutti_graph", 2),
            ("pub use tutti_graph::{self as g};", "g", 2),
            ("pub use {tutti_engine as e, tutti_units::Hz};", "e", 2),
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
                    "  crates/tutti/src/lib.rs:{line}: re-exports a crate or module whole"
                )),
                "{src}: {:?}",
                r.findings
            );
            assert!(r.findings[1].contains(&format!("(`{name}`)")), "{src}");
        }
    }

    #[test]
    fn the_same_item_at_two_paths_is_reported_with_both() {
        let r = lib("pub use tutti_a::{Hz, Db};\npub mod types {\n    pub use tutti_a::Hz;\n}\n");
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(
            r.findings[1].contains("crates/tutti/src/lib.rs:3: `tutti_a::Hz` is exported 2 times")
        );
        assert!(r.findings[1].contains("`Hz` in the root (crates/tutti/src/lib.rs:1)"));
        assert!(r.findings[1].contains("`Hz` in types (crates/tutti/src/lib.rs:3)"));
    }

    #[test]
    fn the_same_item_under_an_alias_is_reported() {
        let r = lib("pub use tutti_a::Error;\npub mod m { pub use tutti_a::Error as AError; }\n");
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(r.findings[1].contains("`tutti_a::Error` is exported 2 times"));
        assert!(r.findings[1].contains("`AError` in m"));
        let twice = lib("pub use tutti_a::{Hz, Hz as H};\n");
        assert!(twice.findings[1].contains("`tutti_a::Hz` is exported 2 times"));
    }

    #[test]
    fn two_items_sharing_a_name_in_different_modules_pass() {
        let r = lib(
            "pub use tutti_session::Key;\npub mod midi { pub use tutti_ump::Key; }\n\
             pub mod graph { pub use tutti_graph::Key; }\n",
        );
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_module_exported_by_name_and_one_of_its_items_are_reported() {
        let r = lib("pub use tutti_a::m;\npub mod x {\n    pub use tutti_a::m::Item;\n}\n");
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(r.findings[1].contains("lib.rs:3: `tutti_a::m::Item` is also under `tutti_a::m`"));
        // A sibling that only shares the prefix text is not under it.
        assert!(lib("pub use tutti_a::m;\npub use tutti_a::mx::Item;\n").passed());
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
    fn a_file_module_is_its_own_path_but_its_item_is_the_same_item() {
        let mut m = Mem::with(&[(LIB, "pub use a::Hz;\n")]);
        m.files.insert(
            "crates/tutti/src/types.rs".into(),
            "pub use a::Hz;\npub use b::Hz as Other;\n".into(),
        );
        let r = check(&m);
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(r.findings[1].contains("types.rs:1: `a::Hz` is exported 2 times"));
        assert!(r.findings[1].contains("in the root"));
        assert!(r.findings[1].contains("in types"));
    }

    #[test]
    fn a_missing_source_directory_fails() {
        let r = check(&Mem::default());
        assert_eq!(r.findings[0], "check-umbrella: FAIL");
        assert!(r.findings[1].contains("holds no .rs file"));
    }
}
