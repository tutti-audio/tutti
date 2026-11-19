//! `affected <base>`: the packages a diff can reach, as `-p name` flags, or `all`.
//!
//! Not a gate: a pure function from the changed paths and the workspace's
//! manifests (read through [`Tree`], members and every dependency kind as
//! `check-layout` reads them) to the CI test selection. A changed path belongs to the member
//! whose directory holds it; the selection is those members and every member
//! that reaches one through a normal, build or dev dependency, or through a row
//! of [`SPAWNS`].
//!
//! A diff that touches one of [`ALL`]'s paths, or a path outside every member
//! that [`IGNORED`] does not name, selects the whole workspace.

use super::Tree;
use super::layout::packages;
use std::collections::{BTreeMap, BTreeSet};

/// The paths whose change selects the whole workspace, each with why. A
/// pattern ending in `/` is a directory, one ending in `*` a name prefix, any
/// other an exact path.
const ALL: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "the workspace's members, dependencies and lints",
    ),
    ("Cargo.lock", "every resolved version"),
    (
        "rust-toolchain.toml",
        "the compiler every crate builds with",
    ),
    (".config/", "nextest's profiles"),
    (".cargo/", "cargo's flags and aliases"),
    (".github/", "the workflows and actions that run the tests"),
    ("vendor/", "the VST3 SDK the plugin crates build against"),
    ("xtask/", "the tool that selects and gates"),
    (
        "testing/tutti-fixtures/",
        "the helpers every suite finds its files through",
    ),
    (
        "testing/tutti-shared-plugin*",
        "the plugin every format's suites load",
    ),
    (
        "testing/tutti-plugin-contract/",
        "the cases run against every format",
    ),
];

/// The paths a diff may touch without selecting anything: prose.
const IGNORED: &[&str] = &["docs/"];

/// `(changed, spawners)`: a suite of each spawner starts the binary of a
/// changed package instead of depending on it, an edge a manifest does not
/// hold (`tutti_fixtures::plugin_server` and `plugin_probe` find the binary).
/// A spawner is selected when any of `changed` is, and so is whatever reaches it.
const SPAWNS: &[(&[&str], &[&str])] = &[(
    &[
        "tutti-plugin-server",
        "tutti-plugin-probe",
        "tutti-plugin-ipc",
    ],
    &[
        "tutti-plugin-contract",
        "tutti-plugin-db",
        "tutti-plugin-ipc",
        "tutti-plugin",
        "tutti-session",
        "tutti",
    ],
)];

fn matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        path.starts_with(prefix)
    } else if pattern.ends_with('/') {
        path.starts_with(pattern)
    } else {
        path == pattern
    }
}

fn ignored(path: &str) -> bool {
    IGNORED.iter().any(|p| path.starts_with(p)) || (!path.contains('/') && path.ends_with(".md"))
}

