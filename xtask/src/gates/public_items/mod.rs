//! `check-public-items`: each crate's public items are a list, and the list only
//! shrinks on its own.
//!
//! The surface of a crate grows one `pub` at a time. The gate reads every
//! `crates/<crate>/src/**/*.rs` and lists its public items; `xtask/public-items/<crate>.txt`
//! holds one row per item, so an added item is a line in the diff of the pull request
//! that adds it, and that pull request argues for it.
//!
//! This is the ratchet. The exact count (`cargo public-api` lines, derives and trait impls
//! included) is printed by `cargo xtask doc` and is not enforced: two pull requests that each
//! pass alone would fail together on the merged count, while rows merge as a set.
//!
//! **Where it reads.** The files `check-building` reads (comments and literals blanked;
//! no `tests`, `benches` or `examples` directory, no `tests.rs`), less `src/main.rs`,
//! `src/bin/`, every file a `#[cfg(test)] mod name;` declares (and the files under it) and
//! every item behind `#[cfg(test)]`.
//!
//! **What it lists.** `pub fn`, `struct`, `enum`, `trait`, `type`, `const`, `static`,
//! `mod`, `union` (a `pub(..)` item is not public), and a `#[macro_export] macro_rules!`.
//! A field and a variant are not listed; a method of a trait impl carries no `pub`.
//! **Blind spots.** An item a macro writes (or inside a `macro_rules!` body) is not seen; a
//! `pub` inside a fn body is read as an item of the enclosing module or `impl`;
//! `src/main.rs` and `src/bin/` are not listed, so a binary's own items are not rows.
//!
//! **Keys.** `<kind> <path>`: the path is the file's module path in the crate, the
//! names of the inline `mod` blocks around the item, the `impl` target's last path
//! segment without generics for an associated item, and the item's name:
//! `fn edit::Editor::replace`, `struct stream::OutputSpec`, `trait node::Node`. The same
//! key can occur twice (a `cfg` twin per platform, two `impl` blocks): the list is a
//! multiset, and each occurrence has its row.
//!
//! **Lists.** `<crate>.txt` holds `#` lines and one key per line, sorted;
//! `cargo xtask write-public-items` rewrites them from the tree. A crate with no file has
//! no rows.

mod scan;
#[cfg(test)]
mod tests;

use super::{Report, Tree, ratchet};
use scan::{Item, items};
use std::collections::{BTreeMap, BTreeSet};

/// The directory of the per-crate lists.
pub(crate) const DIR: &str = "xtask/public-items";

/// The fewest items a real run finds; fewer means the walk is broken.
pub(super) const MIN_ITEMS: usize = 1000;

const KINDS: [&str; 10] = [
    "fn", "struct", "enum", "trait", "type", "const", "static", "mod", "union", "macro",
];

fn path_of(krate: &str) -> String {
    format!("{DIR}/{krate}.txt")
}

fn header(krate: &str) -> String {
    format!(
        "# The public items of {krate}: one row per `pub` item its source declares, `<kind> <path>`,\n\
         # sorted (`cargo xtask write-public-items` rewrites this file from the tree).\n\
         # A new public item needs its row in the change that adds it, argued in the pull request;\n\
         # a row with no item is deleted, so the list only shrinks on its own.\n"
    )
}

/// The list file of each crate with its keys, one per item.
pub(super) fn lists(tree: &dyn Tree) -> BTreeMap<String, String> {
    items(tree)
        .into_iter()
        .map(|(krate, items)| {
            let mut text = header(&krate);
            for item in items {
                text.push_str(&item.key);
                text.push('\n');
            }
            (path_of(&krate), text)
        })
        .collect()
}

/// The rows of a list: each key with the lines it is on, or the lines that are no key.
fn rows(krate: &str, text: &str) -> Result<BTreeMap<String, Vec<usize>>, Vec<String>> {
    let mut map: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let path = path_of(krate);
    ratchet::parse_rows(text, |at, line, errors| {
        let kind = line.split(' ').next().unwrap_or("");
        if !KINDS.contains(&kind) || line.len() == kind.len() {
            errors.push(format!("  {path}:{at}: not `<kind> <path>`: {line}"));
            return None;
        }
        map.entry(line.to_owned()).or_default().push(at);
        Some(())
    })?;
    Ok(map)
}

pub(super) fn check(tree: &dyn Tree, min_items: usize) -> Report {
    const GATE: &str = "check-public-items";
    let mut r = Report::default();
    let found = items(tree);
    let total: usize = found.values().map(Vec::len).sum();
    if total < min_items {
        r.fail(format!(
            "{GATE}: FAIL: {total} public items found, at least {min_items} expected; the walk is broken"
        ));
        return r;
    }
    let mut crates: BTreeSet<&str> = found.keys().map(String::as_str).collect();
    let files = tree.files(DIR, &|_| false);
    crates.extend(
        files
            .iter()
            .filter_map(|f| f.strip_prefix(DIR)?.strip_prefix('/')?.strip_suffix(".txt")),
    );
    let none: Vec<Item> = Vec::new();
    for krate in crates {
        let path = path_of(krate);
        let have = found.get(krate).unwrap_or(&none);
        let text = tree.read(&path).unwrap_or_default();
        let listed = match rows(krate, &text) {
            Ok(rows) => rows,
            Err(errors) => {
                r.out.extend(ratchet::malformed(GATE, &path, errors).out);
                r.findings
                    .push(format!("{GATE}: FAIL ({path} is malformed)"));
                continue;
            }
        };
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for item in have {
            let n = seen.entry(&item.key).or_default();
            *n += 1;
            if *n > listed.get(&item.key).map_or(0, Vec::len) {
                r.fail(format!(
                    "{GATE}: FAIL: {krate}: {} ({}:{}): a new public item needs its row in {path}, \
                     added in the same change, or make it pub(crate); say in the PR description \
                     what calls it from outside the crate (CONTRIBUTING.md, reviewer checklist, item 4)",
                    item.key, item.file, item.line
                ));
            }
        }
        for (key, lines) in &listed {
            let n = seen.get(key.as_str()).copied().unwrap_or(0);
            for at in lines.iter().skip(n) {
                r.fail(format!(
                    "{GATE}: FAIL: {krate}: {key} ({path}:{at}) has a row and no item: delete the row: the list only shrinks on its own"
                ));
            }
        }
    }
    if r.passed() {
        r.say(format!("{GATE}: OK ({total} public items)"));
    }
    r
}
