//! The umbrella's name collisions (stable-API rule R6: one noun per concept), a part
//! of `check-umbrella`.
//!
//! Every `pub use` of `crates/tutti/src` is read as a use tree ([`super::use_tree`]); a
//! whole-crate or glob leaf names no item and is skipped. An export is an item (its source
//! path, crate first) under the name it takes (the alias when there is an `as`) at an
//! umbrella path (the file's module, the inline modules, the name). Two collisions:
//!
//! - `crates`: one name exported for items of more than one crate, counted across umbrella
//!   modules (`tutti::midi::Stream` and `tutti::sampler::Stream`). The crate is the first
//!   segment of the item's source path.
//! - `paths`: one item exported at more than one umbrella path.
//!
//! **Lists.** [`BASELINE`] holds `<kind>  <name>  <crates or paths, comma separated>` rows,
//! sorted, one per known collision. [`ALLOW`] holds `<name>  <crates>  <spec citation>: <why>`
//! rows for a `crates` collision that is a spec's own name. A collision with no row fails,
//! and a row with no collision (or an allow row that also has a baseline row, or a baseline
//! row for an unsorted or repeated key) fails as stale, so the lists only shrink.

use super::umbrella::{SRC, file_module, skipped};
use super::use_tree::placed_uses;
use super::{Report, Tree};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const BASELINE: &str = "xtask/umbrella-collisions-baseline.txt";
pub(crate) const ALLOW: &str = "xtask/umbrella-collisions-allow.txt";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Crates,
    Paths,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Crates => "crates",
            Kind::Paths => "paths",
        }
    }
}

/// `(kind, name, list)`: a collision, or a row of the baseline.
type Row = (Kind, String, String);

fn show(r: &Row) -> String {
    format!("{}  {}  {}", r.0.name(), r.1, r.2)
}

/// The collisions of the umbrella's source.
fn collisions(tree: &dyn Tree) -> Result<BTreeSet<Row>, String> {
    let files: Vec<String> = tree
        .files(SRC, &skipped)
        .into_iter()
        .filter(|p| p.ends_with(".rs"))
        .collect();
    if files.is_empty() {
        return Err(format!("{SRC} holds no .rs file; was the crate moved?"));
    }
    // Exported name -> (item -> umbrella paths of it); and item's crate.
    let mut by_name: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    let mut by_item: BTreeMap<String, (String, BTreeSet<String>)> = BTreeMap::new();
    for path in &files {
        let src = tree.read(path).unwrap_or_default();
        for u in placed_uses(&src) {
            if u.leaf.whole || u.leaf.name == "_" {
                continue;
            }
            let mut at = vec!["tutti".to_owned()];
            at.extend(file_module(path));
            at.extend(u.module);
            at.push(u.leaf.name.clone());
            let at = at.join("::");
            let item = u.leaf.item();
            by_name
                .entry(u.leaf.name.clone())
                .or_default()
                .entry(item.clone())
                .or_default()
                .insert(at.clone());
            by_item
                .entry(item)
                .or_insert_with(|| (u.leaf.source.clone(), BTreeSet::new()))
                .1
                .insert(at);
        }
    }
    let mut out = BTreeSet::new();
    for (name, items) in &by_name {
        let crates: BTreeSet<&str> = items
            .keys()
            .map(|i| i.split("::").next().unwrap_or(""))
            .collect();
        if crates.len() > 1 {
            out.insert((
                Kind::Crates,
                name.clone(),
                crates.into_iter().collect::<Vec<_>>().join(","),
            ));
        }
    }
    for (source, paths) in by_item.values() {
        if paths.len() > 1 {
            out.insert((
                Kind::Paths,
                source.clone(),
                paths.iter().cloned().collect::<Vec<_>>().join(","),
            ));
        }
    }
    Ok(out)
}