/// `all`, or the affected package names as `-p name` flags on one line,
/// sorted (empty when nothing is affected).
pub(super) fn select(changed: &[String], tree: &dyn Tree) -> Result<String, String> {
    let changed: Vec<&String> = changed.iter().filter(|p| !ignored(p)).collect();
    if changed
        .iter()
        .any(|p| ALL.iter().any(|(pat, _)| matches(pat, p)))
    {
        return Ok("all".to_owned());
    }
    let mut bad = Vec::new();
    let pkgs = packages(tree, &mut bad);
    if !bad.is_empty() {
        return Err(bad.join("\n"));
    }
    let mut seeds: BTreeSet<&str> = BTreeSet::new();
    for path in changed {
        let owner = pkgs.iter().find(|p| {
            path.strip_prefix(p.dir.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        });
        match owner {
            Some(p) => {
                seeds.insert(&p.name);
            }
            None => return Ok("all".to_owned()),
        }
    }
    // package -> the packages that depend on it.
    let mut users: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let names: BTreeSet<&str> = pkgs.iter().map(|p| p.name.as_str()).collect();
    for p in &pkgs {
        for (dep, _) in &p.deps {
            if names.contains(dep.as_str()) {
                users.entry(dep).or_default().insert(&p.name);
            }
        }
    }
    for (sources, spawners) in SPAWNS {
        for s in *sources {
            users.entry(s).or_default().extend(
                spawners
                    .iter()
                    .copied()
                    .filter(|sp| sp != s && names.contains(sp)),
            );
        }
    }
    let mut out = seeds.clone();
    let mut todo: Vec<&str> = seeds.into_iter().collect();
    while let Some(p) = todo.pop() {
        for u in users.get(p).into_iter().flatten() {
            if out.insert(u) {
                todo.push(u);
            }
        }
    }
    Ok(out
        .iter()
        .map(|p| format!("-p {p}"))
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    /// A workspace of `(dir, name, manifest body)` members, with the root's list.
    fn ws(members: &[(&str, &str, &str)]) -> Mem {
        let list: Vec<String> = members
            .iter()
            .map(|(d, ..)| format!("    \"{d}\","))
            .collect();
        let mut m = Mem::with(&[(
            "Cargo.toml",
            &format!("[workspace]\nmembers = [\n{}\n]\n", list.join("\n")),
        )]);
        for (dir, name, body) in members {
            m.files.insert(
                format!("{dir}/Cargo.toml"),
                format!("[package]\nname = \"{name}\"\n{body}"),
            );
        }
        m
    }

    fn sample() -> Mem {
        ws(&[
            ("xtask", "xtask", ""),
            ("crates/tutti-units", "tutti-units", ""),
            ("crates/tutti-leaf", "tutti-leaf", ""),
            (
                "crates/tutti-rt",
                "tutti-rt",
                "[dependencies]\ntutti-units.path = \"../tutti-units\"\n",
            ),
            (
                "crates/tutti-graph",
                "tutti-graph",
                "[dependencies]\ntutti-rt.path = \"../tutti-rt\"\n",
            ),
            (
                "crates/tutti-probe-user",
                "tutti-probe-user",
                "[dev-dependencies]\ntutti-leaf.path = \"../tutti-leaf\"\n",
            ),
            ("crates/tutti-plugin-server", "tutti-plugin-server", ""),
            ("crates/tutti-plugin", "tutti-plugin", ""),
            (
                "crates/tutti",
                "tutti",
                "[dependencies]\ntutti-graph.path = \"../tutti-graph\"\ntutti-leaf.path = \"../tutti-leaf\"\n",
            ),
        ])
    }

    fn pick(paths: &[&str]) -> String {
        let paths: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
        select(&paths, &sample()).unwrap()
    }

    #[test]
    fn a_leaf_change_selects_only_itself_and_its_users() {
        assert_eq!(
            pick(&["crates/tutti-leaf/src/lib.rs"]),
            "-p tutti -p tutti-leaf -p tutti-probe-user"
        );
        assert_eq!(pick(&["crates/tutti/src/lib.rs"]), "-p tutti");
    }

    #[test]
    fn a_dev_dependency_edge_is_followed() {
        assert!(pick(&["crates/tutti-leaf/src/lib.rs"]).contains("-p tutti-probe-user"));
    }

    #[test]
    fn transitive_reverse_dependencies_are_followed() {
        assert_eq!(
            pick(&["crates/tutti-units/src/lib.rs"]),
            "-p tutti -p tutti-graph -p tutti-rt -p tutti-units"
        );
    }

    #[test]
    fn each_trigger_class_selects_the_workspace() {
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".config/nextest.toml",
            ".cargo/config.toml",
            ".github/workflows/ci.yml",
            "vendor/vst3-sdk/x.h",
            "xtask/src/main.rs",
            "testing/tutti-fixtures/src/lib.rs",
            "testing/tutti-shared-plugin-cpp/a.cpp",
            "testing/tutti-plugin-contract/tests/server.rs",
            "scripts/outside-every-member.sh",
        ] {
            assert_eq!(pick(&[path]), "all", "{path}");
        }
    }

    #[test]
    fn a_docs_only_diff_selects_nothing() {
        assert_eq!(
            pick(&["docs/engineering-notes.md", "README.md", "docs/x/y.png"]),
            ""
        );
        assert_eq!(pick(&[]), "");
    }

    #[test]
    fn a_nested_markdown_file_outside_docs_is_not_ignored() {
        assert_eq!(pick(&["notes/a.md"]), "all");
    }

    #[test]
    fn a_spawned_binary_selects_the_crates_whose_tests_spawn_it() {
        assert_eq!(
            pick(&["crates/tutti-plugin-server/src/main.rs"]),
            "-p tutti -p tutti-plugin -p tutti-plugin-server"
        );
    }

    #[test]
    fn a_directory_named_like_a_member_prefix_is_not_that_member() {
        assert_eq!(pick(&["crates/tutti-rt-extra/src/lib.rs"]), "all");
    }
}
