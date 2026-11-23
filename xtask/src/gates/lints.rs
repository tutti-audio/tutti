//! `check-lints`: the workspace's lint table reaches every member.
//!
//! - **Every workspace member's `Cargo.toml` takes the workspace lints**, as the
//!   `[lints]` table with `workspace = true`, the dotted key `lints.workspace = true` or
//!   the inline `lints = { workspace = true }`. The members come from
//!   the root manifest's `members`.
//! - **The root `[workspace.lints.rust]` sets `missing_debug_implementations` to
//!   `deny`**: a public type without `Debug` fails the build on its own, not
//!   only under `-D warnings`.

use super::toml::{Toml, inline_entries};
use super::{Report, Tree};

const LINT: &str = "missing_debug_implementations";

/// Fewer members than this means the root manifest was not read.
const MIN_MEMBERS: usize = 20;

/// Whether the manifest sets `workspace = true` under `lints`, in any of the three
/// forms Cargo reads. A dotted or inline `lints` key belongs to the root table, so it
/// counts only before the first `[header]`.
fn takes_workspace_lints(manifest: &Toml) -> bool {
    let inline = |v: &str| {
        inline_entries(v)
            .iter()
            .any(|(k, v)| k == "workspace" && v == "true")
    };
    manifest.value("lints", "workspace") == Some("true")
        || manifest.value("", "lints.workspace") == Some("true")
        || manifest.value("", "lints").is_some_and(inline)
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let root = tree.read("Cargo.toml");
    let root = root.as_deref().map(Toml::parse);
    let members: Vec<String> = root
        .as_ref()
        .map(Toml::workspace_members)
        .unwrap_or_default();
    if members.len() < MIN_MEMBERS {
        bad.push(format!(
            "only {} workspace members found in Cargo.toml; the scan is broken",
            members.len()
        ));
    }

    for dir in &members {
        let path = format!("{dir}/Cargo.toml");
        let Some(text) = tree.read(&path) else {
            bad.push(format!("{dir}: a workspace member with no Cargo.toml"));
            continue;
        };
        if !takes_workspace_lints(&Toml::parse(&text)) {
            bad.push(format!(
                "{path}: missing `[lints] workspace = true` (or `lints.workspace = true`, or `lints = {{ workspace = true }}`)"
            ));
        }
    }
    match root {
        None => bad.push("Cargo.toml: missing".to_owned()),
        Some(root) => {
            let level = root
                .value("workspace.lints.rust", LINT)
                .map(|v| v.trim().trim_matches('"'));
            if level != Some("deny") {
                bad.push(format!(
                    "Cargo.toml: [workspace.lints.rust] must set {LINT} = \"deny\" (found {})",
                    level.unwrap_or("nothing")
                ));
            }
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!("check-lints: ok ({} members)", members.len()));
    } else {
        r.fail("check-lints: FAIL");
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

    const LINTS: &str = "[workspace.lints.rust]\nmissing_docs = \"warn\"\nmissing_debug_implementations = \"deny\"\n";

    fn root_with(members: usize) -> String {
        let list: String = (0..members)
            .map(|i| format!("  \"crates/c{i}\",\n"))
            .collect();
        format!("[workspace]\nmembers = [\n{list}]\n\n{LINTS}")
    }
    const LINTED: &str = "[package]\nname = \"x\"\n\n[lints]\nworkspace = true\n";

    fn clean() -> Mem {
        let mut m = Mem::default();
        m.files.insert("Cargo.toml".to_owned(), root_with(20));
        for i in 0..20 {
            m.files
                .insert(format!("crates/c{i}/Cargo.toml"), LINTED.to_owned());
        }
        m
    }

    fn with(mut m: Mem, path: &str, text: &str) -> Report {
        m.files.insert(path.to_owned(), text.to_owned());
        check(&m)
    }

    #[test]
    fn a_clean_tree_passes() {
        let r = check(&clean());
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[0].starts_with("check-lints: ok (20 members"));
    }

    #[test]
    fn a_crate_without_the_workspace_lints_fails() {
        let r = with(clean(), "crates/c3/Cargo.toml", "[package]\nname = \"x\"\n");
        assert_eq!(r.findings[0], "check-lints: FAIL");
        assert!(r.findings[1].contains("crates/c3/Cargo.toml: missing `[lints] workspace = true`"));
    }

    #[test]
    fn a_lints_table_of_its_own_is_not_the_workspace_table() {
        let r = with(
            clean(),
            "crates/c3/Cargo.toml",
            "[lints.rust]\nmissing_docs = \"warn\"\n",
        );
        assert_eq!(r.findings.len(), 2);
    }

    #[test]
    fn workspace_false_is_not_the_workspace_table() {
        let r = with(
            clean(),
            "crates/c3/Cargo.toml",
            "[package]\nname = \"x\"\n\n[lints]\nworkspace = false\n",
        );
        assert!(r.findings[1].contains("crates/c3/Cargo.toml: missing"));
    }

    #[test]
    fn the_dotted_and_inline_forms_take_the_workspace_lints() {
        for text in [
            "lints.workspace = true\n\n[package]\nname = \"x\"\n",
            "lints = { workspace = true }\n\n[package]\nname = \"x\"\n",
        ] {
            let r = with(clean(), "crates/c3/Cargo.toml", text);
            assert!(r.passed(), "{text}: {:?}", r.findings);
        }
    }

    #[test]
    fn the_dotted_and_inline_forms_with_false_fail() {
        for text in [
            "lints.workspace = false\n\n[package]\nname = \"x\"\n",
            "lints = { workspace = false }\n\n[package]\nname = \"x\"\n",
        ] {
            let r = with(clean(), "crates/c3/Cargo.toml", text);
            assert!(
                r.findings[1].contains("crates/c3/Cargo.toml: missing"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_workspace_table_that_only_warns_fails() {
        let r = with(
            clean(),
            "Cargo.toml",
            &root_with(20).replace("= \"deny\"", "= \"warn\""),
        );
        assert!(r.findings[1].contains("must set missing_debug_implementations"));
        let r = with(
            clean(),
            "Cargo.toml",
            &root_with(20).replace("missing_debug_implementations = \"deny\"\n", ""),
        );
        assert!(r.findings[1].contains("found nothing"));
    }

    #[test]
    fn too_few_members_fails() {
        let mut m = clean();
        m.files.insert("Cargo.toml".to_owned(), root_with(3));
        assert!(check(&m).findings[1].contains("the scan is broken"));
    }

    #[test]
    fn a_member_without_a_manifest_fails() {
        let mut m = clean();
        m.files.remove("crates/c4/Cargo.toml");
        assert!(check(&m).findings[1].contains("crates/c4: a workspace member with no Cargo.toml"));
    }
}