fn rows(text: &str, file: &str, errors: &mut Vec<String>) -> Vec<(usize, Vec<String>)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        if cols.len() < 3 {
            errors.push(format!("{file}:{}: too few columns: {line}", n + 1));
        } else {
            out.push((n + 1, cols));
        }
    }
    out
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let mut r = Report::default();
    let found = match collisions(tree) {
        Ok(f) => f,
        Err(e) => {
            r.fail(format!("umbrella-collisions: FAIL: {e}"));
            return r;
        }
    };
    let mut errors = Vec::new();
    let mut baseline: Vec<Row> = Vec::new();
    let mut allow: Vec<(Row, String)> = Vec::new();
    match tree.read(BASELINE) {
        None => errors.push(format!("{BASELINE} is missing")),
        Some(text) => {
            for (n, c) in rows(&text, BASELINE, &mut errors) {
                let kind = match c[0].as_str() {
                    "crates" => Kind::Crates,
                    "paths" => Kind::Paths,
                    _ => {
                        errors.push(format!("{BASELINE}:{n}: kind is `crates` or `paths`"));
                        continue;
                    }
                };
                baseline.push((kind, c[1].clone(), c[2].clone()));
            }
        }
    }
    match tree.read(ALLOW) {
        None => errors.push(format!("{ALLOW} is missing")),
        Some(text) => {
            for (n, c) in rows(&text, ALLOW, &mut errors) {
                if c.len() > 3 {
                    allow.push(((Kind::Crates, c[0].clone(), c[1].clone()), c[2..].join(" ")));
                } else {
                    errors.push(format!("{ALLOW}:{n}: `<name> <crates> <citation>: <why>`"));
                }
            }
        }
    }
    if !errors.is_empty() {
        r.fail("umbrella-collisions: FAIL (lists):");
        errors.into_iter().for_each(|e| r.fail(format!("  {e}")));
        return r;
    }
    let keys: BTreeSet<&Row> = baseline.iter().collect();
    let mut unordered = Vec::new();
    if baseline.windows(2).any(|w| w[0] >= w[1]) {
        unordered.push(format!(
            "{BASELINE} is not sorted, or repeats a row; sort it by kind, then name"
        ));
    }
    let allowed: BTreeSet<&Row> = allow.iter().map(|(k, _)| k).collect();
    let new: Vec<&Row> = found
        .iter()
        .filter(|f| !keys.contains(f) && !allowed.contains(f))
        .collect();
    let stale: Vec<&Row> = keys
        .iter()
        .copied()
        .filter(|k| !found.contains(k))
        .collect();
    let both: Vec<&&Row> = keys.iter().filter(|k| allowed.contains(**k)).collect();
    let idle: Vec<&Row> = allowed
        .iter()
        .copied()
        .filter(|a| !found.contains(a))
        .collect();
    if !new.is_empty() {
        r.fail(
            "umbrella-collisions: FAIL (one noun per concept: rename, or export one path; a spec's \
             own name is a row of the allow list):",
        );
        new.iter().for_each(|k| r.fail(format!("  {}", show(k))));
    }
    if !stale.is_empty() {
        r.fail(format!(
            "umbrella-collisions: FAIL (a row of {BASELINE} no longer collides; delete it):"
        ));
        stale.iter().for_each(|k| r.fail(format!("  {}", show(k))));
    }
    if !both.is_empty() {
        r.fail(format!(
            "umbrella-collisions: FAIL (a row of {BASELINE} is also allowed; delete it):"
        ));
        both.iter().for_each(|k| r.fail(format!("  {}", show(k))));
    }
    if !idle.is_empty() {
        r.fail(format!(
            "umbrella-collisions: FAIL (a row of {ALLOW} no longer collides; delete it):"
        ));
        idle.iter().for_each(|k| r.fail(format!("  {}", show(k))));
    }
    unordered
        .into_iter()
        .for_each(|u| r.fail(format!("umbrella-collisions: FAIL: {u}")));
    if r.passed() {
        r.say(format!(
            "umbrella-collisions: ok ({} collisions: {} baseline rows, {} allowed)",
            found.len(),
            baseline.len(),
            allow.len()
        ));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const LIB: &str = "crates/tutti/src/lib.rs";

    fn tree(lib: &str, baseline: &str, allow: &str) -> Mem {
        Mem::with(&[(LIB, lib), (BASELINE, baseline), (ALLOW, allow)])
    }

    const CROSS: &str = "pub mod midi { pub use tutti_ump::Stream; }\n\
                         pub mod sampler { pub use tutti_sampler::Stream; }\n";

    #[test]
    fn a_name_from_two_crates_is_a_collision_across_modules() {
        let r = check(&tree(CROSS, "", ""));
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert_eq!(r.findings[1], "  crates  Stream  tutti_sampler,tutti_ump");
    }

    #[test]
    fn one_item_at_two_paths_is_a_collision() {
        let src = "pub use tutti_a::Hz;\npub mod types { pub use tutti_a::Hz; }\n";
        let r = check(&tree(src, "", ""));
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert_eq!(r.findings[1], "  paths  Hz  tutti::Hz,tutti::types::Hz");
        // The same item twice is one crate, not a cross-crate collision.
        assert!(!r.findings[1].contains("crates"));
    }

    #[test]
    fn two_names_one_crate_and_distinct_names_pass() {
        let src = "pub use tutti_a::{Hz, Db};\npub mod m { pub use tutti_a::x::Q; }\n";
        assert!(check(&tree(src, "", "")).passed());
    }

    #[test]
    fn an_alias_resolves_the_exported_name() {
        let src = "pub use tutti_a::Error;\npub use tutti_b::Error as BError;\n";
        assert!(check(&tree(src, "", "")).passed());
        let src = "pub use tutti_a::Error;\npub use tutti_b::E as Error;\n";
        assert!(!check(&tree(src, "", "")).passed());
    }

    #[test]
    fn a_baseline_row_covers_its_collision() {
        let r = check(&tree(
            CROSS,
            "crates  Stream  tutti_sampler,tutti_ump\n",
            "",
        ));
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn an_allowed_name_passes() {
        let allow = "Stream  tutti_sampler,tutti_ump  M2-104: the UMP stream\n";
        let r = check(&tree(CROSS, "", allow));
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_stale_baseline_row_fails() {
        let r = check(&tree(
            "pub use tutti_a::Hz;\n",
            "crates  Stream  tutti_sampler,tutti_ump\n",
            "",
        ));
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(r.findings[0].contains("no longer collides"));
        assert!(r.findings[1].contains("Stream"));
    }

    #[test]
    fn a_stale_allow_row_fails() {
        let allow = "Stream  tutti_sampler,tutti_ump  M2-104: the UMP stream\n";
        let r = check(&tree("pub use tutti_a::Hz;\n", "", allow));
        assert!(r.findings[0].contains(ALLOW), "{:?}", r.findings);
    }

    #[test]
    fn a_new_collision_beside_a_baseline_row_fails() {
        let src = format!("{CROSS}pub use tutti_a::Key;\npub use tutti_b::Key;\n");
        let r = check(&tree(&src, "crates  Stream  tutti_sampler,tutti_ump\n", ""));
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert_eq!(r.findings[1], "  crates  Key  tutti_a,tutti_b");
    }

    #[test]
    fn a_collision_gaining_a_crate_is_new_and_its_row_stale() {
        let src = format!("{CROSS}pub mod x {{ pub use tutti_z::Stream; }}\n");
        let r = check(&tree(&src, "crates  Stream  tutti_sampler,tutti_ump\n", ""));
        assert_eq!(r.findings.len(), 4, "{:?}", r.findings);
    }

    #[test]
    fn the_baseline_only_goes_down() {
        // A row cannot be added for a collision that is not there, nor kept after it is
        // fixed: the list equals the collisions, so it never holds more than they do.
        let base = "crates  Key  tutti_a,tutti_b\ncrates  Stream  tutti_sampler,tutti_ump\n";
        let r = check(&tree(CROSS, base, ""));
        assert_eq!(r.findings.len(), 2, "{:?}", r.findings);
        assert!(r.findings[1].contains("Key"));
        let r = check(&tree("pub use tutti_a::Hz;\n", base, ""));
        assert_eq!(r.findings.len(), 3, "{:?}", r.findings);
    }

    #[test]
    fn a_row_both_baselined_and_allowed_fails() {
        let row = "Stream  tutti_sampler,tutti_ump  M2-104: UMP\n";
        let r = check(&tree(
            CROSS,
            "crates  Stream  tutti_sampler,tutti_ump\n",
            row,
        ));
        assert!(r.findings[0].contains("also allowed"), "{:?}", r.findings);
    }

    #[test]
    fn an_unsorted_baseline_fails() {
        let src = "pub use tutti_a::Key;\npub use tutti_b::Key;\npub use tutti_a::Hz;\npub use tutti_b::Hz;\n";
        let sorted = "crates  Hz  tutti_a,tutti_b\ncrates  Key  tutti_a,tutti_b\n";
        assert!(check(&tree(src, sorted, "")).passed());
        let rev = "crates  Key  tutti_a,tutti_b\ncrates  Hz  tutti_a,tutti_b\n";
        assert!(check(&tree(src, rev, "")).findings[0].contains("not sorted"));
    }

    #[test]
    fn a_missing_list_or_malformed_row_fails() {
        let mut m = Mem::with(&[(LIB, "pub use tutti_a::Hz;\n")]);
        assert!(check(&m).findings[1].contains("is missing"));
        m.files.insert(BASELINE.into(), "oops  X\n".into());
        m.files.insert(ALLOW.into(), String::new());
        assert!(check(&m).findings[1].contains("too few columns"));
    }
}
